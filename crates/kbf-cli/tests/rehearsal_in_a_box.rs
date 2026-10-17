//! A rehearsal of a deployment in a box: a frontend, three `storage-and-daemon` nodes and
//! one plain daemon, each a real `kbf` process started from the flags `kbf-deploy` renders
//! into its unit (`kbf_deploy::linux::modes::render`, the `ExecStart=` line, nothing
//! written by hand), on loopback addresses of their own, under certificates of the
//! rows the farm's certificate table has. A buck2-shaped client then drives the frontend:
//! CAS writes and reads, an `Execute` of an action (inputs, `Command` and `Action`
//! uploaded first), its outputs read back, and a repeat `Execute` answered from the
//! action cache.
//!
//! What it catches is the class of defect that makes a rendered unit not start or not
//! connect: a unit that omits flags its mode needs (the frontend with no `--storage`, a
//! node with no storage-half flags), a daemon dialling a name the frontend's certificate
//! does not hold (every node here has an address of its own, so `KBF_HOST_NAME` is not
//! the frontend's name), a certificate a deployed node would be refused, a unit kbf
//! refuses at start. Not covered: systemd itself, the held scripts and the host setup.
//!
//! The deviations from the rendered units are listed in [`Farm::start`]: the cold store
//! is a directory (no MinIO), the container driver runs the stand-in `podman` over a
//! planted cgroup tree and id files.

#![cfg(target_os = "linux")]

#[path = "../../kbf-all-in-one/tests/support/node.rs"]
mod node;
#[path = "rehearsal/pki.rs"]
mod pki;
#[path = "rehearsal/procs.rs"]
mod procs;
#[path = "rehearsal/units.rs"]
mod units;

use std::io::{Read as _, Write as _};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use kbf_deploy::linux::Mode;
use kbf_deploy::linux::verify_cert::Row;
use kbf_proto::google::longrunning::operation;
use kbf_proto::reapi::action_cache_client::ActionCacheClient;
use kbf_proto::reapi::execution_client::ExecutionClient;
use kbf_proto::reapi::platform::Property;
use kbf_proto::reapi::{
    Action, ActionResult, Command as ReapiCommand, Digest, Directory, DirectoryNode,
    ExecuteRequest, ExecuteResponse, FileNode, GetActionResultRequest, Platform,
};
use kbf_transfer::{Cas as _, TransferConfig, Transfers, digest_of};
use node::{cgroup_tree, files_holding, id_files, image_path, input_bytes, podman};
use pki::Pki;
use procs::{HANG, Proc, field};
use prost::Message as _;
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Endpoint};
use units::{Rendered, config, rendered};

// The stand-in `podman` runs each action as `kbf-act` beside it: a link to this test
// binary, which answers as `kbf-act` through it.
kbf_testkit::act_test_hook!();

/// The stand-in `podman` (`node::podman`).
const FAKE_PODMAN: &str = env!("CARGO_BIN_EXE_kbf-cli-fake-podman");

/// The bytes the action writes to its one output file.
const OUTPUT: &[u8] = b"a file\n";

/// The node id of storage-and-daemon node `n` (from 1); the plain daemon's is `daemon-1`.
fn node_id(n: usize) -> String {
    format!("node-{n}")
}

const DAEMON: &str = "daemon-1";

/// A loopback address of its own for each process (the whole of `127/8` is loopback on
/// Linux): the frontend's, then each node's, then the plain daemon's.
fn loopback(n: usize) -> String {
    format!("127.0.0.{}", n + 1)
}

/// A fresh directory for one test.
fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("kbf-rehearsal")
        .join(format!("{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch");
    dir
}

/// The last component of `dir`.
fn file_name(dir: &Path) -> String {
    dir.file_name()
        .expect("a name")
        .to_string_lossy()
        .into_owned()
}

/// `n` distinct ports nothing listens on now.
fn ports(n: usize) -> Vec<u16> {
    let held: Vec<TcpListener> = (0..n)
        .map(|_| TcpListener::bind("127.0.0.1:0").expect("bind"))
        .collect();
    held.iter()
        .map(|l| l.local_addr().expect("addr").port())
        .collect()
}

