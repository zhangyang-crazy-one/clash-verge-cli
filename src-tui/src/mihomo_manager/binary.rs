//! Resolve and auto-install the mihomo core binary.

use std::io::Read as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::Context;
use sha2::Digest as _;
use tokio::process::Command;
use tokio::sync::{Mutex, OnceCell};

use super::CoreKind;

/// A candidate whose executable, integrity (for managed files), and actual
/// runtime version were validated. Preparation never changes core selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedCore {
    pub kind: CoreKind,
    pub path: PathBuf,
    pub source: String,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoreUpdateRequest {
    pub kind: CoreKind,
    pub observed: Option<String>,
    pub source: String,
    pub required: String,
    pub destination: PathBuf,
    pub diagnostic: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoreInspection {
    Ready(PreparedCore),
    NeedsUpdate(CoreUpdateRequest),
    Review(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoreProgressPhase {
    Checking,
    Downloading,
    Verifying,
    Ready,
}

#[derive(Debug, Clone)]
pub struct CoreProgress {
    pub kind: CoreKind,
    pub operation_id: u64,
    pub target: String,
    pub phase: CoreProgressPhase,
    pub version: Option<String>,
}

/// Construct only when a user confirms this exact operation/target. The
/// noninteractive resolver retains its explicitly requested install behavior.
#[derive(Debug, Clone, Copy)]
pub struct DownloadAuthorization {
    kind: CoreKind,
    operation_id: u64,
}

impl DownloadAuthorization {
    pub fn confirmed(kind: CoreKind, operation_id: u64) -> Self {
        Self { kind, operation_id }
    }
}

pub fn target_version(kind: CoreKind) -> &'static str {
    match kind {
        CoreKind::Mihomo => MIHOMO_FALLBACK_VERSION,
        CoreKind::SingBox => super::singbox_binary::SINGBOX_FALLBACK_VERSION,
    }
}

fn managed_path(kind: CoreKind) -> PathBuf {
    match kind {
        CoreKind::Mihomo => mihomo_binary_path(),
        CoreKind::SingBox => super::singbox_binary::singbox_binary_path(),
    }
}

fn versioned_path(path: &Path, kind: CoreKind) -> PathBuf {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    path.with_file_name(format!("{name}-{}", target_version(kind)))
}

fn managed_candidates(managed: &Path, kind: CoreKind) -> Vec<PathBuf> {
    let qualified = versioned_path(managed, kind);
    let mut candidates = vec![qualified.clone()];
    // Failed/old candidates remain immutable. Successful replacements use a
    // unique sibling if the fixed-version path was already present.
    if let (Some(parent), Some(name)) = (qualified.parent(), qualified.file_name()) {
        let prefix = format!("{}.candidate-", name.to_string_lossy());
        if let Ok(entries) = std::fs::read_dir(parent) {
            let mut siblings: Vec<_> = entries
                .filter_map(Result::ok)
                .filter_map(|entry| {
                    let name = entry.file_name();
                    let text = name.to_string_lossy();
                    let suffix = text.strip_prefix(&prefix)?;
                    uuid::Uuid::parse_str(suffix).ok()?;
                    Some(entry.path())
                })
                .collect();
            siblings.sort();
            candidates.extend(siblings);
        }
    }
    candidates.push(managed.to_path_buf());
    candidates
}

fn acquisition_destination(managed: &Path, kind: CoreKind) -> PathBuf {
    let qualified = versioned_path(managed, kind);
    if qualified.exists() {
        qualified.with_file_name(format!(
            "{}.candidate-{}",
            qualified.file_name().unwrap_or_default().to_string_lossy(),
            uuid::Uuid::new_v4()
        ))
    } else {
        qualified
    }
}

/// Runtime probes are deliberately injected at the filesystem seam. Tests
/// use captured output and temporary ELF fixtures, never a real executable.
async fn inspect_candidates<F, Fut>(kind: CoreKind, systems: Vec<PathBuf>, managed: PathBuf, probe: F) -> CoreInspection
where
    F: Fn(PathBuf) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<Option<String>>>,
{
    let target = target_version(kind);
    let mut ready = None;
    let mut observed = None;
    let mut source = "missing".to_string();
    let mut diagnostics = Vec::new();
    let mut reviews = Vec::new();
    let candidates = systems.into_iter().map(|path| (path, "system", false)).chain(
        managed_candidates(&managed, kind)
            .into_iter()
            .map(|path| (path, "cached", true)),
    );
    for (path, candidate_source, needs_receipt) in candidates {
        if !path.exists() {
            continue;
        }
        let result = async {
            let metadata = tokio::fs::metadata(&path).await?;
            if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
                anyhow::bail!("candidate is not an executable regular file");
            }
            if needs_receipt && !verify_cached_digest(&path).await? {
                anyhow::bail!("managed cache has no valid trusted-install receipt");
            }
            // Validate magic before probing an unexpected/corrupt payload.
            let mut file = std::fs::File::open(&path)?;
            let mut magic = [0_u8; 4];
            file.read_exact(&mut magic)?;
            ensure_elf(&magic)?;
            let version = probe(path.clone())
                .await?
                .context("version probe omitted a valid version")?;
            super::core_policy::Version::parse(&version)?;
            Ok::<_, anyhow::Error>(version)
        }
        .await;
        match result {
            Ok(version) => {
                if super::core_policy::is_newer_than(&version, target).unwrap_or(false) {
                    reviews.push(format!(
                        "{} at {} reports {version}; reviewed target is {target}; refusing automatic downgrade",
                        kind.as_str(),
                        path.display()
                    ));
                } else if super::core_policy::is_compatible(kind.as_str(), &version, target).unwrap_or(false) {
                    if ready.is_none() {
                        ready = Some(PreparedCore {
                            kind,
                            path,
                            source: candidate_source.into(),
                            version,
                        });
                    }
                } else {
                    diagnostics.push(format!(
                        "{} at {} reports unsupported {version}",
                        candidate_source,
                        path.display()
                    ));
                    if observed.is_none() {
                        observed = Some(version);
                        source = candidate_source.into();
                    }
                }
            }
            Err(error) => diagnostics.push(format!("{} at {}: {error:#}", candidate_source, path.display())),
        }
    }
    if !reviews.is_empty() {
        return CoreInspection::Review(reviews.join("; "));
    }
    if let Some(candidate) = ready {
        return CoreInspection::Ready(candidate);
    }
    CoreInspection::NeedsUpdate(CoreUpdateRequest {
        kind,
        observed,
        source,
        required: target.into(),
        destination: versioned_path(&managed, kind),
        diagnostic: if diagnostics.is_empty() {
            format!(
                "{} is missing; confirm download and verification of {target}",
                kind.as_str()
            )
        } else {
            diagnostics.join("; ")
        },
    })
}

/// Read-only and fully offline: no release lookup, download, permission
/// changes, install lock creation, or persisted selection.
pub async fn inspect(kind: CoreKind) -> CoreInspection {
    let systems = match kind {
        CoreKind::Mihomo => system_mihomo_candidates(),
        CoreKind::SingBox => super::singbox_binary::system_singbox_candidates(),
    };
    inspect_candidates(kind, systems, managed_path(kind), move |path| async move {
        match kind {
            CoreKind::Mihomo => read_mihomo_version(&path).await,
            CoreKind::SingBox => super::singbox_binary::read_singbox_version(&path).await,
        }
    })
    .await
}

pub(crate) fn report_progress(
    sender: Option<&tokio::sync::mpsc::Sender<CoreProgress>>,
    authorization: DownloadAuthorization,
    phase: CoreProgressPhase,
    version: Option<String>,
) {
    if let Some(sender) = sender {
        let _ = sender.try_send(CoreProgress {
            kind: authorization.kind,
            operation_id: authorization.operation_id,
            target: target_version(authorization.kind).into(),
            phase,
            version,
        });
    }
}

/// Confirmation-gated acquisition. New files are version qualified so an old
/// running executable remains available for rollback. Dropping this future
/// drops locks and unique TempPaths without publishing partial executables.
pub async fn acquire(
    kind: CoreKind,
    authorization: DownloadAuthorization,
    progress: Option<tokio::sync::mpsc::Sender<CoreProgress>>,
) -> anyhow::Result<PreparedCore> {
    if authorization.kind != kind {
        anyhow::bail!("download authorization belongs to a different core");
    }
    report_progress(progress.as_ref(), authorization, CoreProgressPhase::Checking, None);
    let dest = versioned_path(&managed_path(kind), kind);
    let _in_process = INSTALL_LOCK.lock().await;
    let _cross_process = lock_install(&dest).await?;
    match inspect(kind).await {
        CoreInspection::Ready(candidate) => {
            report_progress(
                progress.as_ref(),
                authorization,
                CoreProgressPhase::Ready,
                Some(candidate.version.clone()),
            );
            return Ok(candidate);
        }
        CoreInspection::Review(diagnostic) => anyhow::bail!("{diagnostic}"),
        CoreInspection::NeedsUpdate(_) => {}
    }
    let dest = acquisition_destination(&managed_path(kind), kind);
    report_progress(progress.as_ref(), authorization, CoreProgressPhase::Downloading, None);
    let version = match kind {
        CoreKind::Mihomo => {
            download_managed_mihomo(&dest, target_version(kind), Some((authorization, progress.as_ref()))).await?
        }
        CoreKind::SingBox => {
            super::singbox_binary::download_managed_singbox(
                &dest,
                target_version(kind),
                Some((authorization, progress.as_ref())),
            )
            .await?
        }
    };
    report_progress(
        progress.as_ref(),
        authorization,
        CoreProgressPhase::Ready,
        Some(version.clone()),
    );
    Ok(PreparedCore {
        kind,
        path: dest,
        source: "downloaded".into(),
        version,
    })
}

/// Managed (auto-downloaded) mihomo stable version — compile-time fallback
/// when GitHub API is unreachable.
pub const MIHOMO_FALLBACK_VERSION: &str = "v1.19.32";

const MIHOMO_REPO: &str = "MetaCubeX/mihomo";

/// Serialises the check-then-install sequence of `resolve_or_install`
/// within this process. An async mutex so it is held across the download
/// awaits; the cross-process half is the `flock` on [`install_lock_path`].
static INSTALL_LOCK: Mutex<()> = Mutex::const_new(());

static LATEST_VERSION: OnceCell<String> = OnceCell::const_new();

/// Resolve the latest mihomo version tag from GitHub, falling back to the
/// compile-time constant when the API is unreachable.
pub async fn latest_mihomo_version() -> &'static str {
    LATEST_VERSION
        .get_or_init(|| async {
            if let Some(tag) = crate::subscribe::client_meta::fetch_latest_release_tag(MIHOMO_REPO).await {
                tracing::info!(target: "mihomo", "latest mihomo release from GitHub: {tag}");
                return tag;
            }
            tracing::warn!(
                target: "mihomo",
                "GitHub API unreachable, falling back to {MIHOMO_FALLBACK_VERSION}"
            );
            MIHOMO_FALLBACK_VERSION.to_string()
        })
        .await
        .as_str()
}

