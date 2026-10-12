//! Opt-in pinned-core regression. All traffic stays on loopback and all
//! files/XDG state stay in a private tempfile. The copied core has no xattrs
//! (in particular no CAP_NET_ADMIN); no TUN or system proxy is configured.
//!
//! CLASH_TEST_SINGBOX_BIN=/path/to/sing-box cargo test -p clash-verge-cli \
//!   pinned_core_dns_semantics_loopback -- --ignored --test-threads=1

#![allow(clippy::expect_used, clippy::unwrap_used)]

use super::*;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream, UdpSocket};
use std::process::{Child, Command, Stdio};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::{Duration, Instant};

struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct DnsFixture {
    port: u16,
    seen: Arc<Mutex<Vec<(String, u16)>>>,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}
impl DnsFixture {
    fn new() -> Self {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        socket.set_nonblocking(true).unwrap();
        let port = socket.local_addr().unwrap().port();
        let socket6 = UdpSocket::bind((std::net::Ipv6Addr::LOCALHOST, port)).unwrap();
        socket6.set_nonblocking(true).unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (observations, stopping) = (seen.clone(), stop.clone());
        let thread = thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while !stopping.load(Ordering::Relaxed) {
                for socket in [&socket, &socket6] {
                    let Ok((len, peer)) = socket.recv_from(&mut buf) else {
                        continue;
                    };
                    let (name, end) = read_name(&buf[..len], 12);
                    let query_type = u16::from_be_bytes([buf[end], buf[end + 1]]);
                    observations.lock().unwrap().push((name, query_type));
                    let answer = match query_type {
                        1 => vec![127, 0, 0, 1],
                        28 => "::1".parse::<std::net::Ipv6Addr>().unwrap().octets().to_vec(),
                        _ => Vec::new(),
                    };
                    let mut response = buf[..2].to_vec();
                    response.extend([0x81, 0x80, 0, 1, 0, u8::from(!answer.is_empty()), 0, 0, 0, 0]);
                    response.extend(&buf[12..end + 4]);
                    if !answer.is_empty() {
                        response.extend([0xc0, 0x0c]);
                        response.extend(query_type.to_be_bytes());
                        response.extend([0, 1, 0, 0, 0, 30, 0, answer.len() as u8]);
                        response.extend(answer);
                    }
                    socket.send_to(&response, peer).unwrap();
                }
                thread::sleep(Duration::from_millis(2));
            }
        });
        Self {
            port,
            seen,
            stop,
            thread: Some(thread),
        }
    }
    fn saw(&self, name: &str) -> bool {
        self.seen.lock().unwrap().iter().any(|(host, _)| host == name)
    }
}
impl Drop for DnsFixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.thread.take() {
            handle.join().unwrap();
        }
    }
}

fn read_name(packet: &[u8], mut offset: usize) -> (String, usize) {
    let mut labels = Vec::new();
    while packet[offset] != 0 {
        let len = packet[offset] as usize;
        labels.push(std::str::from_utf8(&packet[offset + 1..offset + 1 + len]).unwrap());
        offset += len + 1;
    }
    (labels.join("."), offset + 1)
}

fn query(port: u16, host: &str, query_type: u16) -> Vec<u8> {
    let mut bytes = vec![0x12, 0x34, 1, 0, 0, 1, 0, 0, 0, 0, 0, 0];
    for label in host.split('.') {
        bytes.push(label.len() as u8);
        bytes.extend(label.as_bytes());
    }
    bytes.push(0);
    bytes.extend(query_type.to_be_bytes());
    bytes.extend([0, 1]);
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    socket.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    socket.send_to(&bytes, ("127.0.0.1", port)).unwrap();
    let mut response = [0u8; 4096];
    let length = socket.recv(&mut response).unwrap();
    assert_eq!(&response[..2], &[0x12, 0x34]);
    assert_eq!(response[3] & 15, 0, "DNS rcode for {host}");
    assert!(
        u16::from_be_bytes([response[6], response[7]]) > 0,
        "no answer for {host}"
    );
    let len = if query_type == 1 { 4 } else { 16 };
    response[length - len..length].to_vec()
}