/// `GET path` of the operator API at `127.0.0.1:api`: the status code and the body, or
/// `None` while nothing answers.
fn get(api: u16, path: &str) -> Option<(u16, String)> {
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", api)).ok()?;
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: kbf\r\nConnection: close\r\n\r\n"
    )
    .ok()?;
    let mut text = String::new();
    stream.read_to_string(&mut text).ok()?;
    let (head, body) = text.split_once("\r\n\r\n")?;
    let code = head.split_whitespace().nth(1)?.parse().ok()?;
    Some((code, body.to_owned()))
}

/// The ids of the nodes the frontend lists as connected, sorted; `None` while it does
/// not answer.
fn connected(api: u16) -> Option<Vec<String>> {
    let (code, body) = get(api, "/v1/nodes")?;
    (code == 200).then_some(())?;
    let view: serde_json::Value = serde_json::from_str(&body).ok()?;
    let mut ids: Vec<String> = view["nodes"]
        .as_array()?
        .iter()
        .filter(|n| n["connected"] == true)
        .filter_map(|n| n["node_id"].as_str().map(str::to_owned))
        .collect();
    ids.sort();
    Some(ids)
}

/// How many storage members `GET /v1/members` lists, and whether every one is `active`: a
/// node that registered rebuilds its index first (`catching_up`), and each promotion is a
/// new map generation, which a client that wrote meanwhile would be told is stale.
fn members_active(api: u16) -> Option<(usize, bool)> {
    let (code, body) = get(api, "/v1/members")?;
    (code == 200).then_some(())?;
    let view: serde_json::Value = serde_json::from_str(&body).ok()?;
    let members = view["members"].as_array()?;
    Some((
        members.len(),
        members.iter().all(|m| m["state"] == "active"),
    ))
}

/// What every process of the farm is told about the frontend.
struct Wiring {
    reapi: u16,
    worker: u16,
    api: u16,
    /// The storage URL the frontend dials: node 1's.
    storage_url: String,
}

/// The settings of one process: its own address, node id, directories under `root` and
/// storage port, and the farm's wiring. The frontend is one name for every dialler
/// (`KBF_FRONTEND_NAME`), not the process's own (`KBF_HOST_NAME`).
fn settings(
    root: &Path,
    host: &str,
    node_id: &str,
    storage_port: u16,
    wiring: &Wiring,
    image: &str,
) -> kbf_deploy::linux::LinuxConfig {
    let at = |name: &str| root.join(name).display().to_string();
    config(&[
        ("KBF_HOST_NAME", host.to_owned()),
        ("KBF_FRONTEND_NAME", "localhost".to_owned()),
        ("KBF_LISTEN_ADDRESS", host.to_owned()),
        ("KBF_NODE_ID", node_id.to_owned()),
        // Actions get RAM less the reserved shares: 10 GiB, over the 4 GiB an action asks by default.
        ("KBF_RAM_GIB", "16".to_owned()),
        ("MINIO_RESERVATION_GIB", "1".to_owned()),
        ("KBF_SERVER_RESERVATION_GIB", "1".to_owned()),
        ("KBF_OS_HEADROOM_GIB", "4".to_owned()),
        ("KBF_STORAGE_URL", wiring.storage_url.clone()),
        ("KBF_REAPI_PORT", wiring.reapi.to_string()),
        ("KBF_WORKER_PORT", wiring.worker.to_string()),
        ("KBF_API_PORT", wiring.api.to_string()),
        ("KBF_STORAGE_PORT", storage_port.to_string()),
        ("KBF_ETC", at("etc")),
        ("KBF_DATA_DIR", at("data")),
        ("KBF_META_DIR", at("meta")),
        ("KBF_HOT_DIR", at("hot")),
        ("KBF_STATE_DIR", at("state")),
        ("KBF_IMAGES", image.to_owned()),
    ])
}

/// The directories a process's unit expects to exist (the installer makes them on a
/// host).
fn make_dirs(root: &Path) -> PathBuf {
    for name in ["etc", "data", "meta", "hot", "state", "cold", "bin"] {
        std::fs::create_dir_all(root.join(name)).expect("mkdir");
    }
    root.join("bin")
}

/// The running farm.
struct Farm {
    /// The frontend, then the storage-and-daemon nodes, then the plain daemon.
    procs: Vec<Proc>,
    roots: Vec<PathBuf>,
    reapi: String,
    api: u16,
    ca: Vec<u8>,
    /// The image the actions name: what the stand-in `podman` serves.
    image: String,
    storage_nodes: usize,
    /// The ids the frontend must list as connected, sorted.
    node_ids: Vec<String>,
    /// The processes (by index) a test has killed: [`Farm::wait`] does not take their
    /// exit for a crash.
    killed: Vec<usize>,
}