/// D-01: managed binary path. Resolves to
/// `$XDG_DATA_HOME/clash-verge-cli/mihomo` with a fallback to
/// `~/.local/share/clash-verge-cli/mihomo` for systems without XDG.
pub fn mihomo_binary_path() -> PathBuf {
    managed_binary_path(
        std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).as_deref(),
        std::env::var_os("HOME").map(PathBuf::from).as_deref(),
        "mihomo",
    )
}

/// Pure XDG/HOME resolver so path policy can be tested without mutating the
/// process environment (which is shared by unrelated tests and callers).
pub(crate) fn managed_binary_path(xdg_data_home: Option<&Path>, home: Option<&Path>, name: &str) -> PathBuf {
    if let Some(data_dir) = xdg_data_home {
        return data_dir.join("clash-verge-cli").join(name);
    }
    home.unwrap_or_else(|| Path::new(""))
        .join(".local")
        .join("share")
        .join("clash-verge-cli")
        .join(name)
}

/// Best-effort system mihomo fallback. Checks standard XDG `bin` first,
/// then common system paths.  Skips paths that are not regular files or
/// are not executable so a stale `verge-mihomo` doesn't block the managed
/// download fallback.
pub fn system_mihomo() -> Option<PathBuf> {
    system_mihomo_candidates().into_iter().next()
}

