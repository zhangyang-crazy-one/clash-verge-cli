//! Passive Linux controller socket checks. Never connect to an unknown owner.

use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::Path;

use anyhow::{Context, bail};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SocketState {
    Missing,
    Bound,
    Stale { dev: u64, ino: u64 },
}

pub(super) fn inspect(path: &Path) -> anyhow::Result<SocketState> {
    inspect_with(path, || std::fs::read_to_string("/proc/net/unix"))
}

pub(super) fn inspect_with(
    path: &Path,
    read_status: impl FnOnce() -> std::io::Result<String>,
) -> anyhow::Result<SocketState> {
    let name = path
        .to_str()
        .context("Controller socket path is not UTF-8; left untouched")?;
    if !path.is_absolute() || name.contains(['\n', '\r']) {
        bail!("Unsafe controller socket path; left untouched");
    }
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            // Unlinking a pathname does not close the bound endpoint.
            let status = read_status().context("Cannot read kernel Unix socket status; controller left untouched")?;
            return Ok(if kernel_has_path(&status, name, None)? {
                SocketState::Bound
            } else {
                SocketState::Missing
            });
        }
        Err(error) => return Err(error).context("Cannot inspect controller socket; left untouched"),
    };
    if !metadata.file_type().is_socket() {
        bail!("Controller path is not a Unix socket; left untouched");
    }
    let parent = path
        .parent()
        .context("Controller socket has no parent; left untouched")?;
    let directory = std::fs::symlink_metadata(parent).context("Cannot inspect controller directory; left untouched")?;
    // A private real directory excludes replacement by other users; reject
    // aliases so the kernel's pathname can be compared without guessing.
    let uid = std::fs::metadata("/proc/self")?.uid();
    if !directory.is_dir()
        || directory.file_type().is_symlink()
        || directory.uid() != uid
        || directory.mode() & 0o077 != 0
        || metadata.uid() != uid
        || parent.canonicalize()? != parent
    {
        bail!("Controller socket is outside an owned private directory; left untouched");
    }
    let status = read_status().context("Cannot read kernel Unix socket status; controller left untouched")?;
    if kernel_has_path(&status, name, Some((metadata.dev(), metadata.ino())))? {
        Ok(SocketState::Bound)
    } else {
        Ok(SocketState::Stale {
            dev: metadata.dev(),
            ino: metadata.ino(),
        })
    }
}

fn kernel_has_path(status: &str, expected: &str, identity: Option<(u64, u64)>) -> anyhow::Result<bool> {
    let mut lines = status.lines();
    if lines.next().map(|line| line.split_whitespace().collect::<Vec<_>>())
        != Some(vec![
            "Num", "RefCount", "Protocol", "Flags", "Type", "St", "Inode", "Path",
        ])
    {
        bail!("Unrecognized kernel Unix socket status; controller left untouched");
    }
    let mut found = false;
    for line in lines {
        let mut remaining = line;
        for column in 0..7 {
            remaining = remaining.trim_start_matches([' ', '\t']);
            let end = remaining.find([' ', '\t']).unwrap_or(remaining.len());
            let field = &remaining[..end];
            let valid = match column {
                0 => field
                    .strip_suffix(':')
                    .is_some_and(|value| u64::from_str_radix(value, 16).is_ok()),
                6 => field.parse::<u64>().is_ok(),
                _ => u64::from_str_radix(field, 16).is_ok(),
            };
            if !valid {
                bail!("Malformed kernel Unix socket status; controller left untouched");
            }
            remaining = &remaining[end..];
        }
        let bound_path = remaining.trim_start_matches([' ', '\t']);
        found |= bound_path == expected;
        // A process may bind via a symlink or lexical alias. The proc
        // socket inode is a sockfs inode, so compare filesystem identities.
        if Path::new(bound_path).is_absolute()
            && let Some(identity) = identity
            && let Ok(metadata) = std::fs::metadata(bound_path)
        {
            found |= (metadata.dev(), metadata.ino()) == identity;
        }
    }
    Ok(found)
}

/// Revalidate both kernel state and inode immediately before unlinking.
pub(super) fn remove_stale(path: &Path) -> anyhow::Result<()> {
    match inspect(path)? {
        SocketState::Missing => Ok(()),
        SocketState::Bound => bail!("Controller socket is still bound; left untouched"),
        stale @ SocketState::Stale { .. } => {
            if inspect(path)? != stale {
                bail!("Controller socket changed before cleanup; left untouched");
            }
            std::fs::remove_file(path).context("Failed to remove verified stale controller socket")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    #[test]
    fn kernel_table_matches_exact_bound_path_including_spaces() {
        let table = "Num RefCount Protocol Flags Type St Inode Path\n0000: 0002 0000 0001 0001 01 42 /tmp/a b.sock\n";
        assert!(kernel_has_path(table, "/tmp/a b.sock", None).unwrap());
        assert!(!kernel_has_path(table, "/tmp/a", None).unwrap());
        assert!(kernel_has_path("", "/tmp/a", None).is_err());
        assert!(kernel_has_path(&format!("{table}bad row\n"), "/tmp/a b.sock", None).is_err());
    }

    #[test]
    fn passive_inspection_and_cleanup_preserve_live_and_unsafe_paths() {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        std::fs::set_permissions(home.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = home.path().join("a b.sock");
        assert_eq!(inspect(&path).unwrap(), SocketState::Missing);
        let listener = UnixListener::bind(&path).unwrap();
        assert_eq!(inspect(&path).unwrap(), SocketState::Bound);
        assert!(remove_stale(&path).is_err());
        drop(listener);
        assert!(matches!(inspect(&path).unwrap(), SocketState::Stale { .. }));
        remove_stale(&path).unwrap();
        std::fs::write(&path, "preserve").unwrap();
        assert!(remove_stale(&path).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "preserve");
        let link = home.path().join("link.sock");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(remove_stale(&link).is_err());
    }

    #[test]
    fn stale_cleanup_requires_private_real_parent() {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        let directory = home.path().join("runtime");
        std::fs::create_dir(&directory).unwrap();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = directory.join("controller.sock");
        drop(UnixListener::bind(&path).unwrap());
        let inode = std::fs::symlink_metadata(&path).unwrap().ino();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o770)).unwrap();
        assert!(remove_stale(&path).is_err());
        assert_eq!(std::fs::symlink_metadata(&path).unwrap().ino(), inode);
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        let alias = home.path().join("alias");
        std::os::unix::fs::symlink(&directory, &alias).unwrap();
        assert!(remove_stale(&alias.join("controller.sock")).is_err());
        assert!(path.exists());
        remove_stale(&path).unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn live_endpoint_bound_through_alias_is_preserved() {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        std::fs::set_permissions(home.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let alias = home.path().join("alias");
        std::os::unix::fs::symlink(home.path(), &alias).unwrap();
        let _listener = UnixListener::bind(alias.join("controller.sock")).unwrap();
        let path = home.path().join("controller.sock");
        assert_eq!(inspect(&path).unwrap(), SocketState::Bound);
        assert!(remove_stale(&path).is_err());
        assert!(path.exists());
    }
}
