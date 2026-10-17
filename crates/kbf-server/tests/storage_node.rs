//! The storage node as a library (`kbf_server::storage_node`): its flags, its start line,
//! its shutdown, and the role check over mutual TLS with the certificate flags
//! (docs/design/architecture-v2.md, section 9).

use std::path::{Path, PathBuf};

use bytes::Bytes;
use clap::CommandFactory as _;
use kbf_meta::Role;
use kbf_server::storage_node::{StorageFlags, command, parse, start_storage};
use kbf_storage::service::{ROLE_URI_PREFIX, RemoteStorage};
use kbf_storage::{Storage as _, StorageError};
use kbf_types::{Digest, DigestFunction};
use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, DnType, ExtendedKeyUsagePurpose, IsCa,
    KeyPair, KeyUsagePurpose, SanType,
};
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Identity};

fn dir(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("storage-node")
        .join(name);
    std::fs::create_dir_all(&dir).expect("directory");
    dir
}

/// The flags of `kbf storage` over new stores in `dir`, and these more.
fn flags(dir: &Path, more: &[String]) -> Result<StorageFlags, clap::Error> {
    let mut argv = vec!["storage".to_owned()];
    if !more.iter().any(|m| m.starts_with("--storage-listen")) {
        argv.push("--storage-listen=127.0.0.1:0".to_owned());
    }
    argv.extend([
        format!("--cold-store=file://{}", dir.join("cold").display()),
        format!("--meta-dir={}", dir.join("meta").display()),
    ]);
    argv.extend_from_slice(more);
    parse(&command().try_get_matches_from(argv)?)
}

fn never() -> std::io::Result<std::future::Pending<()>> {
    Ok(std::future::pending())
}

/// Catches a start line that lacks what a supervisor reads (the version, the address it
/// bound, the log id), or one that carries a warning: a plain-text listener off loopback
/// is refused now, so no start line has one.
#[tokio::test]
async fn the_start_line_names_the_address_and_the_log() {
    let local = flags(&dir("line-local"), &[]).expect("flags");
    let node = start_storage(&local, never).await.expect("starts");
    let line = node.start_line().to_owned();
    let want = format!(
        "kbf-storage {} storage={} log=",
        kbf_server::SERVER_VERSION,
        node.addr
    );
    assert!(
        line.starts_with(&want) && !line.contains("warning"),
        "{line}"
    );
}

/// Catches a plain-text listener that is allowed off loopback (it admits Write and
/// ActionCommit from anyone), and a refusal that comes late or is not one line: every
/// address that is not loopback (a wildcard, IPv4 and IPv6, a LAN address, the IPv6
/// forms) is refused by `listener()` and by `start_storage` before any store is opened
/// (the meta directory is never created), with the address and the three flags in the
/// message; and the same addresses start with the TLS flags, and loopback starts
/// without them (IPv4, IPv6, and IPv4-mapped).
#[tokio::test]
async fn plain_text_is_for_loopback_only() {
    let d = dir(&format!("plain-off-loopback-{}", std::process::id()));
    for addr in ["0.0.0.0:0", "[::]:0", "192.0.2.10:0", "[2001:db8::1]:0"] {
        let f = flags(&d, &[format!("--storage-listen={addr}")]).expect("parses");
        let refused = f.listener().expect_err(addr);
        let said = refused.to_string();
        assert!(!said.contains('\n'), "{said}");
        for want in [
            addr,
            "--storage-tls-cert",
            "--storage-tls-key",
            "--storage-client-ca",
        ] {
            assert!(said.contains(want), "{addr}: {said}");
        }
        let started = start_storage(&f, never).await;
        assert_eq!(started.err().map(|e| e.to_string()), Some(said), "{addr}");
    }
    assert!(!d.join("meta").exists(), "a refused start opened a store");
    for addr in ["127.0.0.1:0", "[::1]:0", "[::ffff:127.0.0.1]:0"] {
        let f = flags(&d, &[format!("--storage-listen={addr}")]).expect("parses");
        assert!(f.listener().is_ok(), "{addr}");
    }
    let pki = Pki::new(&d);
    let mut tls = pki.server_flags();
    tls.push("--storage-listen=0.0.0.0:0".to_owned());
    let wide = flags(&d, &tls).expect("parses");
    let listener = wide.listener().expect("TLS allows any address");
    assert!(listener.tls.is_some());
}