fn system_mihomo_candidates() -> Vec<PathBuf> {
    let candidates = [
        dirs::executable_dir(),
        Some(PathBuf::from("/usr/bin")),
        Some(PathBuf::from("/usr/local/bin")),
    ];
    let mut seen = std::collections::HashSet::new();
    let mut found = Vec::new();
    for dir in candidates.into_iter().flatten() {
        let path = dir.join("verge-mihomo");
        if seen.insert(path.clone())
            && path.is_file()
            && std::fs::metadata(&path).is_ok_and(|metadata| metadata.permissions().mode() & 0o111 != 0)
        {
            found.push(path);
        }
    }
    found
}

/// Resolve the binary that WOULD be used without downloading anything:
/// the system `verge-mihomo` if present, else the managed binary if it
/// already exists. Used by read-only TUN capability preflights (TUI toggle
/// and capability state) that must not trigger a network install.
pub fn candidate_without_install() -> Option<PathBuf> {
    candidate_from_paths(system_mihomo(), &mihomo_binary_path(), CoreKind::Mihomo)
}

pub(crate) fn candidate_from_paths(system: Option<PathBuf>, managed: &Path, kind: CoreKind) -> Option<PathBuf> {
    system.or_else(|| {
        managed_candidates(managed, kind)
            .into_iter()
            .find(|path| path.is_file())
    })
}

/// Where the runnable mihomo binary came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MihomoBinarySource {
    /// System `verge-mihomo`.
    System,
    /// Already present managed binary at the target version.
    ManagedCached,
    /// Freshly downloaded into the managed data directory.
    Downloaded,
}

impl MihomoBinarySource {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::ManagedCached => "cached",
            Self::Downloaded => "downloaded",
        }
    }
}

/// Result of resolving (and possibly installing) mihomo.
#[derive(Debug, Clone)]
pub struct ResolvedMihomo {
    pub path: PathBuf,
    pub source: MihomoBinarySource,
    pub version: String,
}

/// Resolve a runnable mihomo binary, downloading the managed build when needed.
///
/// Preference order:
/// 1. System `verge-mihomo` (left untouched)
/// 2. Managed data-dir binary at the detected latest version (download/upgrade as needed)
pub async fn resolve_or_install() -> anyhow::Result<ResolvedMihomo> {
    let candidate = match inspect(CoreKind::Mihomo).await {
        CoreInspection::Ready(candidate) => candidate,
        CoreInspection::Review(diagnostic) => anyhow::bail!("{diagnostic}"),
        CoreInspection::NeedsUpdate(_) => {
            acquire(
                CoreKind::Mihomo,
                DownloadAuthorization::confirmed(CoreKind::Mihomo, 0),
                None,
            )
            .await?
        }
    };
    Ok(ResolvedMihomo {
        path: candidate.path,
        source: match candidate.source.as_str() {
            "system" => MihomoBinarySource::System,
            "cached" => MihomoBinarySource::ManagedCached,
            _ => MihomoBinarySource::Downloaded,
        },
        version: candidate.version,
    })
}

