//! A farm CA, its certificates, and the three things an agent talks to, played by threads of the
//! test: a coordinator over mutual TLS, the frontend's operator API, and the helper's socket.

#![allow(dead_code)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use base64::Engine as _;
use kbf_ca::{Authority, FileSigner, Policy, PublicKey, SystemEntropy, UpdaterCert};
use kbf_host_install::contract::local::{Answer, LastView, PendingView, QueryView};
use kbf_types::HostRole;
use rcgen::{KeyPair, PublicKeyData};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::WebPkiClientVerifier;
use rustls::{RootCertStore, ServerConfig, ServerConnection, StreamOwned};

pub const OPERATOR_TOKEN: &str = "operator-token-0123456789abcdef0123456789";
pub const COMMIT_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
pub const COMMIT_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
pub const VB: &str = "0.1.7+gbbbbbbbbbbbb-3";

/// What happened, in order, across the three.
#[derive(Clone, Default)]
pub struct Events(pub Arc<Mutex<Vec<String>>>);

impl Events {
    pub fn push(&self, event: impl Into<String>) {
        self.0.lock().unwrap().push(event.into());
    }

    pub fn all(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        .try_into()
        .unwrap()
}

fn pem_of(label: &str, der: &[u8]) -> String {
    let body = base64::engine::general_purpose::STANDARD.encode(der);
    let lines: Vec<&str> = body
        .as_bytes()
        .chunks(64)
        .map(|c| std::str::from_utf8(c).unwrap())
        .collect();
    format!(
        "-----BEGIN {label}-----\n{}\n-----END {label}-----\n",
        lines.join("\n")
    )
}

pub struct Pki {
    pub ca: Authority,
}

pub struct Leaf {
    pub der: Vec<u8>,
    pub key: Vec<u8>,
}

fn key_pair() -> (KeyPair, PublicKey) {
    let pair = KeyPair::generate().unwrap();
    let public = PublicKey::from_spki(&pair.subject_public_key_info()).unwrap();
    (pair, public)
}

impl Pki {
    pub fn new() -> Self {
        let signer = Arc::new(FileSigner::generate().unwrap());
        Self {
            ca: Authority::create(signer, Policy::default(), now(), &mut SystemEntropy).unwrap(),
        }
    }

    pub fn root_pem(&self) -> String {
        self.ca.bundle().to_pem()
    }

    pub fn coordinator(&self) -> Leaf {
        let (pair, public) = key_pair();
        let issued = self
            .ca
            .issue_coordinator(
                &["localhost", "127.0.0.1"],
                &public,
                now(),
                &mut SystemEntropy,
            )
            .unwrap();
        Leaf {
            der: issued.der,
            key: pair.serialize_der(),
        }
    }

    pub fn updater(&self, name: &str, role: HostRole) -> Leaf {
        let (pair, public) = key_pair();
        let node_id = name.parse().unwrap();
        let cert = UpdaterCert {
            node_id: &node_id,
            host_role: role,
            key: &public,
        };
        let issued = self
            .ca
            .issue_updater(&cert, now(), &mut SystemEntropy)
            .unwrap();
        Leaf {
            der: issued.der,
            key: pair.serialize_der(),
        }
    }