fn free_tcp_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}
fn free_udp_port() -> u16 {
    UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn start(core: &std::path::Path, dir: &std::path::Path, config: &Value) -> Process {
    let path = dir.join("config.json");
    std::fs::write(&path, serde_json::to_vec_pretty(config).unwrap()).unwrap();
    let checked = Command::new(core)
        .args(["check", "-c"])
        .arg(&path)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(checked.status.success(), "{}", String::from_utf8_lossy(&checked.stderr));
    let log = std::fs::File::create(dir.join("core.log")).unwrap();
    Process(
        Command::new(core)
            .args(["run", "-c"])
            .arg(&path)
            .current_dir(dir)
            .env("XDG_CONFIG_HOME", dir.join("xdg-config"))
            .env("XDG_DATA_HOME", dir.join("xdg-data"))
            .stdout(Stdio::from(log.try_clone().unwrap()))
            .stderr(Stdio::from(log))
            .spawn()
            .unwrap(),
    )
}

fn wait_ready(process: &mut Process, port: u16, dir: &std::path::Path) {
    let until = Instant::now() + Duration::from_secs(5);
    loop {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return;
        }
        if process.0.try_wait().unwrap().is_some() || Instant::now() > until {
            panic!(
                "core did not start: {}",
                std::fs::read_to_string(dir.join("core.log")).unwrap()
            );
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn connect_through(port: u16) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(4))).unwrap();
    stream
        .write_all(b"CONNECT 192.0.2.1:80 HTTP/1.1\r\nHost: 192.0.2.1:80\r\n\r\n")
        .unwrap();
    let mut buf = [0u8; 1024];
    let mut reply = Vec::new();
    while !reply.ends_with(b"\r\n\r\n") {
        let len = stream.read(&mut buf).unwrap();
        assert!(len > 0, "CONNECT closed");
        reply.extend(&buf[..len]);
    }
    let reply = std::str::from_utf8(&reply).unwrap();
    assert!(reply.contains("200"), "CONNECT failed: {reply}");
    // sing-box acknowledges inbound CONNECT before outbound dialing. Only
    // payload returned by the actual proxy proves DNS and dialing completed.
    stream.write_all(b"verified-dns").unwrap();
    let mut echoed = [0u8; 12];
    stream.read_exact(&mut echoed).unwrap();
    assert_eq!(&echoed, b"verified-dns");
}

struct Threads {
    stop: Arc<AtomicBool>,
    handles: Vec<thread::JoinHandle<()>>,
}
impl Drop for Threads {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for handle in self.handles.drain(..) {
            handle.join().unwrap();
        }
    }
}

#[test]
#[ignore = "requires CLASH_TEST_SINGBOX_BIN; config checks and loopback interface DNS only"]
fn pinned_core_dns_fragment_validation_and_loopback_binding() {
    let binary = std::env::var_os("CLASH_TEST_SINGBOX_BIN").expect("set CLASH_TEST_SINGBOX_BIN");
    let root = tempfile::tempdir().unwrap();
    let core = root.path().join("sing-box");
    std::fs::copy(binary, &core).unwrap();
    let version = Command::new(&core).arg("version").output().unwrap();
    assert!(version.status.success());
    assert!(String::from_utf8_lossy(&version.stdout).contains("1.14.2"));
    for fragment in [
        "eth0",
        "direct",
        "block",
        "reject",
        "GLOBAL",
        "RULES",
        "DIRECT",
        "Proxy%20%E4%B8%AD",
        "eth0=true&RULES=true",
    ] {
        let yaml = format!(
            "proxies:\n  - {{name: node, type: http, server: 127.0.0.1, port: 8080}}\nproxy-groups:\n  - {{name: 'Proxy 中', type: select, proxies: [node, DIRECT]}}\ndns:\n  nameserver: ['https://1.1.1.1/dns-query#{fragment}']\n"
        );
        let (report, config) = super::tests::generated_profile_dns(&yaml);
        if fragment == "RULES" {
            assert!(report.spec.is_none());
            assert!(
                report
                    .notes
                    .iter()
                    .any(|note| note.contains("RULES") && note.contains("unsupported"))
            );
        }
        let path = root.path().join("fragment-check.json");
        std::fs::write(&path, serde_json::to_vec_pretty(&config).unwrap()).unwrap();
        let checked = Command::new(&core)
            .args(["check", "-c"])
            .arg(&path)
            .current_dir(root.path())
            .env("XDG_CONFIG_HOME", root.path().join("xdg-config"))
            .env("XDG_DATA_HOME", root.path().join("xdg-data"))
            .output()
            .unwrap();
        assert!(
            checked.status.success(),
            "{fragment}: {}",
            String::from_utf8_lossy(&checked.stderr)
        );
    }
    // Only the loopback interface is used for live traffic. The eth0/public
    // DoH configurations above are checked, never started or queried.
    let upstream = DnsFixture::new();
    let report = dns_conversion_from_clash_yaml(&format!(
        "dns:\n  nameserver: ['udp://127.0.0.1:{}#lo']\n",
        upstream.port
    ))
    .unwrap();
    let spec = report.spec.unwrap();
    let dns_port = free_udp_port();
    let ready_port = free_tcp_port();
    let config = json!({
        "inbounds": [
            {"type":"mixed", "listen":"127.0.0.1", "listen_port":ready_port},
            {"type":"direct", "tag":"dns-in", "network":"udp", "listen":"127.0.0.1", "listen_port":dns_port}
        ],
        "outbounds":[{"type":"direct", "tag":"direct"}],
        "dns":build_dns_section(&spec).unwrap().unwrap(),
        "route":{"rules":[{"inbound":["dns-in"], "action":"hijack-dns"}], "final":"direct"}
    });
    assert_eq!(config["dns"]["servers"][0]["bind_interface"], "lo");
    let mut process = start(&core, root.path(), &config);
    wait_ready(&mut process, ready_port, root.path());
    assert_eq!(query(dns_port, "bound-interface.example", 1), vec![127, 0, 0, 1]);
    assert!(upstream.saw("bound-interface.example"));
    drop(process);
}