pub(crate) async fn verify_cached_digest(path: &Path) -> anyhow::Result<bool> {
    let bytes = tokio::fs::read(path).await?;
    let expected = sha256_hex(&bytes);
    let qualified_receipt = digest_receipt_path(path, &expected);
    match tokio::fs::read_to_string(&qualified_receipt).await {
        Ok(value) if value.trim() == format!("sha256:{expected}") => return Ok(true),
        Ok(_) => return Ok(false),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to read digest receipt {}", qualified_receipt.display()));
        }
    }

    // Legacy single-receipt format remains readable for caches installed by
    // earlier versions. New installs never replace this file.
    let legacy_receipt = path.with_extension("sha256");
    match tokio::fs::read_to_string(&legacy_receipt).await {
        Ok(value) => Ok(verify_sha256(&bytes, value.trim()).is_ok()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("failed to read digest receipt {}", legacy_receipt.display())),
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    sha2::Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn digest_receipt_path(binary: &Path, digest_hex: &str) -> PathBuf {
    binary.with_extension(format!("sha256.{digest_hex}"))
}

/// Publish an immutable digest-qualified receipt before atomically replacing
/// a managed binary. A failed/cancelled binary rename leaves the old binary's
/// receipt available and the candidate receipt is harmlessly reusable.
pub(crate) async fn write_digest_receipt(binary: &Path, bytes: &[u8], prefix: &str) -> anyhow::Result<()> {
    let digest = sha256_hex(bytes);
    let receipt_path = digest_receipt_path(binary, &digest);
    let parent = binary.parent().context("managed binary path has no parent")?;
    let receipt_tmp = tempfile::Builder::new()
        .prefix(prefix)
        .tempfile_in(parent)
        .context("failed to create digest receipt staging file")?;
    tokio::fs::write(receipt_tmp.path(), format!("sha256:{digest}"))
        .await
        .context("failed to write digest receipt")?;
    receipt_tmp
        .into_temp_path()
        .persist(&receipt_path)
        .with_context(|| format!("failed to install digest receipt to {}", receipt_path.display()))?;
    Ok(())
}

/// Lock file guarding the managed binary across processes.
fn install_lock_path(managed: &Path) -> PathBuf {
    managed.with_extension("lock")
}

/// Take an exclusive `flock` on the install lock file. The lock is released
/// when the returned file is dropped (or the process dies).
pub(crate) async fn lock_install(managed: &Path) -> anyhow::Result<std::fs::File> {
    let lock_path = install_lock_path(managed);
    if let Some(parent) = lock_path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .with_context(|| format!("failed to open {}", lock_path.display()))?;
    // An uninterruptible spawn_blocking flock would outlive cancellation
    // and could keep runtime shutdown waiting. Poll the nonblocking lock;
    // dropping this future closes the handle immediately.
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(std::fs::TryLockError::WouldBlock) => tokio::time::sleep(std::time::Duration::from_millis(50)).await,
            Err(std::fs::TryLockError::Error(error)) => {
                return Err(error).with_context(|| format!("failed to lock {}", lock_path.display()));
            }
        }
    }
}

/// Set the executable bit on the binary. Idempotent — if the bits are
/// already 0o755 we return success without touching the inode.
pub async fn ensure_executable(path: &Path) -> std::io::Result<()> {
    let target = std::fs::Permissions::from_mode(0o755);
    let current = tokio::fs::metadata(path).await?.permissions();
    // Mask file-type bits — mode() includes e.g., 0o100755 (regular file)
    if (current.mode() & 0o777) == target.mode() {
        return Ok(());
    }
    tokio::fs::set_permissions(path, target).await
}

