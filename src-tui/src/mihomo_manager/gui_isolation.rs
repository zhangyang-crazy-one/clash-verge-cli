//! Narrow GUI coexistence for explicitly separate, non-TUN CLI resources.
//! Every probe is passive or a temporary local bind; no controller is contacted.

use std::{collections::HashSet, os::unix::fs::MetadataExt, path::Path};

use anyhow::{Context, bail};
use serde_yaml_ng::{Mapping, Value};

use super::CoreKind;

pub(super) fn check(
    config_dir: &Path,
    socket: &Path,
    owned_pid: Option<u32>,
    candidate: Option<(&Path, CoreKind)>,
) -> anyhow::Result<()> {
    let home = clash_verge_core::utils::dirs::app_home_dir()?;
    let canonical = config_dir
        .canonicalize()
        .context("isolated CLI configuration directory is missing")?;
    if canonical != config_dir || canonical != home.canonicalize()? || is_gui_directory(&canonical) {
        bail!("GUI coexistence requires a distinct canonical CLI configuration directory");
    }
    check_directory(config_dir, false)?;
    let socket_parent = socket.parent().context("controller socket has no parent")?;
    check_directory(socket_parent, true)?;
    if socket_parent.canonicalize()? != socket_parent {
        bail!("GUI coexistence requires a private canonical controller socket directory");
    }
    // The CLI socket must not resolve through a GUI data directory either.
    if is_gui_directory(socket_parent) {
        bail!("GUI controller socket cannot be used by the CLI");
    }
    let config_path = clash_verge_core::utils::dirs::clash_path()?;
    let mut config = read_mapping(&config_path)?;
    let verge_path = home.join("verge.yaml");
    let verge = read_mapping(&verge_path)?;
    if verge.get("enable_tun_mode").and_then(Value::as_bool) == Some(true)
        || verge.get("enable_system_proxy").and_then(Value::as_bool) == Some(true)
        || config
            .get("tun")
            .and_then(|tun| tun.get("enable"))
            .and_then(Value::as_bool)
            == Some(true)
    {
        bail!("GUI coexistence requires TUN and system proxy disabled in the isolated CLI configuration");
    }
    apply_ports(&mut config, &verge);
    let original = yaml_listeners(&config, socket)?;
    let listeners = match candidate {
        Some((path, CoreKind::Mihomo)) => yaml_listeners(&read_mapping(path)?, socket)?,
        Some((path, CoreKind::SingBox)) => json_listeners(path)?,
        None => original.clone(),
    };
    for address in listeners {
        let address: std::net::SocketAddr = address.parse().context("invalid isolated listener address")?;
        if !address.ip().is_loopback() || address.port() == 0 {
            bail!("GUI coexistence requires explicit loopback listener ports");
        }
        if let Err(error) = std::net::TcpListener::bind(address)
            && !owned_pid
                .map(|pid| owns_listener(pid, address, "tcp"))
                .transpose()?
                .unwrap_or(false)
        {
            return Err(error).context("isolated CLI TCP listener belongs to another process");
        }
        if let Err(error) = std::net::UdpSocket::bind(address)
            && !owned_pid
                .map(|pid| owns_listener(pid, address, "udp"))
                .transpose()?
                .unwrap_or(false)
        {
            return Err(error).context("isolated CLI UDP listener belongs to another process");
        }
    }
    Ok(())
}

fn is_gui_directory(path: &Path) -> bool {
    path.components().any(|component| {
        matches!(
            component.as_os_str().to_str(),
            Some("clash-verge" | "clash-verge-rev" | "io.github.clash-verge-rev.clash-verge-rev")
        )
    })
}

fn check_directory(path: &Path, private: bool) -> anyhow::Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    let uid = std::fs::metadata("/proc/self")?.uid();
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || metadata.uid() != uid
        || metadata.mode() & if private { 0o077 } else { 0o022 } != 0
    {
        bail!(
            "GUI coexistence requires an owned{} directory",
            if private { " private" } else { "" }
        );
    }
    Ok(())
}

fn read_mapping(path: &Path) -> anyhow::Result<Mapping> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.nlink() != 1
        || metadata.uid() != std::fs::metadata("/proc/self")?.uid()
    {
        bail!("isolated configuration must be an owned regular file without aliases or hard links");
    }
    serde_yaml_ng::from_slice(&std::fs::read(path)?).context("invalid isolated CLI settings")
}

pub(super) fn owns_listener(pid: u32, address: std::net::SocketAddr, protocol: &str) -> anyhow::Result<bool> {
    owns_listener_with_base(pid, address, protocol, Path::new("/proc/net"))
}