impl Farm {
    /// Renders the five units and starts a process from each.
    ///
    /// The flags a test box cannot take as rendered, and what each is changed to:
    /// - the nodes' `--cold-store=s3://...` (no MinIO here) is a `file://` directory;
    /// - `--driver=container` stays, over the stand-in `podman`: the nodes get it on
    ///   `PATH`, their cgroup tree (`--cgroup-root`, `--self-cgroup`) is planted and
    ///   `--id-files` stands for `/etc/subuid`.
    ///
    /// Every other flag is the unit's.
    fn start(name: &str, storage_nodes: usize) -> Self {
        let dir = scratch(name);
        let uid = rustix::process::getuid().as_raw();
        let p = ports(storage_nodes + 4);
        let (storage, spare) = (&p[3..3 + storage_nodes], p[3 + storage_nodes]);
        let wiring = Wiring {
            reapi: p[0],
            worker: p[1],
            api: p[2],
            storage_url: format!("https://{}:{}", loopback(1), storage[0]),
        };
        let pki = Pki::new();
        let steps = ["write-lines", "$UPPER/out/file.txt", "a file"];
        let mut farm = Farm {
            procs: Vec::new(),
            roots: Vec::new(),
            reapi: String::new(),
            api: wiring.api,
            ca: Vec::new(),
            image: String::new(),
            storage_nodes,
            node_ids: std::iter::once(DAEMON.to_owned())
                .chain((1..=storage_nodes).map(node_id))
                .collect(),
            killed: Vec::new(),
        };

        // The frontend: its unit as rendered, a state directory made.
        let root = dir.join("frontend");
        let etc = root.join("etc");
        make_dirs(&root);
        pki.write_ca(&etc);
        pki.issue(&etc, "worker", Row::FrontendWorker, &["localhost"]);
        pki.issue_reapi(&etc, &["localhost"]);
        // The unit names `--api-token-file <etc>/api-token`; the test's API calls only read.
        let token = etc.join("api-token");
        std::fs::write(&token, "rehearsal-api-token-of-32-or-more-visible-chars\n")
            .expect("write the API token");
        std::fs::set_permissions(&token, std::os::unix::fs::PermissionsExt::from_mode(0o600))
            .expect("the API token's mode");
        farm.ca = std::fs::read(etc.join("ca.pem")).expect("ca");
        let cfg = settings(&root, &loopback(0), "frontend-1", spare, &wiring, "");
        let unit = rendered(&cfg, uid, Mode::Frontend);
        // The unit's control socket is in a `RuntimeDirectory=` systemd makes; a test box
        // has no such directory, and the scratch directory's path is too long for a
        // socket's (`SUN_LEN`), so the socket is in the temporary directory, named for
        // the test.
        let socket = std::env::temp_dir().join(format!("{}.sock", file_name(&dir)));
        let _ = std::fs::remove_file(&socket);
        let control = format!("--handover-socket={}", socket.display());
        farm.launch(&dir, &root, "frontend", unit.replace(&[control]), None);
        let line = farm.start_line(0, "kbf-frontend ");
        farm.reapi = field(&line, "reapi=").to_owned();

        // The storage-and-daemon nodes, then the plain daemon.
        for (i, port) in storage.iter().enumerate() {
            let (id, host) = (node_id(i + 1), loopback(i + 1));
            let id = id.as_str();
            let root = dir.join(id);
            let bin = make_dirs(&root);
            let image = podman(&bin, FAKE_PODMAN, &steps);
            let etc = root.join("etc");
            pki.write_ca(&etc);
            pki.issue(&etc, "storage", Row::Storage, &[&host]);
            pki.issue(&etc, "node", Row::Daemon, &[id]);
            let bare = image.strip_prefix("docker://").expect("docker://");
            let cfg = settings(&root, &host, id, *port, &wiring, bare);
            farm.image.clone_from(&image);
            let unit = rendered(&cfg, uid, Mode::StorageAndDaemon);
            let cold = format!("--cold-store=file://{}", root.join("cold").display());
            farm.launch(&dir, &root, id, unit.replace(&[cold]), Some(&bin));
            farm.start_line(i + 1, "kbf-storage ");
        }
        let (id, host) = (DAEMON, loopback(storage_nodes + 1));
        let root = dir.join(id);
        let bin = make_dirs(&root);
        let image = podman(&bin, FAKE_PODMAN, &steps);
        let etc = root.join("etc");
        pki.write_ca(&etc);
        pki.issue(&etc, "node", Row::Daemon, &[id]);
        let bare = image.strip_prefix("docker://").expect("docker://");
        let cfg = settings(&root, &host, id, spare, &wiring, bare);
        let unit = rendered(&cfg, uid, Mode::Daemon);
        farm.launch(&dir, &root, id, unit, Some(&bin));
        farm
    }