/// Download, verify, and atomically install the managed mihomo build.
///
/// Callers must hold the install locks (see [`resolve_or_install`]).
/// Verification, in order:
/// 1. sha256 against GitHub's asset digest or exact official manifest;
/// 2. the decompressed payload is an ELF executable;
/// 3. the staged binary runs and reports exactly `version`.
///
/// Only then is it renamed over `dest`, so a failed or tampered download
/// never replaces a working binary.
async fn download_managed_mihomo(
    dest: &Path,
    version: &str,
    progress: Option<(DownloadAuthorization, Option<&tokio::sync::mpsc::Sender<CoreProgress>>)>,
) -> anyhow::Result<String> {
    let asset = linux_asset_name().context("unsupported CPU architecture for auto-install")?;
    let asset_file = format!("{asset}-{version}.gz");
    let url = format!("https://github.com/{MIHOMO_REPO}/releases/download/{version}/{asset_file}");

    tracing::info!(
        target: "mihomo",
        "downloading mihomo {version} → {}",
        dest.display()
    );

    let client = reqwest::Client::builder()
        .user_agent(format!("clash-verge-cli/{}", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .context("failed to build download client")?;

    let expected =
        crate::subscribe::client_meta::fetch_trusted_release_digest(MIHOMO_REPO, version, &asset_file).await?;
    let response = client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("failed to download mihomo from {url}"))?
        .error_for_status()
        .with_context(|| format!("mihomo download returned error for {url}"))?;

    use tokio_stream::StreamExt as _;
    let mut stream = response.bytes_stream();
    let mut compressed = Vec::new();
    let mut hasher = sha2::Sha256::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("failed to read mihomo download stream")?;
        if compressed.len().saturating_add(chunk.len()) > 256 * 1024 * 1024 {
            anyhow::bail!("mihomo archive exceeds download size limit");
        }
        hasher.update(&chunk);
        compressed.extend_from_slice(&chunk);
    }
    if let Some((authorization, sender)) = progress {
        report_progress(sender, authorization, CoreProgressPhase::Verifying, None);
    }
    let actual = format!(
        "sha256:{}",
        hasher
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    );
    verify_sha256_digest(&actual, &expected).with_context(|| format!("integrity check failed for {url}"))?;
    tracing::info!(target: "mihomo", "verified sha256 of {asset_file}");

    let decoder = flate2::read::GzDecoder::new(compressed.as_slice());
    let mut binary = Vec::new();
    decoder
        .take(512 * 1024 * 1024 + 1)
        .read_to_end(&mut binary)
        .context("failed to decompress mihomo gzip archive")?;
    install_verified_payload(CoreKind::Mihomo, dest, &binary, version, |path| async move {
        read_mihomo_version(&path).await
    })
    .await
}

/// Common atomic publication seam. The caller has verified the archive's
/// trusted digest; this checks ELF and the staged runtime version before
/// publishing a receipt and executable. Tests inject captured probe results.
pub(crate) async fn install_verified_payload<F, Fut>(
    kind: CoreKind,
    dest: &Path,
    payload: &[u8],
    expected: &str,
    probe: F,
) -> anyhow::Result<String>
where
    F: FnOnce(PathBuf) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<Option<String>>>,
{
    if payload.len() > 512 * 1024 * 1024 {
        anyhow::bail!("core executable exceeds size limit");
    }
    ensure_elf(payload)?;
    let parent = dest.parent().context("managed core path has no parent")?;
    tokio::fs::create_dir_all(parent).await?;
    let staged = tempfile::Builder::new()
        .prefix(".core-")
        .suffix(".download")
        .tempfile_in(parent)?;
    tokio::fs::write(staged.path(), payload).await?;
    // Closing the writer avoids ETXTBSY. TempPath owns cleanup on every
    // failure/cancellation before atomic publication.
    let staged = staged.into_temp_path();
    ensure_executable(&staged).await?;
    let reported = probe(staged.to_path_buf())
        .await?
        .context("staged core omitted a valid version")?;
    if !version_matches_target(&reported, expected)
        || !super::core_policy::is_compatible(kind.as_str(), &reported, target_version(kind))?
    {
        anyhow::bail!(
            "downloaded {} reports {reported}, expected reviewed {expected}; refusing to install",
            kind.as_str()
        );
    }
    write_digest_receipt(dest, payload, ".core-receipt-").await?;
    staged
        .persist(dest)
        .with_context(|| format!("failed to install core to {}", dest.display()))?;
    Ok(reported)
}

/// Compare `data` against a GitHub asset digest (`sha256:<hex>`).
pub(crate) fn verify_sha256(data: &[u8], expected: &str) -> anyhow::Result<()> {
    let actual = format!(
        "sha256:{}",
        sha2::Sha256::digest(data)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    );
    verify_sha256_digest(&actual, expected)
}

pub(crate) fn verify_sha256_digest(actual: &str, expected: &str) -> anyhow::Result<()> {
    let expected_hex = expected
        .strip_prefix("sha256:")
        .with_context(|| format!("unsupported digest format: {expected}"))?;
    let actual_hex = actual.strip_prefix("sha256:").unwrap_or(actual);
    if expected_hex.len() != 64 || !expected_hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        anyhow::bail!("invalid sha256 checksum text");
    }
    if !actual_hex.eq_ignore_ascii_case(expected_hex) {
        anyhow::bail!("sha256 mismatch: expected {expected_hex}, got {actual_hex}");
    }
    Ok(())
}

pub(crate) fn ensure_elf(binary: &[u8]) -> anyhow::Result<()> {
    if binary.starts_with(b"\x7fELF") {
        Ok(())
    } else {
        anyhow::bail!("payload is not an ELF binary ({} bytes)", binary.len())
    }
}

fn linux_asset_name() -> Option<&'static str> {
    match std::env::consts::ARCH {
        "x86_64" => Some("mihomo-linux-amd64-v2"),
        "aarch64" => Some("mihomo-linux-arm64"),
        "arm" => Some("mihomo-linux-armv7"),
        "riscv64" => Some("mihomo-linux-riscv64"),
        // MetaCubeX ships loongarch as abi1/abi2-specific assets; do not guess.
        "loongarch64" => None,
        _ => None,
    }
}

