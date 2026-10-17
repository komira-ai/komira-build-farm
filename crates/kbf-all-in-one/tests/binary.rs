//! The `kbf-all-in-one` binary end to end: one action, executed through the REAPI
//! listener of the server half, run by the daemon half's container driver over a
//! stand-in `podman` (`kbf-fake-podman`, which runs the action as a plain process) and a
//! cgroup tree of plain directories, its outputs, stdout and stderr stored through the
//! in-process data plane and read back over REAPI. And the starts each half refuses.

#![cfg(target_os = "linux")]

use std::io::{BufRead as _, BufReader};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
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
use tonic::transport::{Channel, Endpoint};

#[path = "support/node.rs"]
mod node;

use node::{
    PROMPT, Running, cgroup_tree, files_holding, free_port, id_files, image_path, input_bytes, pki,
    podman,
};

// The stand-in `podman` runs each action as `kbf-act` beside it: a link to this test
// binary, which answers as `kbf-act` through it.
kbf_testkit::act_test_hook!();

const BIN: &str = env!("CARGO_BIN_EXE_kbf-all-in-one");

/// The stand-in `podman` (`node::podman`).
const FAKE_PODMAN: &str = env!("CARGO_BIN_EXE_kbf-fake-podman");

/// A fresh directory for one test.
fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("kbf-all-in-one")
        .join(format!("{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch");
    dir
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
    let image = podman(&bin, FAKE_PODMAN, &steps);
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
    podman(&bin, FAKE_PODMAN, &["true"]);
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

/// The farm in `dir` started with `flags`: running, with its REAPI and operator API
/// addresses from its start line.
fn start_farm(dir: &Path, bin: &Path, flags: &[String]) -> (Running, SocketAddr, SocketAddr) {
    let mut child = farm(dir, bin, &image_path(bin), flags.to_vec())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn");
    let stdout = BufReader::new(child.stdout.take().expect("stdout"));
    let running = Running(child);
    let line = stdout.lines().next().expect("a start line").expect("read");
    let field = |name: &str| -> SocketAddr {
        line.split(' ')
            .find_map(|field| field.strip_prefix(name))
            .unwrap_or_else(|| panic!("no {name} in {line:?}"))
            .parse()
            .expect("an address")
    };
    (running, field("reapi="), field("api="))
}

/// Runs one action whose input root holds `input` as `in/input.bin`; the action
/// succeeds.
fn run_with_input(reapi: SocketAddr, image: &str, input: &[u8]) {
    let tokio = tokio::runtime::Runtime::new().expect("runtime");
    tokio.block_on(async {
        let channel = Endpoint::from_shared(format!("http://{reapi}"))
            .expect("endpoint")
            .connect()
            .await
            .expect("connect");
        let cas = Transfers::over(channel.clone(), TransferConfig::default());
        let file = kbf_proto::reapi::FileNode {
            name: "input.bin".to_owned(),
            digest: Some(put(&cas, input.to_vec()).await),
            ..kbf_proto::reapi::FileNode::default()
        };
        let inner = Directory {
            files: vec![file],
            ..Directory::default()
        };
        let root = Directory {
            directories: vec![kbf_proto::reapi::DirectoryNode {
                name: "in".to_owned(),
                digest: Some(put(&cas, inner.encode_to_vec()).await),
            }],
            ..Directory::default()
        };
        let command = ReapiCommand {
            arguments: vec!["true".to_owned()],
            ..ReapiCommand::default()
        };
        let action = Action {
            command_digest: Some(put(&cas, command.encode_to_vec()).await),
            input_root_digest: Some(put(&cas, root.encode_to_vec()).await),
            platform: Some(Platform {
                properties: vec![Property {
                    name: "container-image".to_owned(),
                    value: image.to_owned(),
                }],
            }),
            ..Action::default()
        };
        let action = put(&cas, action.encode_to_vec()).await;
        let result = tokio::time::timeout(PROMPT, execute(channel, action))
            .await
            .expect("the action ran in time");
        assert_eq!(result.exit_code, 0, "{result:?}");
    });
}

/// `GET /metrics` on the operator API at `api`: each sample by name.
fn metrics(api: SocketAddr) -> std::collections::BTreeMap<String, u64> {
    use std::io::{Read as _, Write as _};
    let mut stream = std::net::TcpStream::connect(api).expect("connect");
    let request = "GET /metrics HTTP/1.1\r\nHost: kbf\r\nConnection: close\r\n\r\n";
    stream.write_all(request.as_bytes()).expect("request");
    let mut answer = String::new();
    stream.read_to_string(&mut answer).expect("answer");
    let samples = answer.lines().filter(|line| !line.starts_with('#'));
    samples
        .filter_map(|line| {
            let (name, value) = line.split_once(' ')?;
            Some((name.to_owned(), value.parse().ok()?))
        })
        .collect()
}

/// Waits until the write tier holds no block: everything uploaded is flushed. Returns
/// the samples then.
fn flushed(api: SocketAddr) -> std::collections::BTreeMap<String, u64> {
    let deadline = std::time::Instant::now() + PROMPT;
    loop {
        let samples = metrics(api);
        if samples.get("kbf_write_tier_used_bytes") == Some(&0) {
            return samples;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "not flushed: {samples:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Asserts `input` is on the node's disk once: one inode under the hot store and the
/// data directory holds it, the input cache's entry, of the input's size; and the one
/// cache's fill counts it once, beside no flushed copy.
fn on_disk_once(dir: &Path, input: &[u8], samples: &std::collections::BTreeMap<String, u64>) {
    let digest = kbf_transfer::digest_of(input);
    let copies = files_holding(&[dir.join("hot"), dir.join("data")], input);
    let [(path, size)] = &copies[..] else {
        panic!(
            "the input is on the disk {} times: {copies:?}",
            copies.len()
        );
    };
    let name = format!("{}-{}", digest.hash, digest.size_bytes);
    assert!(path.starts_with(dir.join("data/cache")), "{path:?}");
    assert!(path.ends_with(&name), "{path:?}");
    assert_eq!(*size, input.len() as u64);
    let sample = |name: &str| {
        *samples
            .get(name)
            .unwrap_or_else(|| panic!("no {name}: {samples:?}"))
    };
    assert_eq!(
        sample("kbf_write_tier_flushed_copy_bytes"),
        0,
        "{samples:?}"
    );
    assert_eq!(sample("kbf_local_cache_input_bytes"), input.len() as u64);
    assert_eq!(
        sample("kbf_local_cache_storage_bytes"),
        sample("kbf_write_tier_used_bytes") + sample("kbf_write_tier_flushed_copy_bytes")
    );
    assert_eq!(sample("kbf_local_cache_fill_bytes"), input.len() as u64);
}

/// Catches the input cache and local storage keeping two copies of a blob on one disk
/// (the "two directories" mutant: the hot store keeps flushed copies beside the input
/// cache's entries), a fill metric that counts a blob twice or leaves the hot store
/// out, and a restart after a crash that rebuilds the one index with a duplicate or a
/// leak. A client uploads an input of 3 MiB (every blob its own block on the hot
/// store, so each is flushed at once); the action that reads it runs; once the hot
/// store is flushed, one inode under the hot store and the data directory holds the
/// input (the input cache's entry, which the lease linked) and `/metrics` counts it
/// once. The process is then killed (SIGKILL) and started again, and the same action
/// runs again: still one inode, counted once.
#[test]
fn an_input_is_on_the_nodes_disk_once() {
    let dir = scratch("one-cache");
    pki(&dir);
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).expect("mkdir");
    let image = podman(&bin, FAKE_PODMAN, &["echo", "ran"]);
    let mut flags = farm_flags(&dir, free_port());
    flags.extend([
        "--api-listen=127.0.0.1:0".to_owned(),
        "--write-own-segment-bytes=1".to_owned(),
    ]);
    let input = input_bytes();
    let (running, reapi, api) = start_farm(&dir, &bin, &flags);
    run_with_input(reapi, &image, &input);
    on_disk_once(&dir, &input, &flushed(api));

    let pid = rustix::process::Pid::from_child(&running.0);
    rustix::process::kill_process(pid, rustix::process::Signal::KILL).expect("SIGKILL");
    drop(running);
    let (running, reapi, api) = start_farm(&dir, &bin, &flags);
    run_with_input(reapi, &image, &input);
    on_disk_once(&dir, &input, &flushed(api));
    let pid = rustix::process::Pid::from_child(&running.0);
    rustix::process::kill_process(pid, rustix::process::Signal::TERM).expect("SIGTERM");
    assert!(running.wait().success());
}
