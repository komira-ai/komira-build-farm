//! The `kbf-all-in-one` binary end to end: one action, executed through the REAPI
//! listener of the server half, run by the daemon half's container driver over a
//! stand-in `podman` (`kbf-fake-podman`, which runs the action as a plain process) and a
//! cgroup tree of plain directories, its outputs, stdout and stderr stored through the
//! in-process data plane and read back over REAPI. And the starts each half refuses.

#![cfg(target_os = "linux")]

use std::io::{BufRead as _, BufReader};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use kbf_proto::google::longrunning::operation;
use kbf_proto::reapi::content_addressable_storage_client::ContentAddressableStorageClient;
use kbf_proto::reapi::execution_client::ExecutionClient;
use kbf_proto::reapi::platform::Property;
use kbf_proto::reapi::{
    Action, ActionResult, Command as ReapiCommand, Digest, Directory, ExecuteRequest,
    ExecuteResponse, FindMissingBlobsRequest, Platform, Tree,
};
use kbf_transfer::{Cas as _, TransferConfig, Transfers, fetch};
use prost::Message as _;
use rcgen::{CertificateParams, CertifiedIssuer, IsCa, KeyPair};
use tonic::transport::{Channel, Endpoint};

// The stand-in `podman` runs each action as `kbf-act` beside it: a link to this test
// binary, which answers as `kbf-act` through it.
kbf_testkit::act_test_hook!();

const BIN: &str = env!("CARGO_BIN_EXE_kbf-all-in-one");

/// How long the test waits for what should happen promptly.
const PROMPT: Duration = Duration::from_secs(60);

/// The per-architecture manifest the stand-in image store holds.
const MANIFEST: &[u8] =
    br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json"}"#;

/// A fresh directory for one test.
fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("kbf-all-in-one")
        .join(format!("{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch");
    dir
}

/// A CA, the node's certificate (`node-1`, its one DNS name), a remote daemon's
/// (`remote-1`) and the server's (`localhost`), written into `dir`.
fn pki(dir: &Path) {
    let mut ca = CertificateParams::new(Vec::<String>::new()).expect("CA params");
    ca.is_ca = IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let ca = CertifiedIssuer::self_signed(ca, KeyPair::generate().expect("key")).expect("CA");
    std::fs::write(dir.join("ca.pem"), ca.pem()).expect("write");
    for (name, dns) in [
        ("node", "node-1"),
        ("remote", "remote-1"),
        ("server", "localhost"),
    ] {
        let key = KeyPair::generate().expect("key");
        let cert = CertificateParams::new(vec![dns.to_owned()])
            .expect("params")
            .signed_by(&key, &ca)
            .expect("sign");
        std::fs::write(dir.join(format!("{name}.pem")), cert.pem()).expect("write");
        std::fs::write(dir.join(format!("{name}.key")), key.serialize_pem()).expect("write");
    }
}

/// `--id-files`: a passwd naming this test's user `kbf`, with 65,536 subordinate ids.
fn id_files(dir: &Path) -> PathBuf {
    use std::os::unix::fs::MetadataExt as _;

    let ids = dir.join("ids");
    std::fs::create_dir_all(&ids).expect("mkdir");
    let passwd = ids.join("passwd");
    std::fs::write(&passwd, "").expect("write");
    let uid = std::fs::metadata(&passwd).expect("stat").uid();
    std::fs::write(&passwd, format!("kbf:x:{uid}:{uid}::/:/usr/bin/false\n")).expect("write");
    for file in ["subuid", "subgid"] {
        std::fs::write(ids.join(file), "kbf:100000:65536\n").expect("write");
    }
    ids
}