async fn read_mihomo_version(path: &Path) -> anyhow::Result<Option<String>> {
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        Command::new(path)
            .arg("-v")
            .kill_on_drop(true)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output(),
    )
    .await
    .context("mihomo version probe timed out")?
    .with_context(|| format!("failed to execute {}", path.display()))?;

    if !output.status.success() {
        anyhow::bail!(
            "mihomo version probe exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let text = String::from_utf8_lossy(&output.stdout);
    let err = String::from_utf8_lossy(&output.stderr);
    let combined = format!("{text}{err}");
    Ok(extract_version_token(&combined))
}

fn extract_version_token(text: &str) -> Option<String> {
    // Examples: "Mihomo Meta v1.19.29", "v1.19.29"
    let mut found = Vec::new();
    for token in text.split_whitespace() {
        let trimmed = token.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '.' && c != '-');
        let candidate = if trimmed.starts_with('v') {
            trimmed.to_string()
        } else if trimmed.chars().next().is_some_and(|c| c.is_ascii_digit()) {
            format!("v{trimmed}")
        } else {
            continue;
        };
        if super::core_policy::Version::parse(&candidate).is_ok() {
            found.push(candidate);
        }
    }
    if found.is_empty() || found.iter().any(|v| v != &found[0]) {
        None
    } else {
        found.into_iter().next()
    }
}

