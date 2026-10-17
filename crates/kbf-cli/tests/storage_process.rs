//! `kbf storage` as a separate process (docs/design/architecture-v2.md, section 13 P2):
//! the REAPI conformance suite (`kbf-cas-conformance`) run by the daemon's transfer
//! engine over real gRPC to a child process, and the storage client
//! ([`kbf_storage::service::RemoteStorage`]) over the same connection. Nothing here is
//! in-process: the store, its metadata log and its listener are the child's.

use std::io::{BufRead as _, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use kbf_cas_conformance::Subject;
use kbf_storage::Storage as _;
use kbf_storage::service::RemoteStorage;
use kbf_transfer::{TransferConfig, Transfers};
use tonic::transport::Endpoint;

const BIN: &str = env!("CARGO_BIN_EXE_kbf");

/// A running `kbf storage`; killed when dropped.
struct Node {
    child: Child,
    addr: String,
}

impl Node {
    /// Stops the node as an operator does, with SIGTERM, and waits for its exit status
    /// (it flushes what it has measured on the way out, which a kill would lose). A node
    /// that has not exited in ten seconds is killed.
    fn stop(&mut self) -> Option<std::process::ExitStatus> {
        let pid = self.child.id().to_string();
        let _ = Command::new("kill").args(["-TERM", &pid]).status();
        for _ in 0..200 {
            if let Ok(Some(status)) = self.child.try_wait() {
                return Some(status);
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        None
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

/// Starts `kbf storage` over new stores under `dir`, plain text on a free loopback port,
/// and waits for its start line.
fn start(dir: &Path, extra: &[&str]) -> Node {
    std::fs::create_dir_all(dir).expect("directory");
    let cold = format!("--cold-store=file://{}", dir.join("cold").display());
    let meta = format!("--meta-dir={}", dir.join("meta").display());
    let mut flags = vec![cold.as_str(), meta.as_str()];
    flags.extend_from_slice(extra);
    spawn(&flags, &[])
}

/// `kbf storage --storage-listen=127.0.0.1:0` and these flags, with these variables in
/// its environment.
fn spawn(flags: &[&str], env: &[(&str, &str)]) -> Node {
    let mut child = Command::new(BIN)
        .args(["storage", "--storage-listen=127.0.0.1:0"])
        .args(flags)
        .envs(env.iter().copied())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn kbf storage");
    let mut line = String::new();
    BufReader::new(child.stdout.take().expect("stdout"))
        .read_line(&mut line)
        .expect("a start line");
    let addr = line
        .split_whitespace()
        .find_map(|w| w.strip_prefix("storage="))
        .unwrap_or_else(|| panic!("no storage= in the start line {line:?}"))
        .to_owned();
    assert!(line.starts_with("kbf-storage "), "{line}");
    Node { child, addr }
}

/// One `kbf storage` process per [`Subject::fresh`], each over empty stores.
struct Process {
    scratch: PathBuf,
    nodes: Mutex<Vec<Node>>,
}

impl Process {
    fn new() -> Self {
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let scratch = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("storage-process-{}-{n}", std::process::id()));
        Self {
            scratch,
            nodes: Mutex::new(Vec::new()),
        }
    }
}

impl Subject for Process {
    type Cas = Transfers;

    async fn fresh(&self) -> Arc<Transfers> {
        let mut nodes = self.nodes.lock().expect("lock");
        let node = start(&self.scratch.join(format!("node-{}", nodes.len())), &[]);
        let endpoint = Endpoint::from_shared(format!("http://{}", node.addr)).expect("endpoint");
        nodes.push(node);
        Arc::new(Transfers::new(endpoint, TransferConfig::default()))
    }

    fn scratch(&self) -> &Path {
        std::fs::create_dir_all(&self.scratch).expect("scratch");
        &self.scratch
    }
}

kbf_cas_conformance::tests!(Process::new());

/// Catches a storage client that cannot talk to the process that serves it: blobs and
/// the action cache over `RemoteStorage` against `kbf storage`, as the frontend will
/// use them (the conformance suite above is the daemon's side).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_storage_client_reads_what_it_wrote_in_another_process() {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("storage-client-{}", std::process::id()));
    let node = start(&dir, &[]);
    let channel = Endpoint::from_shared(format!("http://{}", node.addr))
        .expect("endpoint")
        .connect()
        .await
        .expect("connect");
    let remote = RemoteStorage::new(channel, 0);
    let blob = Bytes::from_static(b"stored by one process, read from another");
    let digest = remote.put(blob.clone()).await.expect("put");
    assert_eq!(remote.read_blob(&digest).await.expect("read"), blob);
    let other = kbf_types::Digest::new(kbf_types::DigestFunction::Sha256, [9; 32], 3);
    assert_eq!(
        remote
            .find_missing(&[digest, other])
            .await
            .expect("missing"),
        [other]
    );
}

/// Catches a storage node that starts without a store (it must exit 2 saying which flag
/// is missing, as `kbf-server` does), and one that accepts a state store (it must exit 2
/// before opening anything: storage nodes reach the state store only through a frontend).
#[test]
fn a_refused_start_exits_2_and_a_state_store_is_refused() {
    let run = |args: &[&str]| Command::new(BIN).args(args).output().expect("run kbf");
    let out = run(&["storage"]);
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("kbf storage: --cold-store is required"),
        "{err}"
    );
    assert!(out.stdout.is_empty());
    let out = run(&[
        "storage",
        "--state-store=sqlite:///x",
        "--cold-store=file:///x",
    ]);
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("kbf storage refuses --state-store: "), "{err}");
    assert!(out.stdout.is_empty());
}

/// Catches the two store shapes of `kbf storage` that the conformance runs above do not
/// start: an `s3://` cold store (no connection is made until a blob moves, so a node
/// starts against an endpoint nobody listens on; its key pair is the standard variables)
/// and a hot store (the write tier's flusher runs beside the listener); and a node that
/// does not exit 0 on SIGTERM.
#[test]
fn the_s3_and_hot_store_shapes_start_and_stop_on_sigterm() {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("storage-shapes-{}", std::process::id()));
    let meta = format!("--meta-dir={}", dir.join("s3-meta").display());
    let keys = [("AWS_ACCESS_KEY_ID", "a"), ("AWS_SECRET_ACCESS_KEY", "b")];
    let mut s3 = spawn(
        &[
            "--cold-store=s3://bucket/kbf/?endpoint=http://127.0.0.1:1",
            &meta,
        ],
        &keys,
    );
    assert!(s3.stop().is_some_and(|status| status.success()));
    let cold = format!("--cold-store=file://{}", dir.join("cold").display());
    let hot = format!("--hot-store=file://{}", dir.join("hot").display());
    std::fs::create_dir_all(dir.join("hot")).expect("hot directory");
    let mut tiered = spawn(&[&cold, &hot], &[]);
    assert!(tiered.stop().is_some_and(|status| status.success()));
}