/// The stand-in `podman` in `bin`, and `kbf-act` beside it, with an image store holding
/// [`MANIFEST`] as image `img1`; the stand-in runs `steps` as each action. Returns the
/// image as an action names it.
fn podman(bin: &Path, steps: &[&str]) -> String {
    let state = bin.join("state");
    std::fs::create_dir_all(&state).expect("mkdir");
    let fake = env!("CARGO_BIN_EXE_kbf-fake-podman");
    std::os::unix::fs::symlink(fake, bin.join("podman")).expect("link podman");
    kbf_testkit::act::link_self(&bin.join(kbf_testkit::act::NAME)).expect("link kbf-act");
    std::fs::write(state.join("image-id"), "img1\n").expect("image id");
    std::fs::write(state.join("nonce"), format!("{}:", std::process::id())).expect("nonce");
    let knob: String = steps.iter().map(|s| format!("{s}\n")).collect();
    std::fs::write(state.join("action"), knob).expect("action");
    let digest = format!("sha256:{}", kbf_transfer::digest_of(MANIFEST).hash);
    let store = state.join("store/fake-images/img1");
    std::fs::create_dir_all(&store).expect("mkdir store");
    let file = kbf_driver_container::image::manifest_file(&digest);
    std::fs::write(store.join(file), MANIFEST).expect("manifest");
    format!("docker://registry.test/tools/busybox@{digest}")
}

/// A cgroup v2 mount of plain directories at `bin/cgroup`, where the stand-in `podman`
/// looks, holding the daemon's unit; the flags that point the daemon at it.
fn cgroup_tree(bin: &Path) -> Vec<String> {
    let root = bin.join("cgroup");
    let unit = root.join("kbf.slice/kbf-all-in-one.service");
    std::fs::create_dir_all(&unit).expect("mkdir");
    let plant = |path: PathBuf, text: &str| std::fs::write(path, text).expect("plant");
    plant(
        root.join("cgroup.controllers"),
        "cpuset cpu io memory pids\n",
    );
    plant(unit.join("cgroup.controllers"), "cpu memory pids\n");
    plant(unit.join("cgroup.procs"), "1\n");
    plant(unit.join("cpuset.cpus.effective"), "0\n");
    let own = bin.join("self-cgroup");
    plant(own.clone(), "0::/kbf.slice/kbf-all-in-one.service\n");
    vec![
        format!("--cgroup-root={}", root.display()),
        format!("--self-cgroup={}", own.display()),
    ]
}

/// A port nothing listens on now.
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("addr")
        .port()
}

/// The flags of a one-node farm in `dir`: the server's worker listener on `port` under
/// mutual TLS, a cold store and a hot store (the write tier, holding the metadata log),
/// and the daemon half's container driver dialling it.
fn farm_flags(dir: &Path, port: u16) -> Vec<String> {
    let at = |name: &str| dir.join(name).display().to_string();
    vec![
        "--listen=127.0.0.1:0".to_owned(),
        format!("--worker-listen=127.0.0.1:{port}"),
        format!("--worker-tls-cert={}", at("server.pem")),
        format!("--worker-tls-key={}", at("server.key")),
        format!("--worker-client-ca={}", at("ca.pem")),
        format!("--cold-store=file://{}", at("cold")),
        format!("--hot-store=file://{}", at("hot")),
        format!("--server=https://127.0.0.1:{port}"),
        "--tls-server-name=localhost".to_owned(),
        format!("--ca-cert={}", at("ca.pem")),
        format!("--cert={}", at("node.pem")),
        format!("--key={}", at("node.key")),
        "--node-id=node-1".to_owned(),
        "--reconnect-ms=50".to_owned(),
        "--driver=container".to_owned(),
        format!("--data-dir={}", at("data")),
    ]
}

/// `PATH` with `bin`, where [`podman`] put the stand-in, first.
fn image_path(bin: &Path) -> String {
    format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    )
}