#[test]
#[ignore = "requires CLASH_TEST_SINGBOX_BIN; uses only private files and loopback servers"]
fn pinned_core_dns_semantics_loopback() {
    let binary = std::env::var_os("CLASH_TEST_SINGBOX_BIN").expect("set CLASH_TEST_SINGBOX_BIN");
    let root = tempfile::tempdir().unwrap();
    let core = root.path().join("sing-box");
    // fs::copy copies contents/permissions, not extended attributes.
    std::fs::copy(binary, &core).unwrap();
    let version = Command::new(&core).arg("version").output().unwrap();
    assert!(version.status.success());
    assert!(
        String::from_utf8_lossy(&version.stdout).contains("1.14.2"),
        "use pinned 1.14.2"
    );
    let bootstrap = DnsFixture::new();
    let business = DnsFixture::new();
    let policy = DnsFixture::new();
    let proxy4 = TcpListener::bind("127.0.0.1:0").unwrap();
    let proxy_port = proxy4.local_addr().unwrap().port();
    let proxy6 = TcpListener::bind((std::net::Ipv6Addr::LOCALHOST, proxy_port)).unwrap();
    let mut proxy_threads = Threads {
        stop: Arc::new(AtomicBool::new(false)),
        handles: Vec::new(),
    };
    for listener in [proxy4, proxy6] {
        listener.set_nonblocking(true).unwrap();
        let stop = proxy_threads.stop.clone();
        proxy_threads.handles.push(thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                if let Ok((mut stream, _)) = listener.accept() {
                    stream.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
                    let mut bytes = [0u8; 2048];
                    let len = stream.read(&mut bytes).unwrap_or(0);
                    if len == 0 {
                        continue;
                    }
                    assert!(String::from_utf8_lossy(&bytes[..len]).starts_with("CONNECT "));
                    stream
                        .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
                        .unwrap();
                    let mut payload = [0u8; 12];
                    stream.read_exact(&mut payload).unwrap();
                    stream.write_all(&payload).unwrap();
                } else {
                    thread::sleep(Duration::from_millis(10));
                }
            }
        }));
    }
    let yaml = format!(
        r#"
dns:
  default-nameserver: ['127.0.0.1:{}']
  nameserver: ['udp://business.example:{}']
  nameserver-policy:
    '+.example': 'udp://127.0.0.1:{}'
  enhanced-mode: fake-ip
  fake-ip-filter: [real.example, '*.lan', '.internal', '+.corp']
"#,
        bootstrap.port, business.port, policy.port
    );
    let report = dns_conversion_from_clash_yaml(&yaml).unwrap();
    let spec = report.spec.unwrap();
    let mut outbounds = vec![json!({"type":"direct", "tag":"direct"})];
    let servers = ["node.example", "unmatched.test", "127.0.0.1", "::1"];
    let inbound_ports: Vec<_> = (0..servers.len()).map(|_| free_tcp_port()).collect();
    let mut inbounds = Vec::new();
    let mut routes = Vec::new();
    for (index, server) in servers.iter().enumerate() {
        let tag = format!("node-{index}");
        outbounds.push(json!({"type":"http", "tag":tag, "server":server, "server_port":proxy_port}));
        inbounds.push(json!({"type":"mixed", "tag":tag, "listen":"127.0.0.1", "listen_port":inbound_ports[index]}));
        routes.push(json!({"inbound":[tag], "outbound":tag}));
    }
    let dns_port = free_udp_port();
    inbounds
        .push(json!({"type":"direct", "tag":"dns-in", "network":"udp", "listen":"127.0.0.1", "listen_port":dns_port}));
    routes.insert(0, json!({"inbound":["dns-in"], "action":"hijack-dns"}));
    apply_outbound_domain_resolvers(&spec, &mut outbounds, &[]).unwrap();
    let config = json!({"log":{"level":"debug"}, "inbounds":inbounds, "outbounds":outbounds,
        "dns":build_dns_section(&spec).unwrap().unwrap(),
        "route":{"rules":routes, "final":"direct", "default_domain_resolver":default_domain_resolver(&spec)}});
    // Encrypted upstreams share the same bootstrap dial options. Check their
    // native shape separately; live lookup below exercises domain bootstrap
    // with a loopback UDP upstream rather than an external TLS dependency.
    for kind in [DnsServerKind::Tls, DnsServerKind::Https] {
        let mut encrypted_spec = spec.clone();
        encrypted_spec
            .servers
            .iter_mut()
            .find(|server| server.tag == spec.final_server.clone().unwrap())
            .unwrap()
            .kind = kind;
        let mut checked_config = config.clone();
        checked_config["dns"] = build_dns_section(&encrypted_spec).unwrap().unwrap();
        let path = root.path().join("encrypted-check.json");
        std::fs::write(&path, serde_json::to_vec(&checked_config).unwrap()).unwrap();
        let checked = Command::new(&core)
            .args(["check", "-c"])
            .arg(path)
            .current_dir(root.path())
            .output()
            .unwrap();
        assert!(
            checked.status.success(),
            "{kind:?}: {}",
            String::from_utf8_lossy(&checked.stderr)
        );
    }
    let mut process = start(&core, root.path(), &config);
    wait_ready(&mut process, inbound_ports[0], root.path());
    for port in &inbound_ports {
        connect_through(*port);
    }
    assert!(policy.saw("node.example"), "matching node must query policy DNS");
    assert!(!bootstrap.saw("node.example"), "node lookup must not leak to bootstrap");
    assert!(!business.saw("node.example"), "policy must win over business final");
    assert!(business.saw("unmatched.test"), "unmatched node must query business DNS");
    assert!(!bootstrap.saw("unmatched.test"));
    assert!(
        bootstrap.saw("business.example"),
        "upstream hostname must bootstrap separately"
    );
    for host in ["127.0.0.1", "::1"] {
        assert!(!policy.saw(host) && !business.saw(host) && !bootstrap.saw(host));
    }
    for (host, real) in [
        ("real.example", true),
        ("sub.real.example", false),
        ("host.lan", true),
        ("deep.host.lan", false),
        ("a.b.internal", true),
        ("internal", false),
        ("corp", true),
        ("a.corp", true),
        ("my-clock.test", false),
        ("other.test", false),
    ] {
        for (kind, loopback) in [
            (1, vec![127, 0, 0, 1]),
            (28, std::net::Ipv6Addr::LOCALHOST.octets().to_vec()),
        ] {
            let answer = query(dns_port, host, kind);
            if real {
                assert_eq!(answer, loopback, "{host}/{kind}");
            } else {
                assert_ne!(answer, loopback, "{host}/{kind} must be fake-IP");
            }
        }
    }
    assert!(policy.saw("real.example"), "real-IP filter retains nameserver-policy");
    assert!(!business.saw("real.example"));
    drop(process);
    for mode in ["whitelist", "rule"] {
        let mode_yaml = if mode == "whitelist" {
            yaml.replace(
                "enhanced-mode: fake-ip",
                "enhanced-mode: fake-ip\n  fake-ip-filter-mode: whitelist",
            )
        } else {
            yaml.replace("fake-ip-filter: [real.example, '*.lan', '.internal', '+.corp']", "fake-ip-filter-mode: rule\n  fake-ip-filter:\n    - DOMAIN,real.example,real-ip\n    - DOMAIN-SUFFIX,example,fake-ip\n    - DOMAIN-KEYWORD,clock,fake-ip\n    - MATCH,real-ip")
        };
        let mode_spec = dns_conversion_from_clash_yaml(&mode_yaml).unwrap().spec.unwrap();
        let mut mode_config = config.clone();
        mode_config["dns"] = build_dns_section(&mode_spec).unwrap().unwrap();
        let mut process = start(&core, root.path(), &mode_config);
        wait_ready(&mut process, inbound_ports[0], root.path());
        let cases = if mode == "whitelist" {
            vec![
                ("real.example", false),
                ("sub.real.example", true),
                ("elsewhere.test", true),
            ]
        } else {
            vec![
                ("real.example", true),
                ("sub.real.example", false),
                ("my-clock.test", false),
                ("elsewhere.test", true),
            ]
        };
        for (host, real) in cases {
            for (kind, loopback) in [
                (1, vec![127, 0, 0, 1]),
                (28, std::net::Ipv6Addr::LOCALHOST.octets().to_vec()),
            ] {
                let answer = query(dns_port, host, kind);
                if real {
                    assert_eq!(answer, loopback, "{mode}: {host}/{kind}");
                } else {
                    assert_ne!(answer, loopback, "{mode}: {host}/{kind}");
                }
            }
        }
        drop(process);
    }
    drop(proxy_threads);
    check_explicit_http_client(&core, root.path());
    eprintln!(
        "pinned 1.14.2: policy/missed nodes, IPv4/IPv6 literals, bootstrap isolation, 34 A/AAAA blacklist/whitelist/rule queries and direct remote SRS fetch passed"
    );
}