    /// The updater's three files in `dir`.
    pub fn write_updater_files(&self, dir: &Path, leaf: &Leaf) {
        std::fs::write(dir.join("cert.pem"), pem_of("CERTIFICATE", &leaf.der)).unwrap();
        std::fs::write(dir.join("key.pem"), pem_of("PRIVATE KEY", &leaf.key)).unwrap();
        std::fs::set_permissions(
            dir.join("key.pem"),
            std::os::unix::fs::PermissionsExt::from_mode(0o600),
        )
        .unwrap();
        std::fs::write(dir.join("ca.pem"), self.root_pem()).unwrap();
    }
}

/// A coordinator: serves heartbeats over mutual TLS until `stop`, records each body, and answers
/// `reply` (a function of the heartbeat's JSON).
pub fn coordinator(
    pki: &Pki,
    events: &Events,
    beats: &Arc<Mutex<Vec<serde_json::Value>>>,
    reply: impl Fn(&serde_json::Value) -> String + Send + 'static,
    stop: &Arc<std::sync::atomic::AtomicBool>,
) -> (SocketAddr, JoinHandle<()>) {
    let leaf = pki.coordinator();
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut roots = RootCertStore::empty();
    roots
        .add(CertificateDer::from(pki.ca.root_der().to_vec()))
        .unwrap();
    let verifier = WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider.clone())
        .build()
        .unwrap();
    let mut config = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_client_cert_verifier(verifier)
        .with_single_cert(
            vec![CertificateDer::from(leaf.der.clone())],
            PrivateKeyDer::from_pem_slice(pem_of("PRIVATE KEY", &leaf.key).as_bytes()).unwrap(),
        )
        .unwrap();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    let config = Arc::new(config);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr = listener.local_addr().unwrap();
    let (events, beats, stop) = (events.clone(), Arc::clone(beats), Arc::clone(stop));
    let handle = std::thread::spawn(move || {
        while !stop.load(std::sync::atomic::Ordering::SeqCst) {
            let Ok((tcp, _)) = listener.accept() else {
                std::thread::sleep(Duration::from_millis(5));
                continue;
            };
            tcp.set_nonblocking(false).unwrap();
            tcp.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut tls =
                StreamOwned::new(ServerConnection::new(Arc::clone(&config)).unwrap(), tcp);
            let mut raw = Vec::new();
            let mut chunk = [0_u8; 4096];
            while let Ok(n) = tls.read(&mut chunk) {
                if n == 0 {
                    break;
                }
                raw.extend_from_slice(&chunk[..n]);
                if let Some(end) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&raw[..end]).to_ascii_lowercase();
                    let length = head
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length: "))
                        .and_then(|v| v.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    if raw.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            let Some(end) = raw.windows(4).position(|w| w == b"\r\n\r\n") else {
                continue;
            };
            let Ok(beat) = serde_json::from_slice::<serde_json::Value>(&raw[end + 4..]) else {
                continue;
            };
            events.push(format!("coordinator:beat updating={}", beat["updating"]));
            let body = reply(&beat);
            beats.lock().unwrap().push(beat);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
                 Connection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = tls.write_all(response.as_bytes());
            tls.conn.send_close_notify();
            let _ = tls.flush();
        }
    });
    (addr, handle)
}

/// The frontend's operator API on loopback: checks the bearer token, keeps the node's placement,
/// and says what the roll mode's adapters read.
pub fn operator_api(
    events: &Events,
    stop: &Arc<std::sync::atomic::AtomicBool>,
) -> (SocketAddr, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr = listener.local_addr().unwrap();
    let (events, stop) = (events.clone(), Arc::clone(stop));
    let handle = std::thread::spawn(move || {
        let mut placement = "serving";
        while !stop.load(std::sync::atomic::Ordering::SeqCst) {
            let Ok((tcp, _)) = listener.accept() else {
                std::thread::sleep(Duration::from_millis(5));
                continue;
            };
            tcp.set_nonblocking(false).unwrap();
            tcp.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut reader = BufReader::new(&tcp);
            let mut request_line = String::new();
            if reader.read_line(&mut request_line).is_err() {
                continue;
            }
            let mut authorized = false;
            let mut length = 0_usize;
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).is_err() || line == "\r\n" || line.is_empty() {
                    break;
                }
                let lower = line.to_ascii_lowercase();
                authorized |= lower.trim_end() == format!("authorization: bearer {OPERATOR_TOKEN}");
                if let Some(v) = lower.strip_prefix("content-length:") {
                    length = v.trim().parse().unwrap_or(0);
                }
            }
            let mut body = vec![0_u8; length];
            let _ = reader.read_exact(&mut body);
            let target = request_line
                .split_whitespace()
                .nth(1)
                .unwrap_or("")
                .to_owned();
            let method = request_line
                .split_whitespace()
                .next()
                .unwrap_or("")
                .to_owned();
            let (status, text) = if !authorized {
                (401, "{}".to_owned())
            } else if method == "POST" && target == "/v1/nodes/n1:drain" {
                events.push("api:drain");
                placement = "drained";
                (200, "{}".to_owned())
            } else if method == "POST" && target == "/v1/nodes/n1:uncordon" {
                events.push("api:uncordon");
                placement = "serving";
                (200, "{}".to_owned())
            } else if method == "GET" && target == "/v1/nodes/n1" {
                events.push(format!("api:node {placement}"));
                (
                    200,
                    format!(
                        r#"{{"connected":true,"placement":{{"state":"{placement}"}},"leases":[]}}"#
                    ),
                )
            } else {
                (404, "{}".to_owned())
            };
            let reason = if status == 200 { "OK" } else { "Error" };
            let response = format!(
                "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{text}",
                text.len()
            );
            let mut tcp = &tcp;
            let _ = tcp.write_all(response.as_bytes());
        }
    });
    (addr, handle)
}

