//! A registry in this process: HTTP/1.1 over a loopback socket (or TLS), serving the
//! manifests and blobs a test publishes, with the behaviours the client must survive:
//! a token challenge, a redirect of blobs to another path, tampered bytes, stalls.
//!
//! Shared with the container driver's tests by `#[path]`, so it depends only on the
//! libraries both dev-dependency lists name.

#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use sha2::{Digest as _, Sha256};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpListener;

pub fn sha256(bytes: &[u8]) -> String {
    format!("sha256:{}", hex::encode(Sha256::digest(bytes)))
}

/// The architecture name an image config uses for this machine.
pub fn this_arch() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "arm64",
        _ => "amd64",
    }
}

/// An entry of a layer tar.
pub enum Item<'a> {
    File {
        path: &'a str,
        data: &'a [u8],
        mode: u32,
    },
    Dir {
        path: &'a str,
        mode: u32,
    },
    Symlink {
        path: &'a str,
        target: &'a str,
    },
    Hardlink {
        path: &'a str,
        target: &'a str,
    },
    Special {
        path: &'a str,
        kind: tar::EntryType,
    },
}

/// The owner every entry of a test tar names: the running user's, which an unprivileged
/// unpack may give files to.
pub fn my_ids() -> (u32, u32) {
    (
        rustix::process::geteuid().as_raw(),
        rustix::process::getegid().as_raw(),
    )
}

fn mine() -> (u64, u64) {
    let (uid, gid) = my_ids();
    (u64::from(uid), u64::from(gid))
}

/// A tar of `items`, owned by `ids`.
pub fn tar_of(items: &[Item<'_>], ids: (u64, u64)) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    for item in items {
        let mut header = tar::Header::new_gnu();
        header.set_uid(ids.0);
        header.set_gid(ids.1);
        header.set_mtime(1_000_000);
        match item {
            Item::File { path, data, mode } => {
                header.set_entry_type(tar::EntryType::Regular);
                header.set_mode(*mode);
                header.set_size(data.len() as u64);
                put_name(&mut header, path);
                header.set_cksum();
                builder.append(&header, *data).expect("append");
            }
            Item::Dir { path, mode } => {
                header.set_entry_type(tar::EntryType::Directory);
                header.set_mode(*mode);
                header.set_size(0);
                put_name(&mut header, path);
                header.set_cksum();
                builder.append(&header, std::io::empty()).expect("append");
            }
            Item::Symlink { path, target } | Item::Hardlink { path, target } => {
                let kind = if matches!(item, Item::Symlink { .. }) {
                    tar::EntryType::Symlink
                } else {
                    tar::EntryType::Link
                };
                header.set_entry_type(kind);
                header.set_mode(0o777);
                header.set_size(0);
                put_name(&mut header, path);
                // An empty name is left as zeros: the builder refuses to write one.
                if !target.is_empty() {
                    header.set_link_name(target).expect("link name");
                }
                header.set_cksum();
                builder.append(&header, std::io::empty()).expect("append");
            }
            Item::Special { path, kind } => {
                header.set_entry_type(*kind);
                header.set_mode(0o600);
                header.set_size(0);
                put_name(&mut header, path);
                header.set_cksum();
                builder.append(&header, std::io::empty()).expect("append");
            }
        }
    }
    builder.into_inner().expect("finish")
}

/// Writes `path` into the header's name field as given, `..` included (which the
/// builder's own setter refuses): a hostile tar is one this test must be able to make.
fn put_name(header: &mut tar::Header, path: &str) {
    let name = &mut header.as_old_mut().name;
    name.fill(0);
    name[..path.len()].copy_from_slice(path.as_bytes());
}

pub fn gzip(bytes: &[u8]) -> Vec<u8> {
    use std::io::Write as _;
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    encoder.write_all(bytes).expect("gzip");
    encoder.finish().expect("gzip end")
}

/// A layer as published: the blob's bytes and its media type.
pub struct Blob {
    pub bytes: Vec<u8>,
    pub media_type: &'static str,
}