/// The binary with `flags` (a one-node farm in `dir`, [`farm_flags`]) whose container
/// driver runs the stand-in `podman` in `bin`, found through `path`.
fn farm(dir: &Path, bin: &Path, path: &str, flags: Vec<String>) -> Command {
    let mut command = Command::new(BIN);
    command
        .args(flags)
        .arg(format!("--id-files={}", id_files(dir).display()))
        .args(cgroup_tree(bin))
        .env("PATH", path)
        // The stand-in finds its state beside the link it was run through, which the
        // driver runs by the name `PATH` finds (argv0 `podman`): from here.
        .current_dir(bin);
    command
}

/// Kills the process when dropped, so a failed test leaves nothing running.
struct Running(Child);

impl Running {
    /// Waits for the process to end on its own, for at most [`PROMPT`].
    fn wait(mut self) -> std::process::ExitStatus {
        let deadline = std::time::Instant::now() + PROMPT;
        loop {
            if let Some(status) = self.0.try_wait().expect("wait") {
                return status;
            }
            assert!(std::time::Instant::now() < deadline, "still running");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Stores `bytes` through the REAPI listener.
async fn put(cas: &Transfers, bytes: Vec<u8>) -> Digest {
    cas.put(bytes).await.expect("upload")
}

/// Executes `action` until its operation is done; returns its result.
async fn execute(reapi: Channel, action: Digest) -> ActionResult {
    let mut ops = ExecutionClient::new(reapi)
        .execute(ExecuteRequest {
            action_digest: Some(action),
            skip_cache_lookup: true,
            ..ExecuteRequest::default()
        })
        .await
        .expect("Execute")
        .into_inner();
    let done = loop {
        let op = ops.message().await.expect("stream").expect("an update");
        if op.done {
            break op;
        }
    };
    let Some(operation::Result::Response(any)) = done.result else {
        panic!("no response: {done:?}");
    };
    let response = ExecuteResponse::decode(any.value.as_slice()).expect("response");
    response.result.expect("a result")
}

/// Catches the daemon half's outputs not reaching the server half's CAS, or reaching it
/// other than through the in-process plane (the binary is given no `--cas`, so a daemon
/// half wired to the transfer engine refuses to start), the helper re-execs not
/// dispatched before the flags are read (each action's keeper is this binary, run with
/// arguments the command line refuses), and SIGTERM not stopping both halves with exit
/// 0. One action, run by the container driver over the stand-in `podman`, writes a file,
/// a directory holding a file, stdout and stderr; each is read back over REAPI and is
/// what the action wrote.
#[test]
fn an_actions_outputs_go_through_the_in_process_plane() {
    let dir = scratch("e2e");
    pki(&dir);
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).expect("mkdir");
    let steps = [
        "mkdir",
        "$UPPER/out/dir",
        ";",
        "write-lines",
        "$UPPER/out/file.txt",
        "a file",
        ";",
        "write-lines",
        "$UPPER/out/dir/nested.txt",
        "nested",
        ";",
        "echo",
        "to stdout",
        ";",
        "echo",
        "--stderr",
        "to stderr",
    ];
    let image = podman(&bin, &steps);
    let flags = farm_flags(&dir, free_port());
    let mut child = farm(&dir, &bin, &image_path(&bin), flags)
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn");
    let stdout = BufReader::new(child.stdout.take().expect("stdout"));
    let running = Running(child);
    let line = stdout.lines().next().expect("a start line").expect("read");
    let reapi: SocketAddr = line
        .split(' ')
        .find_map(|field| field.strip_prefix("reapi="))
        .unwrap_or_else(|| panic!("no reapi= in {line:?}"))
        .parse()
        .expect("an address");

    let tokio = tokio::runtime::Runtime::new().expect("runtime");
    tokio.block_on(async {
        let channel = Endpoint::from_shared(format!("http://{reapi}"))
            .expect("endpoint")
            .connect()
            .await
            .expect("connect");
        let cas = Transfers::over(channel.clone(), TransferConfig::default());
        let command = ReapiCommand {
            arguments: vec!["true".to_owned()],
            output_paths: vec!["out/file.txt".to_owned(), "out/dir".to_owned()],
            ..ReapiCommand::default()
        };
        let action = Action {
            command_digest: Some(put(&cas, command.encode_to_vec()).await),
            input_root_digest: Some(put(&cas, Directory::default().encode_to_vec()).await),
            platform: Some(Platform {
                properties: vec![Property {
                    name: "container-image".to_owned(),
                    value: image,
                }],
            }),
            ..Action::default()
        };
        let action = put(&cas, action.encode_to_vec()).await;
        let result = tokio::time::timeout(PROMPT, execute(channel, action))
            .await
            .expect("the action ran in time");
        let read = |d: Option<Digest>| {
            let cas = &cas;
            async move { fetch(cas, &d.expect("a digest")).await.expect("read back") }
        };
        assert_eq!(result.exit_code, 0, "{result:?}");
        assert_eq!(read(result.stdout_digest.clone()).await, b"to stdout\n");
        assert_eq!(read(result.stderr_digest.clone()).await, b"to stderr\n");
        let [file] = &result.output_files[..] else {
            panic!("one output file: {result:?}");
        };
        assert_eq!(file.path, "out/file.txt");
        assert_eq!(read(file.digest.clone()).await, b"a file\n");
        let [tree] = &result.output_directories[..] else {
            panic!("one output directory: {result:?}");
        };
        assert_eq!(tree.path, "out/dir");
        let tree = Tree::decode(read(tree.tree_digest.clone()).await.as_slice()).expect("a Tree");
        let root = tree.root.expect("a root");
        assert_eq!(root.files.len(), 1, "{root:?}");
        assert_eq!(root.files[0].name, "nested.txt");
        assert_eq!(read(root.files[0].digest.clone()).await, b"nested\n");
    });
    // Its connections closed, so the server half's drain does not wait for them.
    drop(tokio);

    let pid = rustix::process::Pid::from_child(&running.0);
    rustix::process::kill_process(pid, rustix::process::Signal::TERM).expect("SIGTERM");
    let status = running.wait();
    assert!(status.success(), "{status}");
}

/// Catches the daemon's helper re-execs dispatched after the flags are read, or not at
/// all: run with keeper arguments that name no mode, as a driver never runs it, the
/// binary answers as the keeper does (its usage line, exit 2), not with clap's error
/// about the flags it lacks, and starts no server (no start line on stdout).
#[test]
fn the_binary_answers_as_a_keeper_before_it_reads_its_flags() {
    let out = Command::new(BIN)
        .args(["__keeper", "groups"])
        .output()
        .expect("spawn");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "{stderr}");
    assert!(
        stderr.starts_with("kbf-keeper: usage: __keeper group"),
        "{stderr}"
    );
    assert!(out.stdout.is_empty(), "a server started");
}

/// Runs the binary with a one-node farm's flags in a fresh directory, `extra` taking the
/// place of any it names again, and returns its exit status and stderr.
fn refused(name: &str, extra: &[&str]) -> (Option<i32>, String) {
    let dir = scratch(name);
    pki(&dir);
    let name = |flag: &str| flag.split('=').next().unwrap_or(flag).to_owned();
    let named: Vec<String> = extra.iter().map(|flag| name(flag)).collect();
    let farm = farm_flags(&dir, free_port());
    let output = Command::new(BIN)
        .args(farm.iter().filter(|flag| !named.contains(&name(flag))))
        .args(extra)
        .output()
        .expect("run");
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    (output.status.code(), stderr)
}

/// Catches a start either half refuses that exits 0 or as the other half would, or
/// that does not say which half refused: a server whose flags its own start refuses
/// (a cold store that cannot be made) exits 2, as `kbf-server` does; a daemon whose
/// driver cannot start (no subordinate ids for its user) exits 1, as `kbf-daemon` does;
/// a driver other than the container driver is a usage error, 2.
#[test]
fn a_start_either_half_refuses_says_which() {
    let (code, stderr) = refused(
        "server-refuses",
        &["--cold-store=file:///proc/nonexistent/cold"],
    );
    assert_eq!(code, Some(2), "{stderr}");
    assert!(stderr.contains("kbf-all-in-one: server: "), "{stderr}");
    let (code, stderr) = refused("daemon-refuses", &["--id-files=/nonexistent/ids"]);
    assert_eq!(code, Some(1), "{stderr}");
    assert!(stderr.contains("kbf-all-in-one: daemon: "), "{stderr}");
    let (code, stderr) = refused("native", &["--driver=native"]);
    assert_eq!(code, Some(2), "{stderr}");
    assert!(stderr.contains("container driver only"), "{stderr}");
}

/// Catches the start-up pass or the evictor not run over the all-in-one's `file://`
/// cold store (mutants: `kbf_server::start`'s `file://` arm passes no pass; the
/// evictor loop removed): a single-voter farm is this binary's main use, and the pass
/// deletes its orphans and forgets its lost objects, the evictor keeps its disk from
/// filling. Without a hot store (uploads go straight to the cold store), on a fake
/// 16 MiB filesystem holding only the object files with a 4 MiB floor and no grace
/// window (the server's hidden test flags, which this binary takes as `kbf-server`
/// does), the pass logs its last line, twenty uploads of 1 MiB each all succeed, the
/// evictor logs a round and the first blob is reported missing.
#[test]
fn the_start_up_pass_and_the_evictor_run_over_a_file_cold_store() {
    const MIB: usize = 1 << 20;
    let dir = scratch("pass");
    pki(&dir);
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).expect("mkdir");
    podman(&bin, &["true"]);
    let mut flags: Vec<String> = farm_flags(&dir, free_port())
        .into_iter()
        .filter(|flag| !flag.starts_with("--hot-store="))
        .collect();
    flags.extend([
        format!("--meta-dir={}", dir.join("meta").display()),
        format!("--cold-store-size-bytes={}", 16 * MIB),
        format!("--cold-store-floor-bytes={}", 4 * MIB),
        "--cold-store-evict-grace-ms=0".to_owned(),
    ]);
    let mut child = farm(&dir, &bin, &image_path(&bin), flags)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn");
    let stdout = BufReader::new(child.stdout.take().expect("stdout"));
    let stderr = BufReader::new(child.stderr.take().expect("stderr"));
    let _running = Running(child);
    let (seen, lines) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in stderr.lines().map_while(Result::ok) {
            if line.contains(PASS_DONE) || line.contains(EVICTED) {
                let _ = seen.send(line);
            }
        }
    });
    let line = stdout.lines().next().expect("a start line").expect("read");
    let reapi = line
        .split(' ')
        .find_map(|field| field.strip_prefix("reapi="))
        .unwrap_or_else(|| panic!("no reapi= in {line:?}"))
        .to_owned();
    let line = lines.recv_timeout(PROMPT).expect("the pass's last line");
    assert!(line.contains(PASS_DONE), "{line}");

    let tokio = tokio::runtime::Runtime::new().expect("runtime");
    let (first, missing) = tokio.block_on(async {
        let channel = Endpoint::from_shared(format!("http://{reapi}"))
            .expect("endpoint")
            .connect()
            .await
            .expect("connect");
        let cas = Transfers::over(channel.clone(), TransferConfig::default());
        let mut blobs = Vec::new();
        for i in 0..20_u8 {
            blobs.push(put(&cas, vec![i; MIB]).await);
        }
        let line = lines.recv_timeout(PROMPT).expect("an eviction round");
        assert!(line.contains(EVICTED), "{line}");
        let request = FindMissingBlobsRequest {
            blob_digests: vec![blobs[0].clone()],
            ..FindMissingBlobsRequest::default()
        };
        let answer = ContentAddressableStorageClient::new(channel)
            .find_missing_blobs(request)
            .await
            .expect("FindMissingBlobs");
        (blobs[0].clone(), answer.into_inner().missing_blob_digests)
    });
    assert_eq!(missing, [first], "the least recently used blob was kept");
}