/// Reads the kernel listener tables from `base` (injectable for tests). Hosts
/// booted with `ipv6.disable=1` (and minimal containers) have no tcp6/udp6
/// tables at all: that means "no IPv6 listeners", not a verification
/// failure — only the IPv4 table is mandatory.
fn owns_listener_with_base(
    pid: u32,
    address: std::net::SocketAddr,
    protocol: &str,
    base: &Path,
) -> anyhow::Result<bool> {
    let mut inodes = HashSet::new();
    for entry in std::fs::read_dir(format!("/proc/{pid}/fd")).context("cannot verify owned core listeners")? {
        let entry = entry?;
        // A descriptor can be closed by another thread between the directory
        // listing and read_link (parallel tests, runtime I/O). A closed fd no
        // longer holds a listener, so skip it instead of failing the probe.
        let link = match std::fs::read_link(entry.path()) {
            Ok(link) => link,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error).context("cannot verify owned core descriptor"),
        };
        if let Some(inode) = link
            .to_str()
            .and_then(|link| link.strip_prefix("socket:["))
            .and_then(|link| link.strip_suffix(']'))
        {
            inodes.insert(inode.to_owned());
        }
    }
    let mut owned = false;
    for family in ["", "6"] {
        let status = match std::fs::read_to_string(base.join(format!("{protocol}{family}"))) {
            Ok(status) => status,
            Err(error) if family == "6" && error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error).context("cannot verify kernel listener ownership"),
        };
        for line in status.lines().skip(1) {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.len() < 10 {
                bail!("unrecognized kernel listener ownership data");
            }
            // Accepted/outgoing TCP connections do not establish listener
            // ownership. Multiple listeners sharing a port must all be owned.
            if protocol == "tcp" && fields[3] != "0A" {
                continue;
            }
            let (host, port) = fields[1].split_once(':').context("invalid kernel listener address")?;
            let port = u16::from_str_radix(port, 16)?;
            let host = decode_kernel_ip(host)?;
            // An unspecified listener (`0.0.0.0` or dual-stack `[::]`) accepts
            // the probed address regardless of address family.
            if port == address.port() && (host == address.ip() || host.is_unspecified()) {
                if !inodes.contains(fields[9]) {
                    return Ok(false);
                }
                owned = true;
            }
        }
    }
    Ok(owned)
}

fn decode_kernel_ip(host: &str) -> anyhow::Result<std::net::IpAddr> {
    match host.len() {
        8 => Ok(std::net::Ipv4Addr::from(u32::from_str_radix(host, 16)?.to_le_bytes()).into()),
        32 => {
            let mut bytes = [0; 16];
            for index in 0..4 {
                bytes[index * 4..index * 4 + 4]
                    .copy_from_slice(&u32::from_str_radix(&host[index * 8..index * 8 + 8], 16)?.to_le_bytes());
            }
            Ok(std::net::Ipv6Addr::from(bytes).into())
        }
        _ => bail!("invalid kernel listener IP"),
    }
}

fn apply_ports(config: &mut Mapping, verge: &Mapping) {
    if let Some(port) = verge.get("verge_mixed_port") {
        config.insert("mixed-port".into(), port.clone());
    }
    for (field, enabled, port) in [
        ("socks-port", "verge_socks_enabled", "verge_socks_port"),
        ("port", "verge_http_enabled", "verge_port"),
        ("redir-port", "verge_redir_enabled", "verge_redir_port"),
        ("tproxy-port", "verge_tproxy_enabled", "verge_tproxy_port"),
    ] {
        if verge.get(enabled).and_then(Value::as_bool) == Some(false) {
            config.remove(field);
        } else if verge.get(enabled).and_then(Value::as_bool) == Some(true)
            && let Some(port) = verge.get(port)
        {
            config.insert(field.into(), port.clone());
        }
    }
}