impl Blob {
    pub fn gzip_tar(items: &[Item<'_>]) -> Self {
        Self {
            bytes: gzip(&tar_of(items, mine())),
            media_type: "application/vnd.oci.image.layer.v1.tar+gzip",
        }
    }

    pub fn plain_tar(items: &[Item<'_>]) -> Self {
        Self {
            bytes: tar_of(items, mine()),
            media_type: "application/vnd.oci.image.layer.v1.tar",
        }
    }
}

/// An image: manifest bytes and every blob it names.
pub struct Image {
    pub manifest: Vec<u8>,
    pub digest: String,
    pub config: Vec<u8>,
    pub layers: Vec<Blob>,
}

impl Image {
    /// An image of `layers` for this machine.
    pub fn of(layers: Vec<Blob>) -> Self {
        let config = format!(
            r#"{{"architecture":"{}","os":"linux","rootfs":{{"type":"layers","diff_ids":[]}}}}"#,
            this_arch()
        );
        Self::with_config(layers, config.into_bytes())
    }

    pub fn with_config(layers: Vec<Blob>, config: Vec<u8>) -> Self {
        let descriptors: Vec<String> = layers
            .iter()
            .map(|l| {
                format!(
                    r#"{{"mediaType":"{}","digest":"{}","size":{}}}"#,
                    l.media_type,
                    sha256(&l.bytes),
                    l.bytes.len()
                )
            })
            .collect();
        let manifest = format!(
            r#"{{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"{}","size":{}}},"layers":[{}]}}"#,
            sha256(&config),
            config.len(),
            descriptors.join(",")
        )
        .into_bytes();
        Self {
            digest: sha256(&manifest),
            manifest,
            config,
            layers,
        }
    }

    /// The image reference text for a registry at `addr`.
    pub fn reference(&self, addr: &str, repository: &str) -> String {
        format!("docker://{addr}/{repository}@{}", self.digest)
    }
}

/// How the registry behaves; a test changes it while the registry runs.
#[derive(Default)]
pub struct Behaviour {
    /// Answer 401 with a challenge until a request carries this token.
    pub token: Option<String>,
    /// What the challenge says is the token endpoint's answer: the body to serve.
    pub token_answer: Option<String>,
    /// Challenge with this scheme instead of Bearer.
    pub challenge_scheme: Option<String>,
    /// Never accept the token: always 401.
    pub refuse_token: bool,
    /// 401 with no challenge header.
    pub no_challenge: bool,
    /// Answer blob requests with a redirect to `/cdn/<digest>`.
    pub redirect_blobs: bool,
    /// Redirect every path to itself, forever.
    pub redirect_loop: bool,
    /// A redirect with no Location.
    pub redirect_without_location: bool,
    /// Redirect to this relative `Location` instead of `/cdn/<digest>`.
    pub redirect_to: Option<String>,
    /// Send the headers of a blob and then nothing.
    pub stall_body: bool,
    /// Read the request and never answer.
    pub stall_head: bool,
    /// Status for every request (404...).
    pub status: Option<u16>,
    /// Read the request and close the connection without answering.
    pub hang_up: bool,
    /// Promise a blob's bytes and a hundred more, send the blob, and close.
    pub short_body: bool,
    /// Send manifests and blobs chunked, with a trailer after the last chunk.
    pub trailers: bool,
    /// The challenge names no scope.
    pub challenge_no_scope: bool,
    /// The challenge names no service.
    pub challenge_no_service: bool,
    /// The token endpoint's address has a query already.
    pub realm_query: bool,
    /// The token endpoint answers with this status.
    pub token_status: Option<u16>,
}

#[derive(Default)]
struct State {
    blobs: HashMap<String, Vec<u8>>,
    manifests: HashMap<String, (String, Vec<u8>)>,
    log: Vec<String>,
    behaviour: Behaviour,
}

pub struct FakeRegistry {
    pub addr: String,
    state: Arc<Mutex<State>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for FakeRegistry {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl FakeRegistry {
    /// A plain-HTTP registry on a loopback port.
    pub async fn start() -> Self {
        Self::start_with(None).await
    }

    /// A registry that speaks TLS with `acceptor`, if given.
    pub async fn start_with(acceptor: Option<tokio_rustls::TlsAcceptor>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr").to_string();
        let state = Arc::new(Mutex::new(State::default()));
        let shared = Arc::clone(&state);
        let task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let state = Arc::clone(&shared);
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    match acceptor {
                        Some(acceptor) => {
                            if let Ok(stream) = acceptor.accept(stream).await {
                                serve(stream, state).await;
                            }
                        }
                        None => serve(stream, state).await,
                    }
                });
            }
        });
        Self { addr, state, task }
    }

    /// A registry address nothing listens on.
    pub async fn closed_address() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        listener.local_addr().expect("addr").to_string()
    }

    /// A socket that accepts connections and says nothing: a handshake that never ends.
    pub async fn silent() -> (String, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr").to_string();
        let task = tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((stream, _)) = listener.accept().await {
                held.push(stream);
            }
        });
        (addr, task)
    }

    /// Publishes `image`: its manifest under its digest, and every blob.
    pub fn publish(&self, image: &Image) {
        let mut state = self.state.lock().expect("lock");
        state.manifests.insert(
            image.digest.clone(),
            (
                "application/vnd.oci.image.manifest.v1+json".to_owned(),
                image.manifest.clone(),
            ),
        );
        state
            .blobs
            .insert(sha256(&image.config), image.config.clone());
        for layer in &image.layers {
            state
                .blobs
                .insert(sha256(&layer.bytes), layer.bytes.clone());
        }
    }

    /// Serves `bytes` as the manifest of `digest`, whatever they hash to.
    pub fn publish_manifest_as(&self, digest: &str, media_type: &str, bytes: &[u8]) {
        let mut state = self.state.lock().expect("lock");
        state
            .manifests
            .insert(digest.to_owned(), (media_type.to_owned(), bytes.to_vec()));
    }

    /// Serves `bytes` as the blob `digest`, whatever they hash to.
    pub fn publish_blob_as(&self, digest: &str, bytes: &[u8]) {
        let mut state = self.state.lock().expect("lock");
        state.blobs.insert(digest.to_owned(), bytes.to_vec());
    }

    pub fn behave(&self, change: impl FnOnce(&mut Behaviour)) {
        change(&mut self.state.lock().expect("lock").behaviour);
    }

    /// The requests so far, as `GET <path> [auth]`.
    pub fn requests(&self) -> Vec<String> {
        self.state.lock().expect("lock").log.clone()
    }

    pub fn request_count(&self) -> usize {
        self.state.lock().expect("lock").log.len()
    }
}