/// The start-up pass's last line (`kbf-server`'s `startup_pass`).
const PASS_DONE: &str = "start-up pass over the cold store directory done";

/// What a round of eviction that evicted logs (`kbf-front`'s `Cache::evict_round`).
const EVICTED: &str = "evicted the least recently used objects";

/// The binary on a one-node farm in a fresh directory `name` whose stand-in `podman`
/// runs `steps` as each action, once its start line is out: the process, the REAPI
/// address the line names, the worker listener's port, the image an action names and
/// the directory.
fn start_farm(name: &str, steps: &[&str]) -> (Running, SocketAddr, u16, String, PathBuf) {
    let dir = scratch(name);
    pki(&dir);
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).expect("mkdir");
    let image = podman(&bin, steps);
    let port = free_port();
    let mut child = farm(&dir, &bin, &image_path(&bin), farm_flags(&dir, port))
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn");
    let mut stdout = BufReader::new(child.stdout.take().expect("stdout"));
    let mut line = String::new();
    stdout.read_line(&mut line).expect("a start line");
    // The rest of stdout is drained, so a later line never meets a closed pipe.
    std::thread::spawn(move || std::io::copy(&mut stdout, &mut std::io::sink()));
    let reapi: SocketAddr = line
        .trim_end()
        .split(' ')
        .find_map(|field| field.strip_prefix("reapi="))
        .unwrap_or_else(|| panic!("no reapi= in {line:?}"))
        .parse()
        .expect("an address");
    (Running(child), reapi, port, image, dir)
}

