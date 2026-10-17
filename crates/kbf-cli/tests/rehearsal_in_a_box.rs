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
//! is a directory (no MinIO), the container driver runs the stand-in `crun` over a
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
use node::{cgroup_tree, crun, files_holding, id_files, image_path, input_bytes};
use pki::Pki;
use procs::{HANG, Proc, field};
use prost::Message as _;
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Endpoint};
use units::{Rendered, config, rendered};

// The stand-in `crun` runs each action as `kbf-act` beside it: a link to this test
// binary, which answers as `kbf-act` through it.
kbf_testkit::act_test_hook!();

/// The stand-in `crun` (`node::crun`).
const FAKE_CRUN: &str = env!("CARGO_BIN_EXE_kbf-cli-fake-crun");

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
    let dir = node::scratch_root()
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
    /// The image the actions name: the one unpacked in each node's image store.
    image: String,
    storage_nodes: usize,
    /// The ids the frontend must list as connected, sorted.
    node_ids: Vec<String>,
}

impl Farm {
    /// Renders the five units and starts a process from each.
    ///
    /// The flags a test box cannot take as rendered, and what each is changed to:
    /// - the nodes' `--cold-store=s3://...` (no MinIO here) is a `file://` directory;
    /// - `--driver=container` stays, over the stand-in `crun`: `--oci-runtime=` names it,
    ///   the id tools are on `PATH`, their cgroup tree
    ///   (`--cgroup-root`, `--self-cgroup`) is planted and `--id-files` stands for
    ///   `/etc/subuid`.
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
            let image = crun(&bin, &root.join("data"), FAKE_CRUN, &steps);
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
        let image = crun(&bin, &root.join("data"), FAKE_CRUN, &steps);
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
    /// the unit's `--oci-runtime` made the stand-in `crun`, the id tools on `PATH` and
    /// the planted cgroup tree and id files.
    fn launch(&mut self, dir: &Path, root: &Path, name: &str, unit: Rendered, bin: Option<&Path>) {
        let (unit, cwd, mut env) = match bin {
            Some(bin) => {
                let mut flags = cgroup_tree(bin);
                flags.push(format!("--id-files={}", id_files(root).display()));
                let env = vec![("PATH".to_owned(), image_path(bin))];
                // The rendered unit names `/usr/bin/crun`: this box's is the stand-in.
                let runtime = [format!("--oci-runtime={}", bin.join("crun").display())];
                // The stand-in finds its state beside the link it was run through.
                (unit.replace(&runtime).add(&flags), bin, env)
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
            let ended = self.procs.iter_mut().find_map(|p| {
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
    // The stand-in `crun` runs its planted steps whatever the arguments are.
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

/// Reads every blob back through the frontend after one storage node is killed.
///
/// NOT RUN until the real binaries wire R=2 (`with_chain` in `kbf storage-and-daemon`,
/// the open R=2 glue change; the frontend routes by the map already, see
/// [`three_storage_nodes_run_an_action`]): at R=1 the killed node is the only owner of
/// its blobs, so this fails by construction. Remove the `ignore` when R=2 is in; the
/// flags the units render are the ones R=2 reads, so it needs no other change.
#[test]
#[ignore = "needs R=2 wired in the real binaries"]
fn a_killed_storage_node_loses_no_blob() {
    let mut farm = Farm::start("kill", 3);
    farm.wait_ready();
    let tokio = tokio::runtime::Runtime::new().expect("runtime");
    let (cas, blobs) = tokio.block_on(async {
        let cas = Transfers::over(farm.channel().await, TransferConfig::default());
        let blobs = write_and_read(&cas).await;
        (cas, blobs)
    });
    // Node 2 holds some of them (and the partner's copies are in place) before it dies.
    farm.wait("blobs on node 2", |f| {
        blobs
            .iter()
            .flat_map(|b| f.holders(b))
            .any(|n| n == 2)
            .then_some(())
    });
    farm.procs[2].kill();
    tokio.block_on(async {
        for bytes in &blobs {
            let read = cas
                .get(&digest_of(bytes))
                .await
                .expect("a blob lost with the node");
            assert_eq!(&read, bytes);
        }
    });
}