    /// Starts the process for `unit`; those with a `bin` (the container driver's) get
    /// the stand-in `podman` on `PATH` and the planted cgroup tree and id files.
    fn launch(&mut self, dir: &Path, root: &Path, name: &str, unit: Rendered, bin: Option<&Path>) {
        let (unit, cwd, mut env) = match bin {
            Some(bin) => {
                let mut flags = cgroup_tree(bin);
                flags.push(format!("--id-files={}", id_files(root).display()));
                let env = vec![("PATH".to_owned(), image_path(bin))];
                // The stand-in finds its state beside the link it was run through.
                (unit.add(&flags), bin, env)
            }
            None => (unit, root, Vec::new()),
        };
        env.extend(unit.env.iter().cloned());
        let proc = Proc::start(name, dir, &unit.argv, &env, cwd);
        self.procs.push(proc);
        self.roots.push(root.to_owned());
    }

    /// Waits for `ready`, failing at once with every process's output if one exits, and
    /// after [`HANG`] with the same.
    fn wait<T>(&mut self, what: &str, mut ready: impl FnMut(&Farm) -> Option<T>) -> T {
        let deadline = Instant::now() + HANG;
        loop {
            let killed = &self.killed;
            let ended = self.procs.iter_mut().enumerate().find_map(|(i, p)| {
                if killed.contains(&i) {
                    return None;
                }
                p.exited()
                    .map(|s| format!("{} exited ({s}) while waiting for {what}", p.name))
            });
            if let Some(ended) = ended {
                panic!("{ended}\n{}", self.logs());
            }
            if let Some(found) = ready(self) {
                return found;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}\n{}",
                self.logs()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// SIGKILL to process `which`, as a host losing power would take it.
    fn kill(&mut self, which: usize) {
        self.procs[which].kill();
        self.killed.push(which);
    }

    fn logs(&self) -> String {
        self.procs.iter().map(Proc::log).collect()
    }

    fn start_line(&mut self, which: usize, prefix: &str) -> String {
        let name = self.procs[which].name.clone();
        self.wait(&format!("{name}'s start line"), |f| {
            f.procs[which].line(prefix)
        })
    }

    /// Waits until the frontend is ready and lists every node as connected.
    fn wait_ready(&mut self) {
        self.wait("the frontend's /readyz", |f| {
            (get(f.api, "/readyz")?.0 == 200).then_some(())
        });
        self.wait("every node connected to the frontend", |f| {
            (connected(f.api)? == f.node_ids).then_some(())
        });
        self.wait("every storage member active", |f| {
            (members_active(f.api)? == (f.storage_nodes, true)).then_some(())
        });
    }

    /// A client of the frontend's REAPI listener (TLS under the farm CA).
    async fn channel(&self) -> Channel {
        let tls = ClientTlsConfig::new()
            .ca_certificate(Certificate::from_pem(&self.ca))
            .domain_name("localhost");
        Endpoint::from_shared(format!("https://{}", self.reapi))
            .expect("endpoint")
            .tls_config(tls)
            .expect("TLS")
            .connect()
            .await
            .expect("connect to the frontend")
    }

    /// SIGTERM to each process, the daemons and nodes before the frontend (which waits
    /// for open streams), and each must exit 0.
    fn stop(mut self) {
        for i in (0..self.procs.len()).rev() {
            let status = self.procs[i].term();
            assert!(status.success(), "{status}\n{}", self.logs());
        }
    }

    /// The nodes (by index) that hold `bytes` in their hot or cold store.
    fn holders(&self, bytes: &[u8]) -> Vec<usize> {
        (1..=self.storage_nodes)
            .filter(|n| {
                let root = &self.roots[*n];
                !files_holding(&[root.join("hot"), root.join("cold")], bytes).is_empty()
            })
            .collect()
    }
}

/// Blob `i`: 100 distinct bytes (the disk scan anchors on 64).
fn blob(i: usize) -> Vec<u8> {
    format!("rehearsal blob {i:04} {}", "x".repeat(80)).into_bytes()
}

/// The action that reads `input` as `in/input.bin` and has `out/file.txt` written,
/// uploaded through `cas`; its digest.
async fn upload_action(cas: &Transfers, image: &str, input: &[u8]) -> Digest {
    let put = async |bytes: Vec<u8>| cas.put(bytes).await.expect("upload");
    let file = FileNode {
        name: "input.bin".to_owned(),
        digest: Some(put(input.to_vec()).await),
        ..FileNode::default()
    };
    let inner = Directory {
        files: vec![file],
        ..Directory::default()
    };
    let root = Directory {
        directories: vec![DirectoryNode {
            name: "in".to_owned(),
            digest: Some(put(inner.encode_to_vec()).await),
        }],
        ..Directory::default()
    };
    // The stand-in `podman` runs its planted steps whatever the arguments are.
    let command = ReapiCommand {
        arguments: vec!["true".to_owned()],
        output_paths: vec!["out/file.txt".to_owned()],
        ..ReapiCommand::default()
    };
    let action = Action {
        command_digest: Some(put(command.encode_to_vec()).await),
        input_root_digest: Some(put(root.encode_to_vec()).await),
        platform: Some(Platform {
            properties: vec![Property {
                name: "container-image".to_owned(),
                value: image.to_owned(),
            }],
        }),
        ..Action::default()
    };
    put(action.encode_to_vec()).await
}

/// Executes `action` (the action cache consulted) until its operation is done.
async fn execute(reapi: Channel, action: Digest) -> ExecuteResponse {
    let mut ops = ExecutionClient::new(reapi)
        .execute(ExecuteRequest {
            action_digest: Some(action),
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
    ExecuteResponse::decode(any.value.as_slice()).expect("response")
}

/// Writes 24 blobs through the frontend and reads them back; returns them.
async fn write_and_read(cas: &Transfers) -> Vec<Vec<u8>> {
    let blobs: Vec<Vec<u8>> = (0..24).map(blob).collect();
    for bytes in &blobs {
        let digest = cas
            .put(bytes.clone())
            .await
            .expect("write through the frontend");
        assert_eq!(digest, digest_of(bytes));
    }
    let digests: Vec<Digest> = blobs.iter().map(|b| digest_of(b)).collect();
    assert!(
        cas.missing(&digests).await.is_empty(),
        "a written blob is missing"
    );
    for bytes in &blobs {
        let read = cas
            .get(&digest_of(bytes))
            .await
            .expect("read through the frontend");
        assert_eq!(&read, bytes);
    }
    blobs
}

/// A buck2-shaped client's session against the ready farm: CAS writes and reads
/// through the frontend, then an `Execute` (inputs, `Command` and `Action` uploaded
/// first) whose output is read back through the frontend, then the same `Execute` again,
/// which the action cache answers. Returns the blobs written.
fn drive(farm: &Farm) -> Vec<Vec<u8>> {
    let tokio = tokio::runtime::Runtime::new().expect("runtime");
    tokio.block_on(async {
        let channel = farm.channel().await;
        let cas = Transfers::over(channel.clone(), TransferConfig::default());
        let blobs = write_and_read(&cas).await;

        let action = upload_action(&cas, &farm.image, &input_bytes()).await;
        let first = tokio::time::timeout(HANG, execute(channel.clone(), action.clone()))
            .await
            .expect("the action ran in time");
        let result = first
            .result
            .clone()
            .unwrap_or_else(|| panic!("no result: {first:?}"));
        assert!(!first.cached_result, "the first run was cached: {first:?}");
        assert_eq!(result.exit_code, 0, "{first:?}");
        let [file] = &result.output_files[..] else {
            panic!("one output file: {first:?}");
        };
        assert_eq!(file.path, "out/file.txt");
        assert_eq!(file.digest.as_ref(), Some(&digest_of(OUTPUT)));
        let read = cas
            .get(&digest_of(OUTPUT))
            .await
            .expect("the output, through the frontend");
        assert_eq!(read, OUTPUT);

        let again = tokio::time::timeout(HANG, execute(channel.clone(), action.clone()))
            .await
            .expect("the repeat answered in time");
        assert!(again.cached_result, "the repeat ran again: {again:?}");
        assert_eq!(again.result, first.result);
        let cached: ActionResult = ActionCacheClient::new(channel)
            .get_action_result(GetActionResultRequest {
                action_digest: Some(action),
                ..GetActionResultRequest::default()
            })
            .await
            .expect("GetActionResult")
            .into_inner();
        assert_eq!(Some(cached), first.result);
        blobs
    })
}

/// Catches a rendered unit that does not start or does not connect, for the whole
/// topology of a frontend, three storage-and-daemon nodes and a daemon: any process
/// exiting before its start line (a flag kbf refuses, a unit with its flags missing, a
/// certificate refused); a node that never registers or whose daemon half never connects
/// to the frontend (the frontend's name taken from the node's own, a certificate lacking
/// the role or name); a CAS write or read through the frontend that fails; and a process
/// not exiting 0 on SIGTERM.
#[test]
fn the_rendered_units_start_connect_and_serve_cas() {
    let mut farm = Farm::start("topology", 3);
    farm.wait_ready();
    let tokio = tokio::runtime::Runtime::new().expect("runtime");
    tokio.block_on(async {
        let cas = Transfers::over(farm.channel().await, TransferConfig::default());
        write_and_read(&cas).await;
    });
    farm.stop();
}

/// Catches what the topology test cannot, a REAPI round trip through the rendered units
/// (a frontend, one storage-and-daemon node, a daemon): inputs uploaded, `Execute`
/// answered by a daemon, the output read back through the frontend, a repeat answered
/// from the action cache (`cached_result`, and `GetActionResult` alike). One storage
/// node: the farm of one member the frontend reaches through `--storage`; see
/// [`three_storage_nodes_run_an_action`] for the map with three.
#[test]
fn one_storage_node_runs_an_action_and_repeats_it_from_the_cache() {
    let mut farm = Farm::start("one-node", 1);
    farm.wait_ready();
    drive(&farm);
    farm.stop();
}

/// The same session on three storage nodes, and that the map spreads the blobs (each
/// node holds some).
///
/// Catches a frontend that reaches storage through its one `--storage` node (the
/// `Owners::single` and `Backend::remote` it started with) while the daemons route by the
/// map: a client's CAS writes would all land on that node, a daemon puts an action's
/// output on its rendezvous owner, so when that is another node the frontend asks the
/// `--storage` node `prepare_action_result`, is told the blob "is not held", and fails the
/// lease as an infrastructure failure on every node in turn. The frontend routes by the
/// members' map (`kbf_front::Routed`), so the output is found at its owner, the entry is
/// committed with its blobs at the action's owner, and the repeat is a cache hit.
#[test]
fn three_storage_nodes_run_an_action() {
    let mut farm = Farm::start("three-nodes", 3);
    farm.wait_ready();
    let blobs = drive(&farm);
    // The map spreads the blobs: each of the three nodes holds some (a frontend that
    // sent everything to its one `--storage` node leaves two empty).
    let spread = farm.wait("blobs on every storage node", |f| {
        let mut held = vec![0_usize; f.storage_nodes + 1];
        blobs
            .iter()
            .flat_map(|b| f.holders(b))
            .for_each(|n| held[n] += 1);
        held[1..].iter().all(|n| *n > 0).then_some(held)
    });
    eprintln!("blobs held per node (index 1-3): {spread:?}");
    farm.stop();
}

/// How many actions the kill test runs before the kill: enough that the killed node
/// has committed some action's cache entry (it commits those of which it is the first
/// owner not marked down, a third of them), so the miss-and-rerun path runs.
const ACTIONS: usize = 6;

/// Writes the blobs numbered `range` through `cas` and reads each back.
async fn write_blobs(cas: &Transfers, range: std::ops::Range<usize>) -> Vec<Vec<u8>> {
    let blobs: Vec<Vec<u8>> = range.map(blob).collect();
    for bytes in &blobs {
        let digest = cas
            .put(bytes.clone())
            .await
            .expect("a write after the kill goes on to the blob's other owner");
        assert_eq!(digest, digest_of(bytes));
    }
    for bytes in &blobs {
        let read = cas.get(&digest_of(bytes)).await.expect("read back");
        assert_eq!(&read, bytes);
    }
    blobs
}

/// Runs an action whose input is `blob(input)` and returns its digest and result.
async fn run_action(
    channel: &Channel,
    cas: &Transfers,
    image: &str,
    input: usize,
) -> (Digest, ActionResult) {
    let action = upload_action(cas, image, &blob(input)).await;
    let response = tokio::time::timeout(HANG, execute(channel.clone(), action.clone()))
        .await
        .expect("the action answered in time");
    let result = response
        .result
        .clone()
        .unwrap_or_else(|| panic!("no result: {response:?}"));
    assert_eq!(result.exit_code, 0, "{response:?}");
    let [file] = &result.output_files[..] else {
        panic!("one output file: {response:?}");
    };
    assert_eq!(file.digest.as_ref(), Some(&digest_of(OUTPUT)));
    (action, result)
}

/// What the client asks of the frontend after the kill, `round` numbering the asking
/// (fresh blobs are numbered from it). `before` is what was written before it.
///
/// Every blob and the actions' output read back. Each action's cache entry either
/// answers as it did before (`GetActionResult`, and a repeat `Execute` with
/// `cached_result` set) or is a miss (NOT_FOUND, never another error), and the repeat
/// `Execute` of a miss runs the action again to the same output and its entry is back.
/// Then writes: 24 fresh blobs and a new action. Returns the fresh blobs and the number
/// of entries that were a miss.
async fn survivors(
    channel: &Channel,
    cas: &Transfers,
    image: &str,
    before: (&[Vec<u8>], &[(Digest, ActionResult)]),
    round: usize,
) -> (Vec<Vec<u8>>, usize) {
    let (blobs, actions) = before;
    for bytes in blobs {
        let read = cas
            .get(&digest_of(bytes))
            .await
            .expect("a blob lost with the node");
        assert_eq!(&read, bytes);
    }
    let read = cas
        .get(&digest_of(OUTPUT))
        .await
        .expect("the actions' output lost with the node");
    assert_eq!(read, OUTPUT);
    let lookup = async |action: &Digest| {
        ActionCacheClient::new(channel.clone())
            .get_action_result(GetActionResultRequest {
                action_digest: Some(action.clone()),
                ..GetActionResultRequest::default()
            })
            .await
    };
    let mut reruns = 0;
    for (action, result) in actions {
        let hit = match lookup(action).await {
            Ok(entry) => Some(entry.into_inner()),
            // A miss costs the action a rerun; any other answer is a failure of the farm.
            Err(status) if status.code() == tonic::Code::NotFound => None,
            Err(status) => panic!("GetActionResult after the kill: {status:?}"),
        };
        if let Some(entry) = &hit {
            assert_eq!(&entry.output_files, &result.output_files);
        }
        let again = tokio::time::timeout(HANG, execute(channel.clone(), action.clone()))
            .await
            .expect("the repeat answered in time");
        let ran = again.result.as_ref().expect("a result");
        assert_eq!(ran.exit_code, 0, "{again:?}");
        assert_eq!(ran.output_files, result.output_files, "{again:?}");
        assert_eq!(again.cached_result, hit.is_some(), "{again:?}");
        reruns += usize::from(hit.is_none());
        // Whichever it was, the entry is there now.
        let entry = lookup(action).await.expect("the entry after the repeat");
        assert_eq!(entry.into_inner().output_files, result.output_files);
    }
    // Writes: fresh blobs, then a fresh action (its input a fresh blob too).
    let fresh = 1000 * round;
    let written = write_blobs(cas, fresh..fresh + 24).await;
    run_action(channel, cas, image, fresh + 500).await;
    (written, reruns)
}

/// No blob is lost with a storage node, and the farm keeps taking writes.
///
/// A frontend, three storage-and-daemon nodes and a daemon, from the rendered units
/// (R=2: the map is the one the frontend and the nodes share). The client writes 24
/// blobs and `Execute`s six actions (six action cache entries, each committed with its
/// output at its owners). Once every blob is on two storage nodes, the node holding
/// most of them is killed with SIGKILL, and the frontend is asked, with the node dead
/// and not yet marked down anywhere the client can see:
/// - every blob written before the kill reads back, and so does the actions' output;
/// - each action's cache entry either answers as before (`GetActionResult` and a repeat
///   `Execute` agree, `cached_result` set) or is a miss (NOT_FOUND), in which case the
///   repeat `Execute` runs it again to the same output and the entry is back. An entry is
///   committed at one owner and not replicated (the design's "AC miss, rerun"), so the
///   entries the dead node committed are misses; a miss costs a rerun and never a wrong
///   or failed answer;
/// - writes after the kill succeed: 24 more blobs, whose first owner is the dead node
///   for about a third of them, go on to the other owner, and a new action runs to a
///   result committed at an owner that is alive;
/// - the same holds once the frontend lists the dead node's daemon as gone (the
///   frontend's connection list, not the map: the member's lease outlives this test, so
///   the map still has the node up and a `Down` owner is not reached here), and then
///   every entry is a hit: what the reruns committed is on a live node.
///
/// Catches a farm that writes a blob once (a primary that does not send its copy, a
/// copy that is owed and never made: the replica check before the kill, then the read
/// after it, fail), a frontend that fails a write because the first owner is dead
/// instead of going on to the next, and a commit that fails because its first owner is
/// dead. It reaches the write failover of the frontend's raw upload and of
/// `Router::commit_with_closure`; `Router::put` and skipping a `Down` owner are covered
/// by the `kbf-route` tests (`a_write_goes_on_to_the_next_owner_when_the_first_cannot_be_reached`,
/// `a_write_skips_an_owner_that_is_down`), not here. Planted to see it fail: `Chain::plan` answering `Plan::Local` for the primary
/// (no copy is written, so the blobs are on one node each).
#[test]
fn a_killed_storage_node_loses_no_blob() {
    let mut farm = Farm::start("kill", 3);
    farm.wait_ready();
    let tokio = tokio::runtime::Runtime::new().expect("runtime");
    let (channel, cas, blobs, actions) = tokio.block_on(async {
        let channel = farm.channel().await;
        let cas = Transfers::over(channel.clone(), TransferConfig::default());
        let blobs = write_and_read(&cas).await;
        let mut actions = Vec::new();
        for input in 100..100 + ACTIONS {
            actions.push(run_action(&channel, &cas, &farm.image, input).await);
        }
        (channel, cas, blobs, actions)
    });
    // Every blob is on two storage nodes before the node dies: the primary sent the
    // partner's copy before it answered, or owes it and the repair worker makes it.
    farm.wait("every blob on two storage nodes", |f| {
        blobs.iter().all(|b| f.holders(b).len() >= 2).then_some(())
    });
    let held = |n: usize| {
        blobs
            .iter()
            .filter(|b| farm.holders(b).contains(&n))
            .count()
    };
    let victim = (1..=farm.storage_nodes)
        .max_by_key(|n| held(*n))
        .expect("a node");
    assert!(held(victim) > 0, "no blob on any node");
    eprintln!("killing node {victim}, which holds {} blobs", held(victim));
    farm.kill(victim);

    let check = |farm: &Farm, round: usize| {
        tokio.block_on(survivors(
            &channel,
            &cas,
            &farm.image,
            (&blobs, &actions),
            round,
        ))
    };
    let (first, reruns) = check(&farm, 1);
    eprintln!("{reruns} of {ACTIONS} cache entries were gone with the node: rerun");
    // The farm is fixed (its map, the digests, the image), so the node holding most blobs
    // also committed at least one entry: without this a farm where the dead node
    // committed none would pass with the commit failover never exercised.
    assert!(
        reruns > 0,
        "no cache entry was committed by the killed node: the commit failover is unexercised"
    );

    // The frontend lists the killed node's daemon as gone; the same again.
    let gone: Vec<String> = farm
        .node_ids
        .iter()
        .filter(|id| **id != node_id(victim))
        .cloned()
        .collect();
    farm.wait("the frontend to list the killed node as gone", |f| {
        (connected(f.api)? == gone).then_some(())
    });
    let (second, reruns) = check(&farm, 2);
    assert_eq!(reruns, 0, "an entry rewritten after the kill is gone again");
    // What was written after the kill is on the live nodes, whole.
    for bytes in first.iter().chain(&second) {
        let live = farm.holders(bytes).into_iter().filter(|n| *n != victim);
        assert!(
            live.count() > 0,
            "a write after the kill is on no live node"
        );
    }
    // The killed process is already gone; the others stop cleanly.
    for i in (0..farm.procs.len()).rev().filter(|i| *i != victim) {
        let status = farm.procs[i].term();
        assert!(status.success(), "{status}\n{}", farm.logs());
    }
}