/// Runs an action whose command is `true` and whose platform is `properties` through
/// the REAPI listener at `reapi`; returns its result.
async fn run_action(reapi: SocketAddr, properties: Vec<Property>) -> ActionResult {
    let channel = Endpoint::from_shared(format!("http://{reapi}"))
        .expect("endpoint")
        .connect()
        .await
        .expect("connect");
    let cas = Transfers::over(channel.clone(), TransferConfig::default());
    let command = ReapiCommand {
        arguments: vec!["true".to_owned()],
        ..ReapiCommand::default()
    };
    let action = Action {
        command_digest: Some(put(&cas, command.encode_to_vec()).await),
        input_root_digest: Some(put(&cas, Directory::default().encode_to_vec()).await),
        platform: Some(Platform { properties }),
        ..Action::default()
    };
    let action = put(&cas, action.encode_to_vec()).await;
    tokio::time::timeout(PROMPT, execute(channel, action))
        .await
        .expect("the action ran in time")
}

/// The node that ran `result`'s action, as the server records it.
fn ran_on(result: &ActionResult) -> &str {
    let metadata = result.execution_metadata.as_ref().expect("metadata");
    &metadata.worker
}

/// Process `pid`'s TCP sockets, from its descriptors (`/proc/<pid>/fd`) and the rows
/// of `/proc/<pid>/net/tcp` and `tcp6` they own: each one's local port, remote port and
/// state (`0A` LISTEN, `01` ESTABLISHED, ...).
fn tcp_sockets(pid: u32) -> Vec<(u16, u16, String)> {
    let mut inodes = std::collections::BTreeSet::new();
    for fd in std::fs::read_dir(format!("/proc/{pid}/fd")).expect("list fds") {
        let Ok(target) = std::fs::read_link(fd.expect("an fd").path()) else {
            continue; // closed since it was listed
        };
        let target = target.to_string_lossy().into_owned();
        if let Some(inode) = target
            .strip_prefix("socket:[")
            .and_then(|t| t.strip_suffix(']'))
        {
            inodes.insert(inode.to_owned());
        }
    }
    let port = |address: &str| {
        let hex = address.rsplit(':').next().expect("a port");
        u16::from_str_radix(hex, 16).expect("a hex port")
    };
    let mut sockets = Vec::new();
    for table in ["tcp", "tcp6"] {
        let text = std::fs::read_to_string(format!("/proc/{pid}/net/{table}")).expect("read");
        for row in text.lines().skip(1) {
            let fields: Vec<&str> = row.split_whitespace().collect();
            let (Some(local), Some(remote), Some(state), Some(inode)) =
                (fields.get(1), fields.get(2), fields.get(3), fields.get(9))
            else {
                continue;
            };
            if inodes.contains(*inode) {
                sockets.push((port(local), port(remote), (*state).to_owned()));
            }
        }
    }
    sockets
}