/// Catches a node that serves past its shutdown future, or never starts serving.
#[tokio::test]
async fn it_serves_until_the_shutdown_future_completes() {
    let f = flags(&dir("shutdown"), &[]).expect("flags");
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let node = start_storage(&f, || {
        Ok(async move {
            let _ = stopped.await;
        })
    })
    .await
    .expect("starts");
    let serving = tokio::spawn(node.serve());
    stop.send(()).expect("stop");
    serving.await.expect("task").expect("serves to the end");
}

/// Catches flags that do not make a node: the three certificate flags must come
/// together, a segment upload cap over the write bound is refused as the server refuses
/// it, a certificate file that is not there is an error (not plain text), and a start
/// with no `--cold-store` is refused.
#[tokio::test]
async fn flags_that_do_not_make_a_node_are_refused() {
    let d = dir("refused");
    for partial in [
        vec!["--storage-tls-cert=c".to_owned()],
        vec![
            "--storage-tls-cert=c".to_owned(),
            "--storage-tls-key=k".to_owned(),
        ],
        vec!["--storage-client-ca=a".to_owned()],
    ] {
        assert!(flags(&d, &partial).is_err(), "{partial:?}");
    }
    let over = flags(
        &d,
        &[
            "--segment-upload-max-bytes=1074790400".to_owned(),
            "--write-bytes-in-flight=1073741824".to_owned(),
        ],
    )
    .expect("parses");
    assert!(over.listener().is_err());
    let missing: Vec<String> = ["tls-cert", "tls-key", "client-ca"]
        .iter()
        .map(|n| format!("--storage-{n}={}", d.join(format!("no-{n}.pem")).display()))
        .collect();
    let f = flags(&d, &missing).expect("parses");
    assert!(f.listener().is_err());
    let none = parse(
        &command()
            .try_get_matches_from(["storage"])
            .expect("matches"),
    )
    .expect("flags");
    assert!(start_storage(&none, never).await.is_err());
}

/// Catches the command line growing a flag the node must not have: nothing about a state
/// store, REAPI, workers or the operator API is shown in `--help`.
#[test]
fn the_help_shows_only_what_a_storage_node_takes() {
    let help = command().render_long_help().to_string();
    for gone in [
        "state",
        "--reapi",
        "--worker",
        "--api-",
        "--heartbeat",
        "memory-ladder",
        "input-tree",
    ] {
        assert!(!help.contains(gone), "{gone} in:\n{help}");
    }
    for shown in [
        "--cold-store",
        "--hot-store",
        "--storage-listen",
        "--storage-tls-cert",
        "--write-bytes-in-flight",
        "--cpu-threads",
    ] {
        assert!(help.contains(shown), "{shown}");
    }
    // Every id the allowlist names is a flag of kbf-server.
    let server = kbf_server::Args::command();
    for id in kbf_server::storage_node::ALLOWED {
        assert!(server.get_arguments().any(|a| a.get_id() == *id), "{id}");
    }
}

/// Catches limits that drifted from the front's, which the service must match: a full
/// batch has to fit, and a Write is streamed from the size the front streams from.
#[test]
fn the_service_limits_are_the_fronts() {
    assert_eq!(
        kbf_storage::service::MAX_MESSAGE_BYTES,
        kbf_front::MAX_MESSAGE_BYTES
    );
    assert_eq!(
        kbf_storage::service::STREAM_FROM_BYTES,
        kbf_front::MAX_UNSTAGED_BLOB_BYTES
    );
}

/// A CA, and certificates issued from it.
struct Pki {
    ca: CertifiedIssuer<'static, KeyPair>,
    dir: PathBuf,
}

