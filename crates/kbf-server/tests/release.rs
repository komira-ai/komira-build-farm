//! The release store, its HTTP routes and `ReleaseService.Fetch` over real sockets
//! (`kbf_server::release`, `docs/design/pull-updates.md` section 6).

use std::collections::BTreeSet;
use std::io;
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use axum::body::Bytes;
use kbf_proto::update::FetchRequest;
use kbf_proto::update::release_service_client::ReleaseServiceClient;
use kbf_server::release::{
    self, Commit, Component, Key, Limits, PutError, ReleaseStore, Sha256Hex,
};
use kbf_server::token::ApiToken;
use kbf_types::ReleasePlatform;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tonic::Code;
use tonic::transport::Server;
use tonic::transport::server::TcpIncoming;

const TOKEN: &str = "kbf-test-token-0123456789abcdef0123456789";
const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
const OTHER_COMMIT: &str = "fedcba9876543210fedcba9876543210fedcba98";
const SMALL: Limits = Limits {
    tarball: 1024,
    attestation: 64,
};

/// A directory unique to one test, removed when it is dropped.
struct Dir(PathBuf);

impl Dir {
    fn new() -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("kbf-server-release-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&path).expect("a test directory");
        Self(path)
    }

    /// The store directory, inside, so that a path leaving it stays in the test's own.
    fn store(&self) -> PathBuf {
        self.0.join("store")
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn token() -> ApiToken {
    let dir = Dir::new();
    let path = dir.0.join("token");
    std::fs::write(&path, format!("{TOKEN}\n")).expect("write the token");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    ApiToken::from_file(&path).expect("a usable token")
}

fn sha(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// Every file and directory under `root`, relative, sorted.
fn tree(root: &Path) -> BTreeSet<String> {
    fn walk(dir: &Path, root: &Path, out: &mut BTreeSet<String>) {
        for entry in std::fs::read_dir(dir).expect("read a directory") {
            let path = entry.expect("an entry").path();
            let relative = path.strip_prefix(root).expect("inside");
            out.insert(relative.display().to_string());
            if path.is_dir() {
                walk(&path, root, out);
            }
        }
    }
    let mut out = BTreeSet::new();
    walk(root, root, &mut out);
    out
}

/// The `.partial` files under `root`.
fn partials(root: &Path) -> Vec<String> {
    tree(root)
        .into_iter()
        .filter(|p| p.ends_with(".partial"))
        .collect()
}

fn key(commit: &str, platform: ReleasePlatform, component: Component) -> Key {
    Key {
        commit: Commit::parse(commit).expect("a commit"),
        platform,
        component,
    }
}

fn kbf_key() -> Key {
    key(COMMIT, ReleasePlatform::LinuxX86_64, Component::Kbf)
}

fn body(bytes: &[u8]) -> impl futures::Stream<Item = io::Result<Bytes>> + Unpin + Send + use<> {
    futures::stream::iter(vec![Ok(Bytes::copy_from_slice(bytes))])
}

/// The store and the HTTP routes over it on a loopback port.
struct Served {
    dir: Dir,
    store: Option<Arc<ReleaseStore>>,
    addr: SocketAddr,
}

async fn serve_with(limits: Limits, configured: bool, token: Option<ApiToken>) -> Served {
    let dir = Dir::new();
    let store = configured
        .then(|| Arc::new(ReleaseStore::open(&dir.store(), limits).expect("open the store")));
    let app = release::router(store.clone(), token);
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let service = app.into_make_service_with_connect_info::<SocketAddr>();
        axum::serve(listener, service).await.expect("serve");
    });
    Served { dir, store, addr }
}

async fn serve() -> Served {
    serve_with(SMALL, true, Some(token())).await
}

struct Reply {
    status: u16,
    body: String,
}

impl Reply {
    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or_else(|e| panic!("{e}: {}", self.body))
    }
}

fn authorized() -> Vec<(&'static str, String)> {
    vec![("Authorization", format!("Bearer {TOKEN}"))]
}

async fn read_reply(stream: &mut TcpStream) -> Reply {
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.expect("read the reply");
    let text = String::from_utf8_lossy(&raw).into_owned();
    let status = text[9..12].parse().expect("a status");
    let body = text
        .split_once("\r\n\r\n")
        .map(|(_, b)| b)
        .unwrap_or_default();
    Reply {
        status,
        body: body.to_owned(),
    }
}