/// Catches the node's own daemon reaching its server half over the network (dialled
/// to `--server` over mutual TLS, as a remote daemon does) rather than by calls: one
/// action runs on the own daemon (`node-1`, the container driver), and afterwards the
/// process holds no TCP connection with the worker listener's port at either end,
/// while the listener itself is still bound for remote daemons.
#[test]
fn the_own_daemon_reaches_the_server_half_by_calls_alone() {
    let (running, reapi, port, image, _dir) = start_farm("own-daemon", &["echo", "ran"]);
    let tokio = tokio::runtime::Runtime::new().expect("runtime");
    let image = Property {
        name: "container-image".to_owned(),
        value: image,
    };
    let result = tokio.block_on(run_action(reapi, vec![image]));
    assert_eq!(result.exit_code, 0, "{result:?}");
    assert_eq!(ran_on(&result), "node-1");
    let sockets = tcp_sockets(running.0.id());
    let on_worker: Vec<_> = sockets
        .iter()
        .filter(|(local, remote, state)| state != "0A" && (*local == port || *remote == port))
        .collect();
    assert!(
        on_worker.is_empty(),
        "connections on the worker port {port}: {on_worker:?}"
    );
    assert!(
        sockets
            .iter()
            .any(|(local, _, state)| *local == port && state == "0A"),
        "the worker listener on {port} is not bound: {sockets:?}"
    );
}