fn yaml_listeners(config: &Mapping, socket: &Path) -> anyhow::Result<HashSet<String>> {
    if config.get("allow-lan").and_then(Value::as_bool) == Some(true)
        || config
            .get("tun")
            .and_then(|tun| tun.get("enable"))
            .and_then(Value::as_bool)
            == Some(true)
        || config
            .get("listeners")
            .is_some_and(|value| value.as_sequence().is_none_or(|list| !list.is_empty()))
    {
        bail!("isolated GUI coexistence only supports loopback listeners without TUN or custom listeners");
    }
    if let Some(path) = config.get("external-controller-unix").and_then(Value::as_str)
        && Path::new(path) != socket
    {
        bail!("isolated candidate points at another Unix controller");
    }
    let mut addresses = HashSet::new();
    let bind = config
        .get("bind-address")
        .and_then(Value::as_str)
        .unwrap_or("127.0.0.1");
    let bind: std::net::IpAddr = bind
        .parse()
        .context("isolated listener bind-address must be a loopback IP")?;
    if !bind.is_loopback() {
        bail!("isolated listener bind-address must be loopback");
    }
    for field in ["mixed-port", "socks-port", "port", "redir-port", "tproxy-port"] {
        if let Some(port) = config.get(field).and_then(Value::as_u64).filter(|port| *port != 0) {
            let port = u16::try_from(port).context("invalid isolated listener port")?;
            addresses.insert(std::net::SocketAddr::new(bind, port).to_string());
        }
    }
    if let Some(address) = config.get("external-controller").and_then(Value::as_str)
        && !address.is_empty()
    {
        addresses.insert(address.to_owned());
    }
    if let Some(address) = config
        .get("dns")
        .and_then(|dns| dns.get("listen"))
        .and_then(Value::as_str)
        && !address.is_empty()
    {
        addresses.insert(address.to_owned());
    }
    Ok(addresses)
}