async fn http(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, String)],
    payload: &[u8],
) -> Reply {
    let mut stream = TcpStream::connect(addr).await.expect("connect");
    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nHost: kbf\r\nConnection: close\r\nContent-Length: {}\r\n",
        payload.len()
    );
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).await.expect("write");
    stream.write_all(payload).await.expect("write");
    read_reply(&mut stream).await
}

fn path_of(commit: &str, platform: &str, name: &str) -> String {
    format!("/v1/releases/{commit}/{platform}/{name}")
}

async fn put(served: &Served, name: &str, payload: &[u8]) -> Reply {
    let path = path_of(COMMIT, "linux-x86_64", name);
    http(served.addr, "PUT", &path, &authorized(), payload).await
}

async fn get_list(served: &Served) -> Reply {
    http(served.addr, "GET", "/v1/releases", &[], b"").await
}

async fn until(what: &str, mut ready: impl FnMut() -> bool) {
    for _ in 0..200 {
        if ready() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("never happened: {what}");
}

/// Catches: a path segment joined into a path as it arrived. Every attempt below would
/// write (or create a directory) outside the store, or under a name nobody asked for; each
/// must be refused with 400 and leave the test's directory as it was. The segments are
/// percent-encoded so that the router decodes them, as a real client's would be.
#[tokio::test]
async fn a_traversal_attempt_is_refused_and_touches_nothing() {
    let served = serve().await;
    std::fs::write(served.dir.0.join("sentinel"), b"keep").expect("sentinel");
    let before = tree(&served.dir.0);
    let attempts = [
        path_of("..", "linux-x86_64", "kbf"),
        path_of("%2e%2e", "linux-x86_64", "kbf"),
        path_of("..%2f..%2f..%2fetc", "linux-x86_64", "kbf"),
        path_of(COMMIT, "..", "kbf"),
        path_of(COMMIT, "..%2f..%2f..%2f..", "kbf"),
        path_of(COMMIT, "linux-x86_64", "..%2f..%2fsentinel"),
        path_of(
            COMMIT,
            "linux-x86_64",
            "..%2f..%2f..%2fsentinel.attestation",
        ),
        path_of(COMMIT, "linux-x86_64", "kbf%2fx"),
        path_of(COMMIT, "linux-x86_64", "kbf.partial"),
        path_of(COMMIT, "linux-x86_64", "%00kbf"),
        path_of(&COMMIT.to_uppercase(), "linux-x86_64", "kbf"),
        path_of(COMMIT, "Linux-x86_64", "kbf"),
        path_of(&COMMIT[..39], "linux-x86_64", "kbf"),
    ];
    for path in &attempts {
        let reply = http(served.addr, "PUT", path, &authorized(), b"payload").await;
        assert_eq!(reply.status, 400, "{path}: {}", reply.body);
    }
    assert_eq!(tree(&served.dir.0), before, "nothing was created anywhere");
    assert_eq!(
        std::fs::read(served.dir.0.join("sentinel")).expect("read"),
        b"keep"
    );
    let listing = get_list(&served).await.json();
    assert_eq!(listing["releases"], serde_json::json!([]));
}

/// Catches: a second write of other bytes replacing the first (the immutability), and an
/// answer other than 409; also that the refused bytes leave no file of their own, and that
/// the first bytes are still what the key names.
#[tokio::test]
async fn overwriting_with_other_bytes_is_409_and_changes_nothing() {
    let served = serve().await;
    let first = put(&served, "kbf", b"first bytes").await;
    assert_eq!(first.status, 201, "{}", first.body);
    let before = tree(&served.dir.0);
    let second = put(&served, "kbf", b"second bytes").await;
    assert_eq!(second.status, 409, "{}", second.body);
    assert_eq!(
        tree(&served.dir.0),
        before,
        "the refused bytes left no trace"
    );
    let store = served.store.as_ref().expect("store");
    let info = store
        .component(&kbf_key())
        .await
        .expect("read")
        .expect("stored");
    assert_eq!(info.sha256.as_str(), sha(b"first bytes"));
    assert_eq!(info.size, 11);
}

/// Catches: a repeat of the same bytes answered as a failure or stored a second time; the
/// first answer is 201 with the digest and size, the repeat 200 with the same.
#[tokio::test]
async fn the_same_bytes_again_is_idempotent() {
    let served = serve().await;
    let first = put(&served, "kbf", b"the bytes").await;
    assert_eq!(first.status, 201, "{}", first.body);
    assert_eq!(first.json()["sha256"], sha(b"the bytes"));
    assert_eq!(first.json()["size"], 9);
    let before = tree(&served.dir.0);
    let again = put(&served, "kbf", b"the bytes").await;
    assert_eq!(again.status, 200, "{}", again.body);
    assert_eq!(again.json(), first.json());
    assert_eq!(tree(&served.dir.0), before);
    // The attestation is immutable the same way.
    assert_eq!(put(&served, "kbf.attestation", b"bundle").await.status, 201);
    assert_eq!(put(&served, "kbf.attestation", b"bundle").await.status, 200);
    assert_eq!(put(&served, "kbf.attestation", b"other!").await.status, 409);
}

/// Catches: two writers racing for one key both winning (a check-then-write instead of an
/// atomic create): of two different bodies sent together exactly one is stored; of two
/// equal bodies both are accepted.
#[tokio::test]
async fn racing_writers_cannot_both_win() {
    for round in 0..5 {
        let served = serve().await;
        let (a, b) = tokio::join!(
            put(&served, "kbf-daemon", b"racing bytes A"),
            put(&served, "kbf-daemon", b"racing bytes B")
        );
        let mut statuses = [a.status, b.status];
        statuses.sort_unstable();
        assert_eq!(statuses, [201, 409], "round {round}");
        let store = served.store.as_ref().expect("store");
        let k = key(COMMIT, ReleasePlatform::LinuxX86_64, Component::KbfDaemon);
        let stored = store.component(&k).await.expect("read").expect("stored");
        let winner = if a.status == 201 { &a } else { &b };
        assert_eq!(winner.json()["sha256"], stored.sha256.as_str());

        let same = serve().await;
        let (a, b) = tokio::join!(
            put(&same, "kbf", b"the same bytes"),
            put(&same, "kbf", b"the same bytes")
        );
        assert!(
            a.status / 100 == 2 && b.status / 100 == 2,
            "{} {}",
            a.status,
            b.status
        );
    }
}

/// Catches: a half-written upload becoming visible or served, and a partial left behind.
/// While an upload is half sent a `.partial` exists and nothing is listed; when the client
/// goes away the partial is gone and still nothing is listed. A `.partial` a power cut left
/// (planted by hand, in the tarball directory and in an entry directory) is never listed
/// and never served, whatever its name.
#[tokio::test]
async fn an_interrupted_write_leaves_no_visible_entry() {
    let served = serve().await;
    let files = served.dir.store().join("files");
    let path = path_of(COMMIT, "linux-x86_64", "kbf");
    let mut stream = TcpStream::connect(served.addr).await.expect("connect");
    let head = format!(
        "PUT {path} HTTP/1.1\r\nHost: kbf\r\nAuthorization: Bearer {TOKEN}\r\nContent-Length: 500\r\n\r\n"
    );
    stream.write_all(head.as_bytes()).await.expect("write");
    stream.write_all(&[7_u8; 100]).await.expect("write");
    until("a partial appears", || !partials(&files).is_empty()).await;
    assert_eq!(
        get_list(&served).await.json()["releases"],
        serde_json::json!([])
    );
    assert_eq!(
        tree(&files).len(),
        1,
        "only the partial is there: {:?}",
        tree(&files)
    );
    drop(stream);
    until("the partial is removed", || tree(&files).is_empty()).await;
    assert_eq!(
        get_list(&served).await.json()["releases"],
        serde_json::json!([])
    );

    // A cut in the store's own reading of the body.
    let store = served.store.as_ref().expect("store");
    let mut cut = futures::stream::iter(vec![
        Ok(Bytes::from_static(b"some bytes")),
        Err(io::Error::other("the client went away")),
    ]);
    let err = store
        .put_tarball(&kbf_key(), &mut cut)
        .await
        .expect_err("cut");
    assert!(matches!(err, PutError::Body(_)), "{err:?}");
    let mut cut = futures::stream::iter(vec![Err(io::Error::other("gone"))]);
    let err = store
        .put_attestation(&kbf_key(), &mut cut)
        .await
        .expect_err("cut");
    assert!(matches!(err, PutError::Body(_)), "{err:?}");
    assert!(tree(&files).is_empty(), "{:?}", tree(&files));

    // Leftovers of a crash.
    let entries = served
        .dir
        .store()
        .join("index")
        .join(COMMIT)
        .join("linux-x86_64");
    std::fs::create_dir_all(&entries).expect("dirs");
    let stale = sha(b"stale");
    std::fs::write(files.join(format!("{stale}.partial")), b"stale").expect("partial");
    std::fs::write(files.join("1-2-3.partial"), b"stale").expect("partial");
    std::fs::write(entries.join("kbf.partial"), format!("{stale} 5\n")).expect("partial");
    std::fs::write(entries.join("1-2-3.partial"), format!("{stale} 5\n")).expect("partial");
    std::fs::write(entries.join("kbf-daemon.attestation.partial"), b"b").expect("partial");
    let listing = get_list(&served).await.json();
    assert_eq!(listing["releases"], serde_json::json!([]), "{listing}");
    let staged = Sha256Hex::parse(&stale).expect("digest");
    assert!(store.open_tarball(&staged).await.expect("open").is_none());
    assert!(store.component(&kbf_key()).await.expect("read").is_none());
    // And a real write beside them works and lists as itself.
    assert_eq!(put(&served, "kbf", b"real").await.status, 201);
    let listing = get_list(&served).await.json();
    let components = &listing["releases"][0]["platforms"][0]["components"];
    assert_eq!(components.as_array().expect("array").len(), 1, "{listing}");
}

/// Catches: the size cap not applied (a tarball or an attestation past it stored), applied
/// one byte off (the limit itself is accepted), or left behind as a file; also an empty body
/// stored as a release.
#[tokio::test]
async fn the_size_cap_is_enforced() {
    let served = serve().await;
    let files = served.dir.store().join("files");
    let over = put(&served, "kbf", &[1_u8; 1025]).await;
    assert_eq!(over.status, 413, "{}", over.body);
    assert!(tree(&files).is_empty(), "{:?}", tree(&files));
    let exact = put(&served, "kbf", &[1_u8; 1024]).await;
    assert_eq!(exact.status, 201, "{}", exact.body);
    assert_eq!(exact.json()["size"], 1024);

    assert_eq!(
        put(&served, "kbf.attestation", &[2_u8; 65]).await.status,
        413
    );
    assert_eq!(
        put(&served, "kbf.attestation", &[2_u8; 64]).await.status,
        201
    );
    // Several chunks add up.
    let store = served.store.as_ref().expect("store");
    let k = key(COMMIT, ReleasePlatform::LinuxArm64, Component::Kbf);
    let mut chunks = futures::stream::iter(vec![
        Ok(Bytes::from(vec![3_u8; 600])),
        Ok(Bytes::from(vec![3_u8; 600])),
    ]);
    let err = store
        .put_tarball(&k, &mut chunks)
        .await
        .expect_err("too long");
    assert!(matches!(err, PutError::TooLarge { limit: 1024 }), "{err:?}");
    let mut chunks = futures::stream::iter(vec![
        Ok(Bytes::from(vec![3_u8; 40])),
        Ok(Bytes::from(vec![3_u8; 40])),
    ]);
    let err = store
        .put_attestation(&k, &mut chunks)
        .await
        .expect_err("too long");
    assert!(matches!(err, PutError::TooLarge { limit: 64 }), "{err:?}");

    assert_eq!(put(&served, "kbf-daemon", b"").await.status, 400);
    assert_eq!(
        put(&served, "kbf-daemon.attestation", b"").await.status,
        400
    );
    assert_eq!(store.limits(), SMALL);
    assert_eq!(Limits::default().tarball, 512 << 20);
    assert_eq!(Limits::default().attestation, 256 << 10);
}

/// Catches: an unconfigured store answering an empty 200 or a 404 instead of saying what
/// is missing, on the list, the write and the gRPC call.
#[tokio::test]
async fn unconfigured_answers_503_everywhere() {
    let served = serve_with(SMALL, false, Some(token())).await;
    for reply in [get_list(&served).await, put(&served, "kbf", b"x").await] {
        assert_eq!(reply.status, 503, "{}", reply.body);
        assert!(reply.body.contains("--releases-dir"), "{}", reply.body);
    }
    let mut client = grpc(release::service(None)).await;
    let status = client
        .fetch(FetchRequest { sha256: sha(b"x") })
        .await
        .expect_err("off");
    assert_eq!(status.code(), Code::Unavailable);
    assert!(
        status.message().contains("--releases-dir"),
        "{}",
        status.message()
    );
}

/// Catches: a write accepted without the operator's credentials: no header, a wrong token,
/// a browser page's `Origin`, a server with writes off. None may store a byte. Reads stay
/// open, like `GET /v1/nodes`.
#[tokio::test]
async fn a_put_needs_the_operator_token() {
    let served = serve().await;
    let path = path_of(COMMIT, "linux-x86_64", "kbf");
    let wrong = vec![(
        "Authorization",
        "Bearer not-the-token-not-the-token-not-the-token".to_owned(),
    )];
    let page = vec![
        ("Authorization", format!("Bearer {TOKEN}")),
        ("Origin", "http://evil.example".to_owned()),
    ];
    for (headers, want) in [(Vec::new(), 401), (wrong, 401), (page, 403)] {
        let reply = http(served.addr, "PUT", &path, &headers, b"payload").await;
        assert_eq!(reply.status, want, "{}", reply.body);
    }
    assert!(tree(&served.dir.store().join("files")).is_empty());
    assert_eq!(get_list(&served).await.status, 200);

    let off = serve_with(SMALL, true, None).await;
    let reply = http(off.addr, "PUT", &path, &authorized(), b"payload").await;
    assert_eq!(reply.status, 403, "{}", reply.body);
    assert!(tree(&off.dir.store().join("files")).is_empty());

    // The token lets the same request through.
    assert_eq!(put(&served, "kbf", b"payload").await.status, 201);
}

/// Catches: a list that shows partial truths: a platform marked complete without both
/// tarballs and both attestations, components out of order, strangers in the directory
/// (a non-commit directory, a file beside the entries) listed, or an empty commit listed.
#[tokio::test]
async fn the_list_names_what_is_stored_and_what_is_complete() {
    let served = serve().await;
    let store = served.store.as_ref().expect("store").clone();
    let put_key = async |k: &Key, bytes: &[u8], attest: bool| {
        let mut b = body(bytes);
        if attest {
            store.put_attestation(k, &mut b).await.expect("put");
        } else {
            store.put_tarball(k, &mut b).await.expect("put");
        }
    };
    let arm = ReleasePlatform::LinuxArm64;
    let x86 = ReleasePlatform::LinuxX86_64;
    // arm64 of COMMIT is complete; x86_64 has one tarball and no attestation.
    for component in Component::ALL {
        let k = key(COMMIT, arm, component);
        put_key(&k, component.as_str().as_bytes(), false).await;
        put_key(&k, b"bundle", true).await;
    }
    put_key(&key(COMMIT, x86, Component::KbfDaemon), b"d", false).await;
    // darwin-arm64 holds both tarballs but only one attestation: not complete.
    let darwin = ReleasePlatform::DarwinArm64;
    for component in Component::ALL {
        put_key(&key(COMMIT, darwin, component), b"t", false).await;
    }
    put_key(&key(COMMIT, darwin, Component::Kbf), b"bundle", true).await;
    // An attestation alone is not a component.
    put_key(&key(OTHER_COMMIT, x86, Component::Kbf), b"orphan", true).await;
    // Strangers.
    let index = served.dir.store().join("index");
    std::fs::create_dir_all(index.join("not-a-commit").join("linux-arm64")).expect("dir");
    std::fs::write(index.join("stranger"), b"x").expect("file");
    std::fs::write(index.join(COMMIT).join("note"), b"x").expect("file");
    std::fs::create_dir_all(index.join(COMMIT).join("plan9-mips")).expect("dir");

    let listing = get_list(&served).await;
    assert_eq!(listing.status, 200);
    let want = serde_json::json!({"releases": [{
    "commit": COMMIT,
    "platforms": [
        {"platform": "linux-x86_64", "complete": false, "components": [
            {"name": "kbf-daemon", "sha256": sha(b"d"), "size": 1, "attested": false}]},
        {"platform": "linux-arm64", "complete": true, "components": [
            {"name": "kbf", "sha256": sha(b"kbf"), "size": 3, "attested": true},
            {"name": "kbf-daemon", "sha256": sha(b"kbf-daemon"), "size": 10, "attested": true}]},
            {"platform": "darwin-arm64", "complete": false, "components": [
                {"name": "kbf", "sha256": sha(b"t"), "size": 1, "attested": true},
                {"name": "kbf-daemon", "sha256": sha(b"t"), "size": 1, "attested": false}]},
    ]}]});
    assert_eq!(listing.json(), want);
    let attestation = store
        .attestation(&key(COMMIT, arm, Component::Kbf))
        .await
        .expect("read");
    assert_eq!(attestation.as_deref(), Some(&b"bundle"[..]));
    let none = store
        .attestation(&key(COMMIT, x86, Component::Kbf))
        .await
        .expect("read");
    assert_eq!(none, None);
}

/// Catches a platform called complete (or incomplete) by a rule that is not the platform's:
/// a Linux host needs `kbf` and a Mac needs `kbf-daemon` (the tarball of each platform holds
/// one binary), so each is complete with its own component attested and nothing else, and
/// neither is complete with the other platform's component alone.
///
/// Mutant seen red: every platform wanting both components (a release is never complete).
#[tokio::test]
async fn a_platform_is_complete_with_the_tarball_its_hosts_need() {
    let served = serve().await;
    let store = served.store.as_ref().expect("store").clone();
    let put_both = async |k: &Key| {
        store.put_tarball(k, &mut body(b"t")).await.expect("put");
        store
            .put_attestation(k, &mut body(b"bundle"))
            .await
            .expect("put");
    };
    put_both(&key(COMMIT, ReleasePlatform::LinuxX86_64, Component::Kbf)).await;
    put_both(&key(
        COMMIT,
        ReleasePlatform::DarwinArm64,
        Component::KbfDaemon,
    ))
    .await;
    put_both(&key(
        COMMIT,
        ReleasePlatform::LinuxArm64,
        Component::KbfDaemon,
    ))
    .await;
    let listing = get_list(&served).await.json();
    let complete: Vec<(String, bool)> = listing["releases"][0]["platforms"]
        .as_array()
        .expect("platforms")
        .iter()
        .map(|p| {
            (
                p["platform"].as_str().expect("name").to_owned(),
                p["complete"].as_bool().expect("flag"),
            )
        })
        .collect();
    assert_eq!(
        complete,
        [
            ("linux-x86_64".to_owned(), true),
            ("linux-arm64".to_owned(), false),
            ("darwin-arm64".to_owned(), true),
        ]
    );
}

/// Catches: a failing directory answered as success or as an empty list, and its path
/// leaking into the answer; an entry file that is not an entry taken for one.
#[tokio::test]
async fn a_failing_directory_is_a_500_that_names_no_path() {
    let served = serve().await;
    assert_eq!(put(&served, "kbf", b"fine").await.status, 201);
    let entry = served
        .dir
        .store()
        .join("index")
        .join(COMMIT)
        .join("linux-x86_64")
        .join("kbf");
    for garbage in [
        "",
        "not an entry",
        "zz 4\n",
        &format!("{} x\n", sha(b"a")),
        &format!("{} 4", sha(b"a")),
    ] {
        std::fs::write(&entry, garbage).expect("corrupt");
        let reply = get_list(&served).await;
        assert_eq!(reply.status, 500, "{garbage:?}: {}", reply.body);
        assert!(!reply.body.contains("store/index"), "{}", reply.body);
        let again = put(&served, "kbf", b"fine").await;
        assert_eq!(again.status, 500, "{garbage:?}");
    }
    std::fs::write(&entry, [0xff_u8, 0xfe]).expect("corrupt");
    assert_eq!(get_list(&served).await.status, 500);
    // A directory where the entry should be: it cannot be read.
    std::fs::remove_file(&entry).expect("remove");
    std::fs::create_dir(&entry).expect("dir");
    assert_eq!(get_list(&served).await.status, 500);
    assert_eq!(put(&served, "kbf", b"fine").await.status, 500);

    let gone = serve().await;
    std::fs::remove_dir_all(gone.dir.store().join("files")).expect("remove");
    let reply = put(&gone, "kbf", b"x").await;
    assert_eq!(reply.status, 500, "{}", reply.body);
    assert!(!reply.body.contains("files"), "{}", reply.body);
    std::fs::remove_dir_all(gone.dir.store().join("index")).expect("remove");
    assert_eq!(get_list(&gone).await.status, 500);
}

async fn grpc(
    service: release::grpc::ReleasesServer,
) -> ReleaseServiceClient<tonic::transport::Channel> {
    let incoming = TcpIncoming::bind("127.0.0.1:0".parse().expect("addr")).expect("bind");
    let addr = incoming.local_addr().expect("addr");
    tokio::spawn(
        Server::builder()
            .add_service(service)
            .serve_with_incoming(incoming),
    );
    let channel = tonic::transport::Channel::from_shared(format!("http://{addr}"))
        .expect("endpoint")
        .connect()
        .await
        .expect("connect");
    ReleaseServiceClient::new(channel)
}

async fn fetch_all(
    client: &mut ReleaseServiceClient<tonic::transport::Channel>,
    sha256: &str,
) -> Result<Vec<Vec<u8>>, tonic::Status> {
    let mut stream = client
        .fetch(FetchRequest {
            sha256: sha256.to_owned(),
        })
        .await?
        .into_inner();
    let mut chunks = Vec::new();
    while let Some(chunk) = stream.message().await? {
        chunks.push(chunk.data);
    }
    Ok(chunks)
}

/// Catches: Fetch serving other bytes than were stored, a digest that does not match what
/// was served, a chunk size other than 1 MiB, an empty chunk after an exact multiple, the
/// wrong code for an unknown digest or a malformed one, and a read error that ends the stream
/// as if the file were short.
#[tokio::test]
async fn fetch_streams_the_exact_bytes() {
    let served = serve_with(
        Limits {
            tarball: 8 << 20,
            attestation: 64,
        },
        true,
        Some(token()),
    )
    .await;
    let store = served.store.as_ref().expect("store").clone();
    let mut client = grpc(release::service(Some(Arc::clone(&store)))).await;
    let pattern = |len: usize| -> Vec<u8> { (0..len).map(|i| (i * 31 % 251) as u8).collect() };
    let mib = 1 << 20;
    for (i, (len, chunk_lens)) in [
        (1, vec![1]),
        (mib - 1, vec![mib - 1]),
        (mib, vec![mib]),
        (mib + 1, vec![mib, 1]),
        (2 * mib, vec![mib, mib]),
        (2 * mib + mib / 2, vec![mib, mib, mib / 2]),
    ]
    .into_iter()
    .enumerate()
    {
        let bytes = pattern(len);
        // One key holds one tarball: each length has a commit of its own.
        let k = key(
            &format!("{i:040x}"),
            ReleasePlatform::DarwinArm64,
            Component::KbfDaemon,
        );
        let digest = store
            .put_tarball(&k, &mut body(&bytes))
            .await
            .expect("put")
            .sha256;
        let chunks = fetch_all(&mut client, digest.as_str())
            .await
            .expect("fetch");
        let lens: Vec<_> = chunks.iter().map(Vec::len).collect();
        assert_eq!(lens, chunk_lens, "{len}");
        let all: Vec<u8> = chunks.concat();
        assert_eq!(sha(&all), digest.as_str(), "{len}");
        assert_eq!(all, bytes, "{len}");
    }

    let unknown = fetch_all(&mut client, &sha(b"never stored"))
        .await
        .expect_err("unknown");
    assert_eq!(unknown.code(), Code::NotFound);
    for bad in [
        "",
        "abc",
        &"A".repeat(64),
        &"../".repeat(22),
        &format!("{}.partial", sha(b"x")),
    ] {
        let status = fetch_all(&mut client, bad).await.expect_err(bad);
        assert_eq!(status.code(), Code::InvalidArgument, "{bad:?}");
    }

    // A file that cannot be read: a directory where the file should be.
    let broken = sha(b"a directory");
    std::fs::create_dir(served.dir.store().join("files").join(&broken)).expect("dir");
    let status = fetch_all(&mut client, &broken)
        .await
        .expect_err("unreadable");
    assert_eq!(status.code(), Code::Internal);
    // A file that cannot be opened: no permission on it (skipped for a user who can).
    let locked = sha(b"locked");
    let locked_path = served.dir.store().join("files").join(&locked);
    std::fs::write(&locked_path, b"x").expect("file");
    std::fs::set_permissions(&locked_path, std::fs::Permissions::from_mode(0o000)).expect("chmod");
    if std::fs::File::open(&locked_path).is_err() {
        let status = fetch_all(&mut client, &locked)
            .await
            .expect_err("unopenable");
        assert_eq!(status.code(), Code::Internal);
    }
}