fn version_matches_target(version: &str, target: &str) -> bool {
    let normalized = if version.starts_with('v') {
        version.to_string()
    } else {
        format!("v{version}")
    };
    normalized == target
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
pub(crate) mod tests {
    use super::*;

    #[tokio::test]
    async fn guided_core_old_system_does_not_hide_verified_offline_cache() {
        let root = tempfile::tempdir().unwrap();
        let old = root.path().join("system");
        let managed = root.path().join("mihomo");
        for path in [&old, &managed] {
            tokio::fs::write(path, b"\x7fELFfixture").await.unwrap();
            ensure_executable(path).await.unwrap();
        }
        write_digest_receipt(&managed, b"\x7fELFfixture", ".receipt-")
            .await
            .unwrap();
        let old_probe = old.clone();
        let result = inspect_candidates(
            super::super::CoreKind::Mihomo,
            vec![old],
            managed.clone(),
            move |path| {
                let old = old_probe.clone();
                async move { Ok(Some(if path == old { "v1.19.29" } else { "v1.19.32" }.to_string())) }
            },
        )
        .await;
        assert!(
            matches!(result, CoreInspection::Ready(ref candidate) if candidate.path == managed && candidate.source == "cached")
        );
    }

    #[tokio::test]
    async fn guided_core_old_system_without_cache_requires_confirmation() {
        let root = tempfile::tempdir().unwrap();
        let old = root.path().join("system");
        tokio::fs::write(&old, b"\x7fELFfixture").await.unwrap();
        ensure_executable(&old).await.unwrap();
        let result = inspect_candidates(
            super::super::CoreKind::SingBox,
            vec![old],
            root.path().join("sing-box"),
            |_| async { Ok(Some("1.13.21".into())) },
        )
        .await;
        assert!(
            matches!(result, CoreInspection::NeedsUpdate(ref request) if request.observed.as_deref() == Some("1.13.21") && request.required == "v1.14.2")
        );
    }

    #[tokio::test]
    async fn guided_core_unknown_newer_requires_review_without_downgrade() {
        let root = tempfile::tempdir().unwrap();
        let system = root.path().join("system");
        tokio::fs::write(&system, b"\x7fELFfixture").await.unwrap();
        ensure_executable(&system).await.unwrap();
        let result = inspect_candidates(
            super::super::CoreKind::Mihomo,
            vec![system],
            root.path().join("mihomo"),
            |_| async { Ok(Some("v1.20.0".into())) },
        )
        .await;
        assert!(matches!(result, CoreInspection::Review(ref diagnostic) if diagnostic.contains("1.20.0")));
    }

    #[tokio::test]
    async fn guided_core_corrupt_cache_is_never_executed() {
        let root = tempfile::tempdir().unwrap();
        let managed = root.path().join("mihomo");
        tokio::fs::write(&managed, b"\x7fELFfixture").await.unwrap();
        ensure_executable(&managed).await.unwrap();
        let result = inspect_candidates(super::super::CoreKind::Mihomo, vec![], managed, |_| async {
            panic!("untrusted cache must not be executed");
            #[allow(unreachable_code)]
            Ok(None)
        })
        .await;
        assert!(matches!(result, CoreInspection::NeedsUpdate(_)));
    }

    #[tokio::test]
    async fn guided_core_probe_failure_is_visible_without_download() {
        let root = tempfile::tempdir().unwrap();
        let system = root.path().join("system");
        tokio::fs::write(&system, b"\x7fELFfixture").await.unwrap();
        ensure_executable(&system).await.unwrap();
        let result = inspect_candidates(CoreKind::Mihomo, vec![system], root.path().join("mihomo"), |_| async {
            anyhow::bail!("captured nonzero version probe")
        })
        .await;
        assert!(
            matches!(result, CoreInspection::NeedsUpdate(ref request) if request.diagnostic.contains("nonzero version probe"))
        );
    }

    #[tokio::test]
    async fn guided_core_wrong_authorization_never_discovers_or_downloads() {
        let result = acquire(
            CoreKind::SingBox,
            DownloadAuthorization::confirmed(CoreKind::Mihomo, 9),
            None,
        )
        .await;
        assert!(result.unwrap_err().to_string().contains("different core"));
    }

    #[tokio::test]
    async fn guided_core_reacquisition_preserves_existing_version_path_and_reuses_sibling_offline() {
        let root = tempfile::tempdir().unwrap();
        let managed = root.path().join("mihomo");
        let qualified = versioned_path(&managed, CoreKind::Mihomo);
        tokio::fs::write(&qualified, b"old running candidate").await.unwrap();
        let dest = acquisition_destination(&managed, CoreKind::Mihomo);
        assert_ne!(dest, qualified);
        install_verified_payload(CoreKind::Mihomo, &dest, b"\x7fELFfixture", "v1.19.32", |_| async {
            Ok(Some("v1.19.32".into()))
        })
        .await
        .unwrap();
        assert_eq!(tokio::fs::read(&qualified).await.unwrap(), b"old running candidate");
        let result = inspect_candidates(CoreKind::Mihomo, vec![], managed, |_| async {
            Ok(Some("v1.19.32".into()))
        })
        .await;
        assert!(matches!(result, CoreInspection::Ready(ref candidate) if candidate.path == dest));
    }

    #[tokio::test]
    async fn guided_core_progress_is_bounded_and_ready_uses_verified_version() {
        let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
        let auth = DownloadAuthorization::confirmed(CoreKind::Mihomo, 42);
        report_progress(Some(&sender), auth, CoreProgressPhase::Verifying, None);
        report_progress(Some(&sender), auth, CoreProgressPhase::Downloading, None);
        let verifying = receiver.recv().await.unwrap();
        assert_eq!(verifying.operation_id, 42);
        assert_eq!(verifying.version, None);
        report_progress(Some(&sender), auth, CoreProgressPhase::Ready, Some("v1.19.32".into()));
        let ready = receiver.recv().await.unwrap();
        assert_eq!(ready.version.as_deref(), Some("v1.19.32"));
        assert_eq!(ready.phase, CoreProgressPhase::Ready);
    }

    #[tokio::test]
    async fn guided_core_version_mismatch_and_non_elf_keep_old_candidate() {
        let root = tempfile::tempdir().unwrap();
        let dest = root.path().join("core");
        tokio::fs::write(&dest, b"old candidate").await.unwrap();
        for kind in [CoreKind::Mihomo, CoreKind::SingBox] {
            let mismatch = install_verified_payload(kind, &dest, b"\x7fELFfixture", target_version(kind), |_| async {
                Ok(Some("1.0.0".into()))
            })
            .await;
            assert!(mismatch.is_err());
            let non_elf = install_verified_payload(kind, &dest, b"not executable", target_version(kind), |_| async {
                panic!("non-ELF cannot be probed");
                #[allow(unreachable_code)]
                Ok(None)
            })
            .await;
            assert!(non_elf.is_err());
            assert_eq!(tokio::fs::read(&dest).await.unwrap(), b"old candidate");
            assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
        }
    }

    #[tokio::test]
    async fn guided_core_cancelled_staged_probe_cleans_up_and_preserves_old() {
        let root = tempfile::tempdir().unwrap();
        let dest = root.path().join("core");
        tokio::fs::write(&dest, b"old candidate").await.unwrap();
        let worker_dest = dest.clone();
        let (entered, wait) = tokio::sync::oneshot::channel();
        let worker = tokio::spawn(async move {
            install_verified_payload(
                CoreKind::Mihomo,
                &worker_dest,
                b"\x7fELFfixture",
                "v1.19.32",
                move |_| async move {
                    entered.send(()).unwrap();
                    std::future::pending::<anyhow::Result<Option<String>>>().await
                },
            )
            .await
        });
        wait.await.unwrap();
        worker.abort();
        assert!(worker.await.unwrap_err().is_cancelled());
        assert_eq!(tokio::fs::read(&dest).await.unwrap(), b"old candidate");
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
    }

    #[test]
    fn guided_core_wrong_archive_digest_never_qualifies_as_trusted() {
        assert!(verify_sha256(b"archive", &format!("sha256:{}", "a".repeat(64))).is_err());
        assert!(verify_sha256_digest("sha256:abc", "sha256:abc").is_err());
    }
    use std::sync::Mutex;

    pub(crate) static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn mihomo_managed_path_prefers_xdg_data_home_without_environment_mutation() {
        let path = managed_binary_path(
            Some(Path::new("/tmp/test-xdg")),
            Some(Path::new("/ignored-home")),
            "mihomo",
        );
        assert!(path.ends_with("clash-verge-cli/mihomo"), "got {path:?}");
        assert!(path.starts_with("/tmp/test-xdg"));
    }

    #[test]
    fn mihomo_managed_path_uses_home_fallback_without_environment_mutation() {
        let path = managed_binary_path(None, Some(Path::new("/tmp/fake-home")), "mihomo");
        assert!(
            path.starts_with("/tmp/fake-home/.local/share/clash-verge-cli/mihomo"),
            "got {path:?}"
        );
    }

    #[test]
    fn extracts_version_from_mihomo_output() {
        assert_eq!(
            extract_version_token("Mihomo Meta v1.19.29 linux amd64"),
            Some("v1.19.29".into())
        );
        assert_eq!(extract_version_token("v1.19.29"), Some("v1.19.29".into()));
        assert!(version_matches_target("v1.19.29", "v1.19.29"));
        assert!(!version_matches_target("v1.19.25", "v1.19.29"));
    }

    #[test]
    fn cached_binary_requires_a_matching_strict_digest_receipt() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fixture-core");
        let bytes = b"test-owned fixture bytes";
        std::fs::write(&path, bytes).unwrap();
        let receipt = format!(
            "sha256:{}",
            sha2::Sha256::digest(bytes)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        );
        std::fs::write(path.with_extension("sha256"), receipt).unwrap();

        tokio::runtime::Runtime::new().unwrap().block_on(async {
            assert!(verify_cached_digest(&path).await.unwrap());
            std::fs::write(&path, b"changed fixture bytes").unwrap();
            assert!(!verify_cached_digest(&path).await.unwrap());
            std::fs::write(path.with_extension("sha256"), "sha256:not-a-digest").unwrap();
            assert!(!verify_cached_digest(&path).await.unwrap());
        });
    }

    #[test]
    fn failed_or_cancelled_binary_replace_keeps_old_digest_receipt_valid() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fixture-core");
        let old_bytes = b"old cached fixture binary";
        let candidate_bytes = b"new staged fixture binary";
        std::fs::write(&path, old_bytes).unwrap();

        tokio::runtime::Runtime::new().unwrap().block_on(async {
            write_digest_receipt(&path, old_bytes, ".fixture-receipt-")
                .await
                .unwrap();
            assert!(verify_cached_digest(&path).await.unwrap());

            // Model cancellation or a failed final rename after the candidate
            // receipt is durable but before the binary replacement commits.
            write_digest_receipt(&path, candidate_bytes, ".fixture-receipt-")
                .await
                .unwrap();
            assert_eq!(tokio::fs::read(&path).await.unwrap(), old_bytes);
            assert!(verify_cached_digest(&path).await.unwrap());
        });
    }

    #[test]
    fn candidate_without_install_prefers_system_binary() {
        // A regular executable file in XDG bin is preferred over a managed
        // path; neither triggers a download.
        let dir = tempfile::tempdir().unwrap();
        let sys = dir.path().join("verge-mihomo");
        std::fs::write(&sys, b"\x7fELFfixture").unwrap();
        let candidate = candidate_from_paths(Some(sys.clone()), &dir.path().join("mihomo"), CoreKind::Mihomo);
        assert_eq!(candidate.as_deref(), Some(sys.as_path()));
    }

    #[test]
    fn candidate_without_install_is_a_no_download_probe() {
        // candidate_without_install must mirror the resolve preference
        // (system first, then existing managed) without downloading and
        // without mutating anything. Works on hosts with or without a
        // system verge-mihomo.
        let dir = tempfile::tempdir().unwrap();
        let managed = dir.path().join("mihomo");
        assert!(candidate_from_paths(None, &managed, CoreKind::Mihomo).is_none());
        std::fs::write(&managed, b"\x7fELFfixture").unwrap();
        assert_eq!(candidate_from_paths(None, &managed, CoreKind::Mihomo), Some(managed));
    }

    #[test]
    fn verify_sha256_accepts_matching_digest_and_rejects_others() {
        // sha256("abc")
        let digest = "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        assert!(verify_sha256(b"abc", digest).is_ok());
        assert!(verify_sha256(b"abc", &digest.to_uppercase().replace("SHA256:", "sha256:")).is_ok());
        assert!(verify_sha256(b"abd", digest).is_err());
        assert!(verify_sha256(b"abc", "md5:900150983cd24fb0d6963f7d28e17f72").is_err());
    }

    #[test]
    fn ensure_elf_rejects_non_executables() {
        assert!(ensure_elf(b"\x7fELF\x02\x01\x01").is_ok());
        assert!(ensure_elf(b"<html>Not Found</html>").is_err());
        assert!(ensure_elf(b"").is_err());
    }

    #[tokio::test]
    async fn install_lock_is_exclusive_across_handles() {
        let dir = std::env::temp_dir().join(format!("cv-lock-{}", uuid::Uuid::new_v4()));
        let managed = dir.join("mihomo");
        let first = lock_install(&managed).await.unwrap();

        // A second, independent open of the lock file (as another process
        // would do) must not acquire the lock while the first is held.
        let other = std::fs::OpenOptions::new()
            .write(true)
            .open(install_lock_path(&managed))
            .unwrap();
        assert!(other.try_lock().is_err(), "lock must be exclusive");

        drop(first);
        assert!(other.try_lock().is_ok(), "lock must release on drop");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn linux_asset_covers_common_arches() {
        assert!(linux_asset_name().is_some() || !matches!(std::env::consts::ARCH, "x86_64" | "aarch64"));
    }
}