fn json_listeners(path: &Path) -> anyhow::Result<HashSet<String>> {
    let config: serde_json::Value = serde_json::from_slice(&std::fs::read(path)?)?;
    if config
        .get("services")
        .is_some_and(|services| services.as_array().is_none_or(|services| !services.is_empty()))
    {
        bail!("isolated GUI coexistence does not support extra sing-box services");
    }
    let mut addresses = HashSet::new();
    if let Some(inbounds) = config["inbounds"].as_array() {
        for inbound in inbounds {
            if inbound["type"].as_str() == Some("tun") {
                bail!("isolated GUI coexistence does not support TUN");
            }
            let host = inbound["listen"]
                .as_str()
                .context("isolated inbound has no explicit listen address")?
                .parse()?;
            let port = u16::try_from(
                inbound["listen_port"]
                    .as_u64()
                    .context("isolated inbound has no listen port")?,
            )?;
            addresses.insert(std::net::SocketAddr::new(host, port).to_string());
        }
    }
    if let Some(address) = config
        .pointer("/experimental/clash_api/external_controller")
        .and_then(serde_json::Value::as_str)
    {
        addresses.insert(address.to_owned());
    }
    Ok(addresses)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn available_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    #[tokio::test]
    async fn gui_coexistence_requires_private_distinct_non_tun_available_resources() {
        let home = tempfile::tempdir().unwrap();
        let _guard = crate::profile_store::store::tests::claim_test_app_home(home.path().to_path_buf()).await;
        let runtime = home.path().join("runtime");
        std::fs::create_dir(&runtime).unwrap();
        std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700)).unwrap();
        let socket = runtime.join("controller.sock");
        let mixed = available_port();
        let controller = available_port();
        let config = format!(
            "mixed-port: {mixed}\nexternal-controller: 127.0.0.1:{controller}\nexternal-controller-unix: {}\ntun: {{enable: false}}\nallow-lan: false\n",
            socket.display()
        );
        std::fs::write(home.path().join("config.yaml"), &config).unwrap();
        std::fs::write(
            home.path().join("verge.yaml"),
            "enable_tun_mode: false\nenable_system_proxy: false\n",
        )
        .unwrap();
        check(home.path(), &socket, None, None).expect("isolated fixture can coexist with a GUI");
        let manager = crate::mihomo_manager::MihomoManager::new(home.path().to_path_buf()).with_socket(socket.clone());
        manager
            .guided_owner_check(true)
            .expect("real policy accepts only the isolated fixture");
        let occupied = std::net::TcpListener::bind(("127.0.0.1", mixed)).unwrap();
        assert!(check(home.path(), &socket, None, None).is_err());
        drop(occupied);
        std::fs::write(home.path().join("verge.yaml"), "enable_tun_mode: true\n").unwrap();
        assert!(check(home.path(), &socket, None, None).is_err());
        std::fs::write(home.path().join("verge.yaml"), "enable_system_proxy: true\n").unwrap();
        assert!(check(home.path(), &socket, None, None).is_err());
        std::fs::write(home.path().join("verge.yaml"), "enable_tun_mode: false\n").unwrap();
        std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(check(home.path(), &socket, None, None).is_err());
        std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700)).unwrap();
        let candidate = home.path().join("candidate.yaml");
        std::fs::write(&candidate, config.replace("enable: false", "enable: true")).unwrap();
        assert!(check(home.path(), &socket, None, Some((&candidate, CoreKind::Mihomo))).is_err());
        std::fs::write(&candidate, &config).unwrap();
        check(home.path(), &socket, None, Some((&candidate, CoreKind::Mihomo))).unwrap();
        let json = home.path().join("candidate.json");
        std::fs::write(&json, serde_json::to_vec(&serde_json::json!({"inbounds": [{"type": "mixed", "listen": "127.0.0.1", "listen_port": mixed}], "experimental": {"clash_api": {"external_controller": format!("127.0.0.1:{controller}")}}})).unwrap()).unwrap();
        check(home.path(), &socket, None, Some((&json, CoreKind::SingBox))).unwrap();
        std::fs::write(&json, b"{\"inbounds\":[{\"type\":\"tun\"}]}").unwrap();
        assert!(check(home.path(), &socket, None, Some((&json, CoreKind::SingBox))).is_err());
    }

    #[test]
    fn gui_data_directories_and_wildcard_or_custom_listeners_are_rejected() {
        assert!(is_gui_directory(Path::new(
            "/home/fixture/.local/share/io.github.clash-verge-rev.clash-verge-rev"
        )));
        assert!(!is_gui_directory(Path::new(
            "/home/fixture/.local/share/clash-verge-cli"
        )));
        for raw in [
            "bind-address: 0.0.0.0\nmixed-port: 12345\n",
            "allow-lan: true\n",
            "listeners: [{type: mixed}]\n",
            "external-controller-unix: /tmp/gui.sock\n",
        ] {
            let config: Mapping = serde_yaml_ng::from_str(raw).unwrap();
            assert!(yaml_listeners(&config, Path::new("/tmp/cli.sock")).is_err());
        }
    }

    #[test]
    fn owned_listener_exemption_uses_actual_pid_socket_inodes_for_tcp_and_udp() {
        let tcp = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let tcp_addr = tcp.local_addr().unwrap();
        let udp = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let udp_addr = udp.local_addr().unwrap();
        assert!(owns_listener(std::process::id(), tcp_addr, "tcp").unwrap());
        assert!(owns_listener(std::process::id(), udp_addr, "udp").unwrap());
        assert!(!owns_listener(std::process::id(), tcp_addr, "udp").unwrap());
        assert!(owns_listener(u32::MAX, tcp_addr, "tcp").is_err());
        assert_eq!(decode_kernel_ip("0100007F").unwrap().to_string(), "127.0.0.1");
        assert_eq!(
            decode_kernel_ip("00000000000000000000000001000000")
                .unwrap()
                .to_string(),
            "::1"
        );
        let home = tempfile::tempdir().unwrap();
        let original = home.path().join("original.yaml");
        std::fs::write(&original, "mode: rule\n").unwrap();
        let alias = home.path().join("hard-link.yaml");
        std::fs::hard_link(&original, &alias).unwrap();
        assert!(read_mapping(&alias).is_err());
    }

    #[test]
    fn owns_listener_tolerates_concurrent_fd_churn() {
        // Regression: read_dir + read_link over /proc/self/fd must not fail
        // when other threads close descriptors mid-scan.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = stop.clone();
        let churn = std::thread::spawn(move || {
            while !flag.load(std::sync::atomic::Ordering::Relaxed) {
                // Open and immediately close a batch of descriptors.
                let batch: Vec<_> = (0..32).filter_map(|_| std::fs::File::open("/dev/null").ok()).collect();
                drop(batch);
            }
        });
        for _ in 0..200 {
            assert!(owns_listener(std::process::id(), addr, "tcp").unwrap());
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        churn.join().unwrap();
    }

    #[test]
    fn owns_listener_tolerates_missing_ipv6_kernel_table() {
        // Regression: hosts booted with ipv6.disable=1 (and minimal containers)
        // have no tcp6/udp6 tables. Synthesize a kernel-table directory with
        // only the IPv4 table; ownership must still verify.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("tcp"),
            std::fs::read_to_string("/proc/net/tcp").unwrap(),
        )
        .unwrap();
        assert!(owns_listener_with_base(std::process::id(), addr, "tcp", dir.path()).unwrap());
        // The IPv4 table itself missing remains a hard error.
        let empty = tempfile::tempdir().unwrap();
        assert!(owns_listener_with_base(std::process::id(), addr, "tcp", empty.path()).is_err());
    }
}