struct Request {
    path: String,
    authorization: Option<String>,
    host: String,
}

async fn read_request<S: tokio::io::AsyncRead + Unpin>(stream: &mut S) -> Option<Request> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let text = String::from_utf8_lossy(&buf).into_owned();
    let mut lines = text.lines();
    let path = lines.next()?.split(' ').nth(1)?.to_owned();
    let headers: Vec<(&str, &str)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k, v.trim()))
        .collect();
    let header = |name: &str| {
        headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| (*v).to_owned())
    };
    Some(Request {
        path,
        authorization: header("authorization"),
        host: header("host").unwrap_or_default(),
    })
}

fn response(status: u16, headers: &[(&str, String)], body: &[u8]) -> Vec<u8> {
    let mut out = format!(
        "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (k, v) in headers {
        out.push_str(&format!("{k}: {v}\r\n"));
    }
    out.push_str("\r\n");
    let mut out = out.into_bytes();
    out.extend_from_slice(body);
    out
}

async fn serve<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(
    mut stream: S,
    state: Arc<Mutex<State>>,
) {
    let Some(request) = read_request(&mut stream).await else {
        return;
    };
    let answer = {
        let mut s = state.lock().expect("lock");
        let auth = request.authorization.clone().unwrap_or_default();
        s.log.push(format!("GET {} [{auth}]", request.path));
        answer(&s, &request)
    };
    match answer {
        Answer::Bytes(bytes) => {
            let _ = stream.write_all(&bytes).await;
            let _ = stream.shutdown().await;
        }
        Answer::StallBody(head) => {
            let _ = stream.write_all(&head).await;
            let _ = stream.flush().await;
            std::future::pending::<()>().await;
        }
        Answer::StallHead => std::future::pending::<()>().await,
        Answer::HangUp => {}
    }
}

/// `bytes` as a chunked answer with a trailer after the last chunk.
fn chunked(media: &str, bytes: &[u8]) -> Vec<u8> {
    let mut out = format!(
        "HTTP/1.1 200 X\r\nContent-Type: {media}\r\nTransfer-Encoding: chunked\r\n\
         Trailer: X-Sum\r\nConnection: close\r\n\r\n{:x}\r\n",
        bytes.len()
    )
    .into_bytes();
    out.extend_from_slice(bytes);
    out.extend_from_slice(b"\r\n0\r\nX-Sum: 1\r\n\r\n");
    out
}