fn check_explicit_http_client(core: &std::path::Path, root: &std::path::Path) {
    let source = root.join("source.json");
    let binary = root.join("fixture.srs");
    std::fs::write(&source, br#"{"version":4,"rules":[{"domain":["example.test"]}]}"#).unwrap();
    let compiled = Command::new(core)
        .args(["rule-set", "compile"])
        .arg(&source)
        .arg("-o")
        .arg(&binary)
        .output()
        .unwrap();
    assert!(
        compiled.status.success(),
        "{}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    let payload = std::fs::read(binary).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    listener.set_nonblocking(true).unwrap();
    let fetched = Arc::new(AtomicBool::new(false));
    let observed = fetched.clone();
    let mut threads = Threads {
        stop: Arc::new(AtomicBool::new(false)),
        handles: Vec::new(),
    };
    let stopping = threads.stop.clone();
    threads.handles.push(thread::spawn(move || {
        while !stopping.load(Ordering::Relaxed) {
            if let Ok((mut stream, _)) = listener.accept() {
                stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
                let mut buf = [0u8; 2048];
                let len = stream.read(&mut buf).unwrap();
                assert!(String::from_utf8_lossy(&buf[..len]).starts_with("GET /fixture.srs"));
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    payload.len()
                )
                .unwrap();
                stream.write_all(&payload).unwrap();
                observed.store(true, Ordering::Relaxed);
            } else {
                thread::sleep(Duration::from_millis(5));
            }
        }
    }));
    let converted = super::super::convert::convert_rule_providers(&format!(
        "rule-providers:\n  fixture:\n    type: http\n    url: http://127.0.0.1:{port}/fixture.srs\n"
    ))
    .unwrap();
    assert_eq!(converted.rule_sets[0]["http_client"], json!({"engine":"go"}));
    assert!(converted.rule_sets[0].get("download_detour").is_none());
    let mixed = free_tcp_port();
    let cache = root.join("singbox-cache.db");
    super::super::config_gen::ensure_private_cache_file(&cache).unwrap();
    let config = json!({"log":{"level":"debug"},
        "inbounds":[{"type":"mixed", "listen":"127.0.0.1", "listen_port":mixed}],
        "outbounds":[{"type":"direct", "tag":"direct"}, {"type":"http", "tag":"unusable-default", "server":"127.0.0.1", "server_port":1}],
        "experimental":{"cache_file":super::super::config_gen::cache_file_json(&cache)},
        "route":{"final":"unusable-default", "rule_set":converted.rule_sets}});
    let mut process = start(core, root, &config);
    wait_ready(&mut process, mixed, root);
    assert!(
        fetched.load(Ordering::Relaxed),
        "explicit HTTP client must fetch directly despite unusable default outbound"
    );
    let log = std::fs::read_to_string(root.join("core.log")).unwrap();
    assert!(!log.contains("download_detour"), "{log}");
    assert!(!log.contains("implicit default HTTP client"), "{log}");
    drop(process);
    drop(threads);
    assert!(
        std::fs::metadata(&cache).unwrap().len() > 0,
        "cold run must persist rule-set cache"
    );
    // Same URL/config/cache, with the HTTP source now offline. Reaching
    // readiness proves the core loaded cached SRS rather than downloading.
    let mut warm = start(core, root, &config);
    wait_ready(&mut warm, mixed, root);
    drop(warm);
    eprintln!(
        "remote SRS: explicit Go client cold GET succeeded with unusable default outbound; same-cache offline warm restart became ready"
    );
}
