//! The storage node's certificate, key and client CA replaced on disk while it runs: the
//! flags (`--tls-reload-interval-secs`, `--storage-tls-*`) through
//! `StorageFlags::listener`, the real listener, and tonic clients that do real TLS
//! handshakes. The reload engine's own cases are in `kbf-tls`; these show the storage
//! node is wired to it, and that the role check still reads the caller's certificate.

use std::future::pending;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use kbf_front::{Cache, CacheStorage, SegmentsConfig};
use kbf_server::storage_node::{command, parse};
use kbf_server::{BoundStorage, bind_storage};
use kbf_storage::Storage as _;
use kbf_storage::service::{ROLE_URI_PREFIX, RemoteStorage};
use kbf_tls::TlsSource;
use kbf_types::{Digest, DigestFunction};
use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, DnType, ExtendedKeyUsagePurpose, IsCa,
    KeyPair, KeyUsagePurpose, SanType,
};
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Identity};

struct Ca(CertifiedIssuer<'static, KeyPair>);

struct Pair {
    cert: String,
    key: String,
}

fn ca(name: &str) -> Ca {
    let mut params = CertificateParams::new(Vec::<String>::new()).expect("CA params");
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.distinguished_name.push(DnType::CommonName, name);
    params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    Ca(CertifiedIssuer::self_signed(params, KeyPair::generate().expect("key")).expect("CA"))
}

impl Ca {
    fn pem(&self) -> String {
        self.0.pem()
    }

    /// A server certificate for `localhost`.
    fn server(&self) -> Pair {
        let mut params = CertificateParams::new(vec!["localhost".to_owned()]).expect("params");
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        self.sign(params)
    }

    /// A client certificate that names `role`.
    fn client(&self, role: &str) -> Pair {
        let mut params = CertificateParams::new(Vec::<String>::new()).expect("params");
        params.subject_alt_names = vec![SanType::URI(
            format!("{ROLE_URI_PREFIX}{role}").try_into().expect("IA5"),
        )];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        self.sign(params)
    }

    fn sign(&self, params: CertificateParams) -> Pair {
        let key = KeyPair::generate().expect("key");
        let cert = params.signed_by(&key, &self.0).expect("sign");
        Pair {
            cert: cert.pem(),
            key: key.serialize_pem(),
        }
    }
}

fn dir(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("storage-tls-reload-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create the directory");
    dir
}

/// Replaces `dir/file` whole, as a renewal tool does.
fn put(dir: &Path, file: &str, contents: &str) {
    let aside = dir.join(format!("{file}.new"));
    std::fs::write(&aside, contents).expect("write");
    std::fs::rename(&aside, dir.join(file)).expect("rename");
}

fn flag(dir: &Path, name: &str, file: &str) -> String {
    format!("--{name}={}", dir.join(file).display())
}

/// The flags of `kbf storage` for a node whose TLS files are in `dir`.
fn argv(dir: &Path, interval: &str) -> Vec<String> {
    vec![
        "storage".to_owned(),
        "--storage-listen=127.0.0.1:0".to_owned(),
        format!("--tls-reload-interval-secs={interval}"),
        flag(dir, "storage-tls-cert", "server.pem"),
        flag(dir, "storage-tls-key", "server.key"),
        flag(dir, "storage-client-ca", "ca.pem"),
    ]
}

/// A running storage node over the listener its flags make.
struct Node {
    addr: SocketAddr,
    tls: TlsSource,
}

impl Node {
    fn start(dir: &Path, server: &Pair, client_ca: &Ca) -> Self {
        put(dir, "server.pem", &server.cert);
        put(dir, "server.key", &server.key);
        put(dir, "ca.pem", &client_ca.pem());
        let flags = parse(
            &command()
                .try_get_matches_from(argv(dir, "1"))
                .expect("flags"),
        )
        .expect("taken");
        let listener = flags.listener().expect("listener");
        let tls = listener.tls.clone().expect("the node has TLS");
        let storage: Arc<dyn kbf_storage::Storage> = Arc::new(CacheStorage::new(
            Arc::new(Cache::memory()),
            SegmentsConfig::DEFAULT,
        ));
        let BoundStorage { addr, serving } =
            bind_storage(storage, listener, pending()).expect("bind");
        tokio::spawn(async move { serving.await.expect("serve") });
        Self { addr, tls }
    }

    /// A client that trusts only `trust`'s certificates and presents `identity`.
    async fn client(&self, trust: &Ca, identity: &Pair) -> Option<RemoteStorage> {
        let tls = ClientTlsConfig::new()
            .ca_certificate(Certificate::from_pem(trust.pem()))
            .domain_name("localhost")
            .identity(Identity::from_pem(&identity.cert, &identity.key));
        let channel = Channel::from_shared(format!("https://{}", self.addr))
            .expect("endpoint")
            .tls_config(tls)
            .expect("TLS")
            .connect()
            .await
            .ok()?;
        Some(RemoteStorage::new(channel, 0))
    }

    /// Whether a new client is answered: the server's certificate verified by `trust`,
    /// and `identity` admitted to ask `FindMissing`.
    async fn answers(&self, trust: &Ca, identity: &Pair) -> bool {
        match self.client(trust, identity).await {
            Some(client) => answered(&client).await,
            None => false,
        }
    }

    async fn until_reloads(&self, what: &str, done: impl Fn(&kbf_tls::Reloads) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while !done(&self.tls.reloads()) {
            assert!(Instant::now() < deadline, "timed out: {what}");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

async fn answered(client: &RemoteStorage) -> bool {
    let digest = Digest::new(DigestFunction::Sha256, [4; 32], 1);
    client.find_missing(&[digest]).await.is_ok()
}

/// A certificate renewed by replacing the files: a connection open at the time keeps
/// being answered, a client that trusts only the new issuer is answered once the files
/// are looked at again, the old issuer's client is not, and the role check still reads
/// the caller's certificate through the reloading acceptor (a certificate naming no
/// role of the node is refused, as before).
///
/// Kills: a storage listener that reads its certificate once (the new issuer's client
/// is never answered); a renewal that ends the open connections (the held client fails);
/// an admission that reads only tonic's own TLS state, which the reloading acceptor does
/// not fill (every call is refused, so the first assertion fails).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_renewed_storage_certificate_is_served_without_a_restart() {
    let (old_issuer, new_issuer, clients) = (ca("old issuer"), ca("new issuer"), ca("clients"));
    let d = dir("renewal");
    let node = Node::start(&d, &old_issuer.server(), &clients);
    let frontend = clients.client("frontend");

    let held = node.client(&old_issuer, &frontend).await.expect("connects");
    assert!(answered(&held).await, "a frontend is admitted");
    assert!(
        !node.answers(&old_issuer, &clients.client("admin")).await,
        "a role the node does not know is refused"
    );
    assert!(!node.answers(&new_issuer, &frontend).await);

    let renewed = new_issuer.server();
    put(&d, "server.pem", &renewed.cert);
    put(&d, "server.key", &renewed.key);
    node.until_reloads("the renewed pair applied", |r| r.applied == 1)
        .await;

    assert!(node.answers(&new_issuer, &frontend).await, "new handshake");
    assert!(!node.answers(&old_issuer, &frontend).await);
    assert!(answered(&held).await, "the open connection survives");
    assert_eq!(node.tls.reloads().rejected, 0);
}

/// Bad states on disk during a renewal keep the working pair serving and are counted:
/// the new certificate beside the old key (a pair caught between its two writes), a
/// certificate file that is not PEM, and the new certificate beside the old key again;
/// then the complete pair is applied.
///
/// Kills: a listener that takes a mismatched pair (the old issuer's client would stop
/// being answered, or the handshakes would fail); a swap before validation (the same);
/// a bad state that is not counted (`rejected`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pair_caught_mid_renewal_or_garbage_keeps_the_working_certificate() {
    let (old_issuer, new_issuer, clients) = (ca("old issuer"), ca("new issuer"), ca("clients"));
    let d = dir("bad-states");
    let node = Node::start(&d, &old_issuer.server(), &clients);
    let frontend = clients.client("frontend");
    let renewed = new_issuer.server();
    let held = node.client(&old_issuer, &frontend).await.expect("connects");

    // The certificate is written first, the key not yet.
    put(&d, "server.pem", &renewed.cert);
    node.until_reloads("the half-written pair refused", |r| r.rejected == 1)
        .await;
    put(&d, "server.pem", "this is not a certificate\n");
    node.until_reloads("the garbage refused", |r| r.rejected == 2)
        .await;
    put(&d, "server.pem", &renewed.cert);
    node.until_reloads("the half-written pair refused again", |r| r.rejected == 3)
        .await;

    assert!(node.answers(&old_issuer, &frontend).await, "still the old");
    assert!(!node.answers(&new_issuer, &frontend).await);
    assert!(answered(&held).await);
    assert_eq!(node.tls.reloads().applied, 0);

    // The key arrives: the pair is whole and is picked up.
    put(&d, "server.key", &renewed.key);
    node.until_reloads("the complete pair applied", |r| r.applied == 1)
        .await;
    assert!(node.answers(&new_issuer, &frontend).await);
    assert!(!node.answers(&old_issuer, &frontend).await);
    assert!(answered(&held).await);
    assert_eq!(node.tls.reloads().rejected, 3);
}

/// The client CA bundle is reloaded too: a client certificate from a CA that is added
/// to the file is admitted to new connections, one from the CA that is taken out is
/// refused to new connections, and a connection already open under the old bundle keeps
/// being answered (a connection keeps the roots it handshook under).
///
/// Kills: a trust bundle read once at start; a reload that swaps the certificate but
/// keeps the first verifier; a reload that ends connections open under the old bundle.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_client_ca_bundle_is_reloaded() {
    let (issuer, first, second) = (ca("issuer"), ca("first clients"), ca("second clients"));
    let d = dir("client-ca");
    let node = Node::start(&d, &issuer.server(), &first);
    let (from_first, from_second) = (first.client("frontend"), second.client("frontend"));

    let held = node.client(&issuer, &from_first).await.expect("connects");
    assert!(answered(&held).await);
    assert!(!node.answers(&issuer, &from_second).await);

    put(&d, "ca.pem", &second.pem());
    node.until_reloads("the new bundle applied", |r| r.applied == 1)
        .await;

    assert!(node.answers(&issuer, &from_second).await, "new trust root");
    assert!(!node.answers(&issuer, &from_first).await, "old trust root");
    assert!(answered(&held).await, "the open connection survives");

    // A bundle with nothing usable in it is refused, and the second CA keeps serving.
    put(&d, "ca.pem", "");
    node.until_reloads("the empty bundle refused", |r| r.rejected == 1)
        .await;
    assert!(node.answers(&issuer, &from_second).await);
}

/// The flags: the files must make a configuration at start (a certificate that is not
/// PEM refuses the node with the TLS error, not a node that serves nothing), the
/// interval is bounded as the other listeners' is, and a plain-text loopback node has no
/// TLS source to look at.
///
/// Kills: a start that defers a bad file to the first look; an unbounded interval (zero
/// would spin the poller); a TLS source on a plain listener.
#[tokio::test]
async fn the_node_needs_good_files_at_start_and_a_bounded_interval() {
    let (issuer, clients) = (ca("issuer"), ca("clients"));
    let d = dir("flags");
    put(&d, "server.pem", "not a certificate");
    put(&d, "server.key", &issuer.server().key);
    put(&d, "ca.pem", &clients.pem());
    let flags = parse(
        &command()
            .try_get_matches_from(argv(&d, "5"))
            .expect("flags"),
    )
    .expect("taken");
    let refused = flags.listener().expect_err("a bad certificate file");
    assert!(refused.to_string().starts_with("TLS: "), "{refused}");

    for bad in ["0", "3601", "soon"] {
        assert!(
            command().try_get_matches_from(argv(&d, bad)).is_err(),
            "{bad}"
        );
    }

    let plain = parse(
        &command()
            .try_get_matches_from(["storage", "--storage-listen=127.0.0.1:0"])
            .expect("flags"),
    )
    .expect("taken");
    assert!(plain.listener().expect("loopback").tls.is_none());
}