/// Catches a `kbf storage` that serves plain text off loopback (it would admit Write and
/// ActionCommit from anyone): on a wildcard IPv4 and IPv6 address and on a LAN one, with
/// no TLS flags, the process exits 2 with one `kbf storage:` line that names the address and the
/// three flags, prints no start line, and opens no store (the meta directory is not
/// created); and with the TLS flags it gets past that check (to the missing certificate
/// file, not to a start). Loopback without TLS is every other test here.
#[test]
fn plain_text_off_loopback_is_a_refused_start() {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("storage-off-loopback-{}", std::process::id()));
    let cold = format!("--cold-store=file://{}", dir.join("cold").display());
    let meta = format!("--meta-dir={}", dir.join("meta").display());
    // A node that wrongly starts serves until it is killed: wait a while, then kill it
    // and let the assertions below say it did not exit 2.
    let run = |listen: &str, more: &[&str]| {
        let mut child = Command::new(BIN)
            .args([
                "storage",
                &cold,
                &meta,
                &format!("--storage-listen={listen}"),
            ])
            .args(more)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn kbf storage");
        for _ in 0..200 {
            if child.try_wait().expect("wait").is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let _ = child.kill();
        child.wait_with_output().expect("output")
    };
    for listen in ["0.0.0.0:0", "[::]:0", "192.0.2.10:0"] {
        let out = run(listen, &[]);
        assert_eq!(out.status.code(), Some(2), "{listen}");
        let err = String::from_utf8_lossy(&out.stderr);
        let refusals = err
            .lines()
            .filter(|l| l.starts_with("kbf storage: "))
            .count();
        assert_eq!(refusals, 1, "{err}");
        for want in [
            "kbf storage: --storage-listen ",
            listen,
            "--storage-tls-cert",
            "--storage-tls-key",
            "--storage-client-ca",
        ] {
            assert!(err.contains(want), "{listen}: {err}");
        }
        assert!(out.stdout.is_empty(), "{listen}");
    }
    assert!(!dir.join("meta").exists(), "a refused start opened a store");
    let nowhere = dir.join("none.pem").display().to_string();
    let out = run(
        "0.0.0.0:0",
        &[
            &format!("--storage-tls-cert={nowhere}"),
            &format!("--storage-tls-key={nowhere}"),
            &format!("--storage-client-ca={nowhere}"),
        ],
    );
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("read ") && err.contains("none.pem"), "{err}");
    assert!(!err.contains("--storage-listen"), "{err}");
}