impl Pki {
    fn new(dir: &Path) -> Self {
        let mut ca = CertificateParams::new(Vec::<String>::new()).expect("CA params");
        ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca.distinguished_name
            .push(DnType::CommonName, "kbf test CA");
        ca.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::DigitalSignature,
        ];
        let ca = CertifiedIssuer::self_signed(ca, KeyPair::generate().expect("key")).expect("CA");
        Self {
            ca,
            dir: dir.to_path_buf(),
        }
    }

    /// The three certificate flags, for a server certificate for `localhost`.
    fn server_flags(&self) -> Vec<String> {
        let mut params = CertificateParams::new(vec!["localhost".to_owned()]).expect("params");
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        let key = KeyPair::generate().expect("key");
        let cert = params.signed_by(&key, &self.ca).expect("sign");
        let write = |name: &str, text: String| {
            let path = self.dir.join(name);
            std::fs::write(&path, text).expect("write");
            path.display().to_string()
        };
        vec![
            format!("--storage-tls-cert={}", write("server.pem", cert.pem())),
            format!(
                "--storage-tls-key={}",
                write("server.key", key.serialize_pem())
            ),
            format!("--storage-client-ca={}", write("ca.pem", self.ca.pem())),
        ]
    }

    async fn client(&self, addr: std::net::SocketAddr, role: &str) -> RemoteStorage {
        let mut params = CertificateParams::new(Vec::<String>::new()).expect("params");
        params.subject_alt_names = vec![SanType::URI(
            format!("{ROLE_URI_PREFIX}{role}").try_into().expect("IA5"),
        )];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        let key = KeyPair::generate().expect("key");
        let cert = params.signed_by(&key, &self.ca).expect("sign");
        let tls = ClientTlsConfig::new()
            .ca_certificate(Certificate::from_pem(self.ca.pem()))
            .domain_name("localhost")
            .identity(Identity::from_pem(cert.pem(), key.serialize_pem()));
        let channel = Channel::from_shared(format!("https://{addr}"))
            .expect("endpoint")
            .tls_config(tls)
            .expect("TLS")
            .connect()
            .await
            .expect("connect");
        RemoteStorage::new(channel, 0)
    }
}

/// Catches a node started with the certificate flags that does not check the role in
/// the caller's certificate (plain admission on a TLS listener), or one that refuses
/// the roles that belong: a daemon stores and fetches blobs and prepares an action
/// result but may not read the action cache; a frontend reads it; a certificate whose
/// role is not one is refused everywhere.
#[tokio::test]
async fn a_node_started_with_certificates_checks_each_callers_role() {
    let d = dir("mtls");
    let pki = Pki::new(&d);
    let f = flags(&d, &pki.server_flags()).expect("flags");
    let node = start_storage(&f, never).await.expect("starts");
    let addr = node.addr;
    tokio::spawn(node.serve());
    let action = Digest::new(DigestFunction::Sha256, [4; 32], 1);

    let daemon = pki.client(addr, "daemon").await;
    let blob = Bytes::from_static(b"a daemon's output");
    let digest = daemon.put(blob.clone()).await.expect("a daemon writes");
    assert_eq!(daemon.read_blob(&digest).await.expect("reads"), blob);
    let refused = daemon
        .action_result(&action)
        .await
        .expect_err("a daemon never reads the action cache");
    assert!(matches!(refused, StorageError::Invalid(m) if m.contains("daemon")));

    let frontend = pki.client(addr, "frontend").await;
    assert!(
        frontend
            .action_result(&action)
            .await
            .expect("a frontend reads")
            .is_none()
    );
    let record = kbf_meta::ActionRecord {
        result: digest,
        closure: kbf_meta::Closure::new(),
    };
    frontend
        .commit_action_record(Role::Daemon, action, record)
        .await
        .expect("a frontend relays a result");

    let stranger = pki.client(addr, "admin").await;
    assert!(matches!(
        stranger.find_missing(&[action]).await,
        Err(StorageError::Invalid(_))
    ));
    assert!(matches!(
        stranger.put(Bytes::from_static(b"x")).await,
        Err(StorageError::Invalid(_))
    ));
}