/// The frontend's worker listener as an updater meets it: mutual TLS with the farm's frontend
/// certificate, and the node calls of node `n1` (and no other) answered as the real frontend
/// answers them, with the node's placement kept. Each call is an event: `api:drain`,
/// `api:uncordon`, `api:node <placement>`, `api:client <node id of the client certificate>`
/// before it, and `api:credential` if the request carried an `Authorization` header.
pub fn frontend_worker(
    pki: &Pki,
    events: &Events,
    stop: &Arc<std::sync::atomic::AtomicBool>,
) -> (SocketAddr, JoinHandle<()>) {
    let (pair, public) = key_pair();
    let issued = pki
        .ca
        .issue_frontend(
            &["localhost", "127.0.0.1"],
            &public,
            now(),
            &mut SystemEntropy,
        )
        .unwrap();
    let key = pair.serialize_der();
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut roots = RootCertStore::empty();
    roots
        .add(CertificateDer::from(pki.ca.root_der().to_vec()))
        .unwrap();
    let verifier = WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider.clone())
        .build()
        .unwrap();
    let mut config = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_client_cert_verifier(verifier)
        .with_single_cert(
            vec![CertificateDer::from(issued.der)],
            PrivateKeyDer::from_pem_slice(pem_of("PRIVATE KEY", &key).as_bytes()).unwrap(),
        )
        .unwrap();
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    let config = Arc::new(config);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr = listener.local_addr().unwrap();
    let (events, stop) = (events.clone(), Arc::clone(stop));
    let handle = std::thread::spawn(move || {
        let mut placement = "serving";
        while !stop.load(std::sync::atomic::Ordering::SeqCst) {
            let Ok((tcp, _)) = listener.accept() else {
                std::thread::sleep(Duration::from_millis(5));
                continue;
            };
            tcp.set_nonblocking(false).unwrap();
            tcp.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut tls =
                StreamOwned::new(ServerConnection::new(Arc::clone(&config)).unwrap(), tcp);
            let mut raw = Vec::new();
            let mut chunk = [0_u8; 4096];
            while let Ok(n) = tls.read(&mut chunk) {
                if n == 0 {
                    break;
                }
                raw.extend_from_slice(&chunk[..n]);
                if let Some(end) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&raw[..end]).to_ascii_lowercase();
                    let length = head
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length: "))
                        .and_then(|v| v.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    if raw.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            let Some(end) = raw.windows(4).position(|w| w == b"\r\n\r\n") else {
                continue;
            };
            let head = String::from_utf8_lossy(&raw[..end]).into_owned();
            let who = tls
                .conn
                .peer_certificates()
                .and_then(<[_]>::first)
                .and_then(|leaf| kbf_updater::identity::identity_of(leaf.as_ref()).ok())
                .map_or_else(|| "none".to_owned(), |id| id.node_id.to_string());
            events.push(format!("api:client {who}"));
            if head.to_ascii_lowercase().contains("\r\nauthorization:") {
                events.push("api:credential");
            }
            let mut words = head.lines().next().unwrap_or("").split_whitespace();
            let (method, target) = (words.next().unwrap_or(""), words.next().unwrap_or(""));
            let (status, text) = if who != "n1" {
                (403, "{}".to_owned())
            } else if method == "POST" && target == "/v1/nodes/n1:drain" {
                events.push("api:drain");
                placement = "drained";
                (200, "{}".to_owned())
            } else if method == "POST" && target == "/v1/nodes/n1:uncordon" {
                events.push("api:uncordon");
                placement = "serving";
                (200, "{}".to_owned())
            } else if method == "GET" && target == "/v1/nodes/n1" {
                events.push(format!("api:node {placement}"));
                (
                    200,
                    format!(
                        r#"{{"connected":true,"placement":{{"state":"{placement}"}},"leases":[]}}"#
                    ),
                )
            } else {
                (403, "{}".to_owned())
            };
            let reason = if status == 200 { "OK" } else { "Forbidden" };
            let response = format!(
                "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{text}",
                text.len()
            );
            let _ = tls.write_all(response.as_bytes());
            tls.conn.send_close_notify();
            let _ = tls.flush();
        }
    });
    (addr, handle)
}

