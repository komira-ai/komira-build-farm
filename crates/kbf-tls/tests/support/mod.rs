//! A throwaway CA and leaf certificates, a server that echoes over `Incoming`, and a
//! TLS client that reports which certificate it was served.

#![allow(dead_code)]

use std::io::IoSlice;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt as _;
use kbf_tls::{ConnInfo, Incoming, TlsFiles, TlsSource};
use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, DnType, ExtendedKeyUsagePurpose, IsCa,
    KeyPair, KeyUsagePurpose,
};
use rustls::RootCertStore;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;
use tonic::transport::server::TcpIncoming;

/// How often the sources under test look at their files.
pub const INTERVAL: Duration = Duration::from_millis(20);

pub struct Ca {
    issuer: CertifiedIssuer<'static, KeyPair>,
    pub pem: String,
}

pub struct Leaf {
    pub cert: String,
    pub key: String,
    pub der: Vec<u8>,
}

pub fn ca(name: &str) -> Ca {
    let mut params = CertificateParams::new(Vec::<String>::new()).expect("CA params");
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.distinguished_name.push(DnType::CommonName, name);
    params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    let issuer = CertifiedIssuer::self_signed(params, KeyPair::generate().expect("CA key"))
        .expect("CA certificate");
    let pem = issuer.pem();
    Ca { issuer, pem }
}

/// A leaf of `ca` for `name`, valid now or, with `expired`, valid only for one day that
/// is long over.
pub fn leaf(ca: &Ca, name: &str, usage: ExtendedKeyUsagePurpose, expired: bool) -> Leaf {
    let mut params = CertificateParams::new(vec![name.to_owned()]).expect("leaf params");
    params.distinguished_name.push(DnType::CommonName, name);
    params.extended_key_usages = vec![usage];
    if expired {
        params.not_before = rcgen::date_time_ymd(2026, 9, 2);
        params.not_after = rcgen::date_time_ymd(2026, 9, 3);
    }
    let key = KeyPair::generate().expect("leaf key");
    let cert = params.signed_by(&key, &ca.issuer).expect("sign leaf");
    Leaf {
        cert: cert.pem(),
        key: key.serialize_pem(),
        der: cert.der().to_vec(),
    }
}

pub fn server_leaf(ca: &Ca) -> Leaf {
    leaf(ca, "localhost", ExtendedKeyUsagePurpose::ServerAuth, false)
}

pub fn client_leaf(ca: &Ca, name: &str) -> Leaf {
    leaf(ca, name, ExtendedKeyUsagePurpose::ClientAuth, false)
}

/// A directory of this test's own under the target directory, empty.
pub fn dir(test: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("kbf-tls-{test}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create the test directory");
    dir
}

/// Replaces `path` whole (write aside, rename), as a renewal tool does, so a look never
/// sees half a file.
pub fn put(path: &Path, contents: &str) {
    let aside = path.with_extension("new");
    std::fs::write(&aside, contents).expect("write aside");
    std::fs::rename(&aside, path).expect("rename into place");
}

pub struct Files {
    pub files: TlsFiles,
}

impl Files {
    /// The server's files in `dir`: `leaf` as the identity, and `ca_pem` as the client CA
    /// if there is one.
    pub fn write(dir: &Path, leaf: &Leaf, ca_pem: Option<&str>) -> Self {
        let files = TlsFiles {
            cert: dir.join("server.pem"),
            key: dir.join("server.key"),
            client_ca: ca_pem.map(|_| dir.join("client-ca.pem")),
        };
        put(&files.cert, &leaf.cert);
        put(&files.key, &leaf.key);
        if let (Some(path), Some(pem)) = (&files.client_ca, ca_pem) {
            put(path, pem);
        }
        Self { files }
    }

    pub fn source(&self) -> TlsSource {
        TlsSource::files("test", self.files.clone(), INTERVAL).expect("a source")
    }

    pub fn install(&self, leaf: &Leaf) {
        put(&self.files.cert, &leaf.cert);
        put(&self.files.key, &leaf.key);
    }
}