/// Catches the worker listener dropped, or not serving a remote daemon beside the
/// node's own: a daemon in this test (`remote-1`, the fake driver, labelled
/// `pool=remote`) dials the process's worker listener over mutual TLS with its own
/// certificate, registers, and runs an action that only it can take (its platform
/// names `label.pool=remote`); the server records `remote-1` as the node that ran it.
#[test]
fn a_remote_daemon_registers_through_the_worker_listener_and_runs_an_action() {
    let (_running, reapi, port, _image, dir) = start_farm("remote-daemon", &["echo", "ran"]);
    let tls = kbf_daemon::TlsFiles {
        ca_cert: dir.join("ca.pem"),
        cert: dir.join("remote.pem"),
        key: dir.join("remote.key"),
        server_name: Some("localhost".to_owned()),
    };
    let server = format!("https://127.0.0.1:{port}");
    let config = kbf_daemon::DaemonConfig::new(server, tls, "remote-1".to_owned());
    let report = kbf_daemon::NodeReport::detect(&[])
        .expect("detect")
        .with_entries([("label.pool", "remote")]);
    let runtime = std::sync::Arc::new(kbf_daemon::FakeRuntime::new(Duration::ZERO));
    let tokio = tokio::runtime::Runtime::new().expect("runtime");
    let remote = {
        let _context = tokio.enter();
        kbf_daemon::Daemon::new(config, runtime, report).expect("a remote daemon")
    };
    let remote = tokio.spawn(remote.run(std::future::pending()));
    let pool = Property {
        name: "label.pool".to_owned(),
        value: "remote".to_owned(),
    };
    let result = tokio.block_on(run_action(reapi, vec![pool]));
    assert_eq!(result.exit_code, 0, "{result:?}");
    assert_eq!(ran_on(&result), "remote-1");
    remote.abort();
}