/// The helper: the real protocol's answers (the types of `kbf-host-install`), for a host that
/// runs release A, is offered B, and goes through the A/B as a helper would.
pub fn helper(
    socket: &Path,
    events: &Events,
    stop: &Arc<std::sync::atomic::AtomicBool>,
) -> JoinHandle<()> {
    let listener = UnixListener::bind(socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let (events, stop) = (events.clone(), Arc::clone(stop));
    std::thread::spawn(move || {
        // (commit, version text, staged, pending, last)
        let mut commit = COMMIT_A.to_owned();
        let mut version: Option<String> = None;
        let mut staged = false;
        let mut pending = false;
        let mut last: Option<LastView> = None;
        while !stop.load(std::sync::atomic::Ordering::SeqCst) {
            let Ok((stream, _)) = listener.accept() else {
                std::thread::sleep(Duration::from_millis(5));
                continue;
            };
            stream.set_nonblocking(false).unwrap();
            let mut line = String::new();
            BufReader::new(&stream).read_line(&mut line).unwrap();
            let line = line.trim_end().to_owned();
            let verb = kbf_host_install::contract::local::request_verb(&line);
            let answer = match verb {
                Some("query") => Answer::Queried(QueryView {
                    commit: Some(commit.clone()),
                    version: version.clone(),
                    node_id: Some("n1".into()),
                    pending: pending.then(|| PendingView {
                        version: Some(VB.into()),
                        commit: commit.clone(),
                        deadline_unix: Some(4_000_000_000),
                    }),
                    last: last.clone(),
                    busy: false,
                }),
                Some("stage") => {
                    events.push(format!("helper:install {line}"));
                    // A fetch takes a while: the heartbeats go on meanwhile.
                    std::thread::sleep(Duration::from_millis(2500));
                    staged = true;
                    Answer::Staged {
                        version: VB.into(),
                        commit: COMMIT_B.into(),
                        current: false,
                    }
                }
                Some("switch") if staged => {
                    events.push("helper:switch");
                    commit = COMMIT_B.to_owned();
                    version = Some(VB.to_owned());
                    pending = true;
                    Answer::Switched {
                        version: VB.into(),
                        commit: COMMIT_B.into(),
                        deadline_unix: 4_000_000_000,
                    }
                }
                Some("confirm") if pending => {
                    events.push("helper:confirm");
                    pending = false;
                    last = Some(LastView {
                        target: VB.into(),
                        outcome: "succeeded",
                        reason: None,
                    });
                    Answer::Confirmed { version: VB.into() }
                }
                _ => Answer::Refused(
                    kbf_host_install::contract::local::Code::BadCommand,
                    String::new(),
                ),
            };
            let mut stream = &stream;
            let _ = writeln!(stream, "{}", answer.json_line());
        }
    })
}