/// A server that echoes every connection of an [`Incoming`], and sends each one's
/// [`ConnInfo`] to `infos`.
pub struct Echo {
    pub addr: SocketAddr,
    pub infos: mpsc::UnboundedReceiver<ConnInfo>,
    task: JoinHandle<()>,
}

impl Drop for Echo {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub fn tcp() -> TcpIncoming {
    TcpIncoming::bind("127.0.0.1:0".parse().expect("address")).expect("bind")
}

pub fn echo(tcp: TcpIncoming, tls: Option<TlsSource>) -> Echo {
    echo_with(tcp, tls, None)
}

pub fn echo_with(tcp: TcpIncoming, tls: Option<TlsSource>, handshake: Option<Duration>) -> Echo {
    use tonic::transport::server::Connected as _;
    let addr = tcp.local_addr().expect("local address");
    let mut incoming = Incoming::new(tcp, tls);
    if let Some(handshake) = handshake {
        incoming = incoming.with_handshake_timeout(handshake);
    }
    let (send, infos) = mpsc::unbounded_channel();
    let task = tokio::spawn(async move {
        while let Some(Ok(mut conn)) = incoming.next().await {
            let _ = send.send(conn.connect_info());
            tokio::spawn(async move {
                let mut buf = [0u8; 64];
                loop {
                    let n = match conn.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => n,
                    };
                    // Out in two parts, to run the connection's plain write and then
                    // its vectored one.
                    let (head, tail) = buf[..n].split_at(n / 2);
                    conn.write_all(head).await.expect("write");
                    let wrote = conn
                        .write_vectored(&[IoSlice::new(tail)])
                        .await
                        .expect("write vectored");
                    conn.write_all(&tail[wrote..]).await.expect("write rest");
                    conn.flush().await.expect("flush");
                }
            });
        }
    });
    Echo { addr, infos, task }
}

/// A TLS connection to `addr` that trusts the CAs in `trust_pem` and presents `identity`
/// if given. With TLS 1.3 a refused client certificate shows at the first read, not here.
pub async fn connect(
    addr: SocketAddr,
    trust_pem: &str,
    identity: Option<&Leaf>,
) -> std::io::Result<TlsStream<TcpStream>> {
    let mut roots = RootCertStore::empty();
    for der in CertificateDer::pem_slice_iter(trust_pem.as_bytes()) {
        roots.add(der.expect("CA PEM")).expect("add root");
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("protocol versions")
        .with_root_certificates(roots);
    let config = match identity {
        Some(identity) => builder
            .with_client_auth_cert(
                CertificateDer::pem_slice_iter(identity.cert.as_bytes())
                    .collect::<Result<_, _>>()
                    .expect("client chain"),
                PrivateKeyDer::from_pem_slice(identity.key.as_bytes()).expect("client key"),
            )
            .expect("client identity"),
        None => builder.with_no_client_auth(),
    };
    let tcp = TcpStream::connect(addr).await?;
    let name = ServerName::try_from("localhost").expect("server name");
    TlsConnector::from(Arc::new(config))
        .connect(name, tcp)
        .await
}

/// One round trip: whether the server echoed.
pub async fn ping(conn: &mut TlsStream<TcpStream>) -> std::io::Result<()> {
    conn.write_all(b"ping").await?;
    conn.flush().await?;
    let mut back = [0u8; 4];
    conn.read_exact(&mut back).await?;
    assert_eq!(&back, b"ping");
    Ok(())
}

/// The DER of the leaf the server presented on `conn`.
pub fn served(conn: &TlsStream<TcpStream>) -> Vec<u8> {
    conn.get_ref()
        .1
        .peer_certificates()
        .expect("the server presented a certificate")[0]
        .as_ref()
        .to_vec()
}

/// Waits until `condition` holds, for at most twenty seconds.
pub async fn until(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while !condition() {
        assert!(std::time::Instant::now() < deadline, "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}