enum Answer {
    Bytes(Vec<u8>),
    /// Nothing, and the connection closed.
    HangUp,
    /// A head promising more body than is ever sent.
    StallBody(Vec<u8>),
    StallHead,
}

fn answer(s: &State, request: &Request) -> Answer {
    let b = &s.behaviour;
    let path = request.path.as_str();
    if b.stall_head {
        return Answer::StallHead;
    }
    if b.hang_up {
        return Answer::HangUp;
    }
    if let Some(status) = b.status {
        return Answer::Bytes(response(status, &[], b"nope"));
    }
    if b.redirect_loop {
        return Answer::Bytes(response(307, &[("Location", path.to_owned())], b""));
    }
    if b.redirect_without_location {
        return Answer::Bytes(response(307, &[], b""));
    }
    if path.starts_with("/token") {
        if let Some(status) = b.token_status {
            return Answer::Bytes(response(status, &[], b"no token"));
        }
        let body = b
            .token_answer
            .clone()
            .unwrap_or_else(|| format!(r#"{{"token":"{}"}}"#, b.token.clone().unwrap_or_default()));
        return Answer::Bytes(response(200, &[], body.as_bytes()));
    }
    if let Some(digest) = path.strip_prefix("/cdn/") {
        return Answer::Bytes(match s.blobs.get(digest) {
            Some(bytes) => response(200, &[], bytes),
            None => response(404, &[], b""),
        });
    }
    if b.token.is_some() || b.refuse_token {
        let wanted = b.token.as_ref().map(|t| format!("Bearer {t}"));
        if b.refuse_token || request.authorization != wanted {
            let scheme = b.challenge_scheme.as_deref().unwrap_or("Bearer");
            let service = if b.challenge_no_service {
                ""
            } else {
                r#",service="fake-registry""#
            };
            let scope = if b.challenge_no_scope {
                ""
            } else {
                r#",scope="repository:test/tiny:pull""#
            };
            let query = if b.realm_query { "?client=kbf" } else { "" };
            let challenge =
                format!(r#"{scheme} realm="http://REALM/token{query}"{service}{scope}"#);
            let host = host_of(request);
            let mut headers = Vec::new();
            if !b.no_challenge {
                headers.push(("WWW-Authenticate", challenge.replace("REALM", &host)));
            }
            return Answer::Bytes(response(401, &headers, b""));
        }
    }
    if let Some((_, rest)) = path.split_once("/manifests/") {
        return match s.manifests.get(rest) {
            Some((media, bytes)) if b.trailers => Answer::Bytes(chunked(media, bytes)),
            Some((media, bytes)) => {
                Answer::Bytes(response(200, &[("Content-Type", media.clone())], bytes))
            }
            None => Answer::Bytes(response(404, &[], b"")),
        };
    }
    if let Some((_, digest)) = path.split_once("/blobs/") {
        if b.redirect_blobs {
            let to = b
                .redirect_to
                .clone()
                .unwrap_or_else(|| format!("/cdn/{digest}"));
            return Answer::Bytes(response(307, &[("Location", to)], b""));
        }
        return match s.blobs.get(digest) {
            Some(bytes) if b.trailers => Answer::Bytes(chunked("application/octet-stream", bytes)),
            Some(bytes) if b.short_body => {
                let head = format!(
                    "HTTP/1.1 200 X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    bytes.len() + 100
                );
                Answer::Bytes([head.as_bytes(), bytes].concat())
            }
            Some(bytes) if b.stall_body => {
                let head = format!(
                    "HTTP/1.1 200 X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    bytes.len() + 100
                );
                Answer::StallBody(head.into_bytes())
            }
            Some(bytes) => Answer::Bytes(response(200, &[], bytes)),
            None => Answer::Bytes(response(404, &[], b"")),
        };
    }
    Answer::Bytes(response(404, &[], b""))
}

/// The `host:port` the registry is reached at, from the request's own `Host`.
fn host_of(request: &Request) -> String {
    request.host.clone()
}
