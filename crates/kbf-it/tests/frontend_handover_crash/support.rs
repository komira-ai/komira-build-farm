//! What the frontend handover crash matrix is made of: real `kbf` processes (a storage
//! node and frontends), the state file read back, a wire-level fake of either end of the
//! control socket, a fake daemon and a REAPI client over gRPC.
//!
//! Nothing here orders anything by sleeping. A process is waited for by what it prints
//! (its start line, a log line) or by its exit; the only timeouts are hang guards
//! ([`GUARD`]).

use std::collections::{BTreeMap, VecDeque};
use std::ffi::OsString;
use std::fmt::Write as _;
use std::io::{BufRead as _, BufReader, IoSlice, IoSliceMut, Write as _};
use std::mem::MaybeUninit;
use std::net::SocketAddr;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::{Duration, Instant};

use futures::channel::mpsc::{UnboundedSender, unbounded};
use kbf_proto::google::longrunning::{Operation, operation};
use kbf_proto::reapi::content_addressable_storage_client::ContentAddressableStorageClient;
use kbf_proto::reapi::execution_client::ExecutionClient;
use kbf_proto::reapi::{
    self, Action, ActionResult, BatchUpdateBlobsRequest, Command as ReapiCommand, Directory,
    ExecuteRequest, ExecuteResponse, FileNode, OutputFile, WaitExecutionRequest,
    batch_update_blobs_request,
};
use kbf_proto::worker::{
    Capability, DaemonMessage, Heartbeat, Hello, LeaseId, NodeStatus, ResultAck, ServerMessage,
    Start, daemon_message, server_message, worker_client::WorkerClient,
};
use kbf_server::handover::Message;
use prost::Message as _;
use rustix::net::{
    RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, SendAncillaryBuffer,
    SendAncillaryMessage, SendFlags, recvmsg, sendmsg,
};
use sha2::{Digest as _, Sha256};
use tokio::time::timeout;
use tonic::transport::{Channel, Endpoint};
use tonic::{Code, Streaming};

/// The longest anything is waited for: a hang guard, never an ordering.
pub const GUARD: Duration = Duration::from_secs(60);

/// The control socket's name, relative to the directory every frontend of a test runs in:
/// a socket path may not be longer than `sockaddr_un` holds, and a directory under a
/// deep target directory is.
const CONTROL: &str = "control.sock";

/// The heartbeat interval every frontend of the matrix is started with.
const HEARTBEAT_MS: &str = "100";

// ---------------------------------------------------------------------------------
// Processes

/// The `kbf` binary of this build. This crate has no `kbf` binary of its own, so the
/// first caller builds `kbf-cli`'s (a no-op when it is up to date, in the same target
/// directory and with the same flags as the test, which cargo hands down in the
/// environment) and asks cargo where the executable is: the test binary does not run
/// from a fixed place (a coverage run puts it in a build directory of its own).
pub fn kbf() -> &'static Path {
    static BIN: OnceLock<PathBuf> = OnceLock::new();
    BIN.get_or_init(|| {
        let cargo = std::env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"));
        let mut build = Command::new(cargo);
        build.args([
            "build",
            "--locked",
            "-p",
            "kbf-cli",
            "--bin",
            "kbf",
            "--message-format=json-render-diagnostics",
        ]);
        // The profile this test was built with: a test of a release build has no debug
        // assertions.
        if !cfg!(debug_assertions) {
            build.arg("--release");
        }
        let built = build
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .stderr(Stdio::inherit())
            .output()
            .expect("run cargo");
        assert!(
            built.status.success(),
            "cargo build -p kbf-cli: {}",
            built.status
        );
        String::from_utf8_lossy(&built.stdout)
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .filter(|message| message["reason"] == "compiler-artifact")
            .filter(|message| message["target"]["name"] == "kbf")
            .find_map(|message| message["executable"].as_str().map(PathBuf::from))
            .expect("cargo named the kbf executable")
    })
}

/// What a child process tells.
enum Event {
    Out(String),
    Err(String),
    Exit(ExitStatus),
}

/// A `kbf` child: its stdout and stderr lines and its exit arrive as events, so a test
/// waits for what the process says and never for a time. Only the process it spawned is
/// ever signalled.
pub struct Proc {
    name: String,
    pid: u32,
    events: Receiver<Event>,
    out: Vec<String>,
    err: Vec<String>,
    exit: Option<ExitStatus>,
}

impl Proc {
    /// `kbf <args>`, its stderr also written to `<dir>/<name>.log`.
    pub fn spawn(dir: &Dir, name: &str, args: &[String]) -> Self {
        let mut child = Command::new(kbf())
            .args(args)
            .current_dir(dir.path())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn kbf");
        let (tx, events) = channel();
        let stdout = child.stdout.take().expect("stdout");
        let out_tx = tx.clone();
        let out_reader = std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if out_tx.send(Event::Out(line)).is_err() {
                    return;
                }
            }
        });
        let stderr = child.stderr.take().expect("stderr");
        let err_tx = tx.clone();
        let mut log = std::fs::File::create(dir.path().join(format!("{name}.log"))).expect("log");
        let err_reader = std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                let _ = writeln!(log, "{line}");
                if err_tx.send(Event::Err(line)).is_err() {
                    return;
                }
            }
        });
        let pid = child.id();
        // The exit is told after the last line the process wrote, so a test that sees
        // it sees the whole log.
        std::thread::spawn(move || {
            if let Ok(status) = child.wait() {
                let _ = out_reader.join();
                let _ = err_reader.join();
                let _ = tx.send(Event::Exit(status));
            }
        });
        Self {
            name: name.to_owned(),
            pid,
            events,
            out: Vec::new(),
            err: Vec::new(),
            exit: None,
        }
    }

    fn take(&mut self, event: Event) {
        match event {
            Event::Out(line) => self.out.push(line),
            Event::Err(line) => self.err.push(line),
            Event::Exit(status) => self.exit = Some(status),
        }
    }

    /// Takes the events that have arrived.
    fn drain(&mut self) {
        while let Ok(event) = self.events.try_recv() {
            self.take(event);
        }
    }

    /// Waits for the next event, within the hang guard measured from `since`.
    fn pump(&mut self, since: Instant) -> bool {
        let left = GUARD.saturating_sub(since.elapsed());
        match self.events.recv_timeout(left) {
            Ok(event) => {
                self.take(event);
                true
            }
            Err(RecvTimeoutError::Timeout) => false,
            // Every thread has ended: the process is gone and has been told of.
            Err(RecvTimeoutError::Disconnected) => false,
        }
    }

    fn fail(&mut self, what: &str) -> ! {
        let log = self.log();
        panic!("{}: {what}; log:\n{log}", self.name);
    }

    /// The stderr text so far.
    pub fn log(&mut self) -> String {
        self.drain();
        self.err.join("\n")
    }

    /// The start line, or the status of a process that exited without printing one.
    pub fn start_line(&mut self) -> Result<String, ExitStatus> {
        let since = Instant::now();
        loop {
            if let Some(line) = self.out.iter().find(|line| line.starts_with("kbf-")) {
                return Ok(line.clone());
            }
            if let Some(status) = self.exit {
                return Err(status);
            }
            if !self.pump(since) {
                self.fail(&format!("no start line in {GUARD:?}"));
            }
        }
    }

    /// Waits for the process to exit.
    pub fn wait_exit(&mut self) -> ExitStatus {
        let since = Instant::now();
        loop {
            self.drain();
            if let Some(status) = self.exit {
                return status;
            }
            if !self.pump(since) {
                self.fail(&format!("still running after {GUARD:?}"));
            }
        }
    }

    fn signal(&mut self, signal: rustix::process::Signal) {
        self.drain();
        if self.exit.is_some() {
            return;
        }
        let pid = i32::try_from(self.pid)
            .ok()
            .and_then(rustix::process::Pid::from_raw)
            .expect("a pid");
        let _ = rustix::process::kill_process(pid, signal);
    }

    /// SIGKILL, and the exit.
    pub fn kill(&mut self) -> ExitStatus {
        self.signal(rustix::process::Signal::KILL);
        self.wait_exit()
    }

    /// SIGTERM, and the exit.
    pub fn term(&mut self) -> ExitStatus {
        self.signal(rustix::process::Signal::TERM);
        self.wait_exit()
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        self.signal(rustix::process::Signal::KILL);
    }
}

/// A scratch directory under the target directory, kept when its test fails.
pub struct Dir(PathBuf);

impl Dir {
    pub fn new(test: &str) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, Ordering::SeqCst);
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("handover-crash-{}-{n}-{test}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        Self(dir)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

/// The three addresses a frontend serves, from its start line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Addrs {
    pub reapi: SocketAddr,
    pub worker: SocketAddr,
    pub api: SocketAddr,
}

impl Addrs {
    fn of(line: &str) -> Self {
        let field = |key: &str| -> SocketAddr {
            line.split_whitespace()
                .find_map(|word| word.strip_prefix(key))
                .unwrap_or_else(|| panic!("no {key} in the start line {line:?}"))
                .parse()
                .expect("an address")
        };
        Self {
            reapi: field("reapi="),
            worker: field("worker="),
            api: field("api="),
        }
    }

    /// Three loopback listeners on free ports, and their addresses.
    pub fn free() -> (Vec<std::net::TcpListener>, Self) {
        let listeners: Vec<_> = (0..3)
            .map(|_| std::net::TcpListener::bind("127.0.0.1:0").expect("a free port"))
            .collect();
        let addr = |n: usize| listeners[n].local_addr().expect("address");
        let addrs = Self {
            reapi: addr(0),
            worker: addr(1),
            api: addr(2),
        };
        (listeners, addrs)
    }
}

/// A storage node and the directory the frontends of one test share.
pub struct Rig {
    pub dir: Dir,
    /// The scratch directory, open: a socket path through it is short enough for
    /// `sockaddr_un` however deep the directory is.
    dirfd: std::fs::File,
    /// Held for the test: killed when dropped.
    _node: Proc,
    storage: String,
}

/// A frontend that printed its start line.
pub struct Frontend {
    pub proc: Proc,
    pub addrs: Addrs,
}

impl Rig {
    /// A storage node over new stores.
    pub fn start(test: &str) -> Self {
        let dir = Dir::new(test);
        let cold = format!("--cold-store=file://{}", dir.path().join("cold").display());
        let meta = format!("--meta-dir={}", dir.path().join("meta").display());
        std::fs::create_dir_all(dir.path().join("cold")).expect("cold dir");
        let args = ["storage", "--storage-listen=127.0.0.1:0", &cold, &meta].map(str::to_owned);
        let mut node = Proc::spawn(&dir, "node", &args);
        let line = node.start_line().expect("the node starts");
        let storage = line
            .split_whitespace()
            .find_map(|word| word.strip_prefix("storage="))
            .expect("storage= in the node's start line")
            .to_owned();
        let dirfd = std::fs::File::open(dir.path()).expect("open the directory");
        Self {
            dir,
            dirfd,
            _node: node,
            storage,
        }
    }

    pub fn state_db(&self) -> PathBuf {
        self.dir.path().join("state").join("state.db")
    }

    /// The control socket as this process names it (through the open directory).
    pub fn control_socket(&self) -> PathBuf {
        use std::os::fd::AsRawFd as _;
        PathBuf::from(format!(
            "/proc/self/fd/{}/{CONTROL}",
            self.dirfd.as_raw_fd()
        ))
    }

    /// The flags of a frontend that hands over, on `addrs` (new ones if none).
    pub fn flags(&self, addrs: Option<Addrs>) -> Vec<String> {
        // A first frontend lets the system pick its ports (a port found free and let go
        // can be taken by another test before the frontend binds it); the frontends
        // that follow it name the ports it got.
        let [reapi, worker, api] = match addrs {
            Some(a) => [a.reapi, a.worker, a.api].map(|a| a.to_string()),
            None => [
                "127.0.0.1:0".to_owned(),
                "127.0.0.1:0".to_owned(),
                "127.0.0.1:0".to_owned(),
            ],
        };
        [
            "frontend".to_owned(),
            format!("--storage=http://{}", self.storage),
            format!("--listen={reapi}"),
            format!("--worker-listen={worker}"),
            format!("--api-listen={api}"),
            format!("--state-store=sqlite://{}", self.state_db().display()),
            format!("--handover-socket={CONTROL}"),
            "--handover-quiesce-secs=1".to_owned(),
            // The old frontend declares an operation lost after three looks in a row at
            // the state file find neither its `op/` row nor its `done/` row (a writer
            // ends an operation in two transactions): 100 ms apart, that is 200 ms for a
            // disk that stalls between the two. At 10 ms it was 20 ms, and a loaded
            // disk answered a client "lost" for a result committed moments later.
            "--handover-follow-interval-ms=100".to_owned(),
            format!("--heartbeat-interval-ms={HEARTBEAT_MS}"),
        ]
        .into()
    }

    /// A frontend started (named `name` for its log), not yet waited for.
    pub fn spawn_frontend(&self, name: &str, addrs: Option<Addrs>) -> Proc {
        Proc::spawn(&self.dir, name, &self.flags(addrs))
    }

    /// A frontend started and serving. With `addrs` it takes the addresses over from the
    /// frontend serving them, if one is.
    pub fn frontend(&self, name: &str, addrs: Option<Addrs>) -> Frontend {
        let mut proc = self.spawn_frontend(name, addrs);
        match proc.start_line() {
            Ok(line) => Frontend {
                addrs: Addrs::of(&line),
                proc,
            },
            Err(status) => panic!(
                "{name} exited {status} before serving; log:\n{}",
                proc.log()
            ),
        }
    }
}

// ---------------------------------------------------------------------------------
// The state file

/// The rows of the state file at one moment, read through a connection of our own (the
/// file is in WAL mode: a read never waits for a writer and never writes).
#[derive(Debug, PartialEq, Eq, Clone)]
pub struct Rows(BTreeMap<String, Vec<u8>>);

impl Rows {
    pub fn read(db: &Path) -> Self {
        let conn =
            rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                .expect("open the state file read-only");
        let mut query = conn.prepare("SELECT key, value FROM kv").expect("prepare");
        let rows = query
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
            })
            .expect("query");
        Self(rows.map(|row| row.expect("a row")).collect())
    }

    /// The keys under `prefix`, in order.
    pub fn keys(&self, prefix: &str) -> Vec<String> {
        self.0
            .keys()
            .filter(|key| key.starts_with(prefix))
            .cloned()
            .collect()
    }

    /// The 8-byte counter at `key`.
    pub fn number(&self, key: &str) -> u64 {
        let value = self.0.get(key).unwrap_or_else(|| panic!("no row {key}"));
        u64::from_be_bytes(<[u8; 8]>::try_from(value.as_slice()).expect("an 8-byte number"))
    }

    pub fn generation(&self) -> u64 {
        self.number(kbf_state::GENERATION)
    }

    pub fn epoch(&self) -> u64 {
        self.number("meta/epoch")
    }
}

// ---------------------------------------------------------------------------------
// The control socket, from either end

/// One end of a control connection, driven by the test: a line per message, the
/// listener sockets attached to the offer.
pub struct Wire(pub UnixStream);

impl Wire {
    pub fn connect(socket: &Path) -> Self {
        Self(UnixStream::connect(socket).expect("connect to the control socket"))
    }

    pub fn say(&mut self, message: &Message) {
        writeln!(self.0, "{}", message.line()).expect("send");
    }

    pub fn say_with(&mut self, message: &Message, sockets: &[std::net::TcpListener]) {
        let line = format!("{}\n", message.line());
        let fds: Vec<_> = sockets.iter().map(AsFd::as_fd).collect();
        let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(3))];
        let mut control = SendAncillaryBuffer::new(&mut space);
        control.push(SendAncillaryMessage::ScmRights(&fds));
        let sent = sendmsg(
            &self.0,
            &[IoSlice::new(line.as_bytes())],
            &mut control,
            SendFlags::empty(),
        )
        .expect("sendmsg");
        assert_eq!(sent, line.len(), "a short send");
    }

    /// The next message and the sockets that came with it.
    pub fn hear(&mut self) -> (Message, Vec<OwnedFd>) {
        let mut space =
            [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(3), ScmCredentials(1))];
        let mut control = RecvAncillaryBuffer::new(&mut space);
        let mut buffer = [0_u8; 512];
        let mut slices = [IoSliceMut::new(&mut buffer)];
        let got =
            recvmsg(&self.0, &mut slices, &mut control, RecvFlags::CMSG_CLOEXEC).expect("recvmsg");
        let sockets = control
            .drain()
            .flat_map(|received| match received {
                RecvAncillaryMessage::ScmRights(sockets) => sockets.collect::<Vec<_>>(),
                _ => Vec::new(),
            })
            .collect();
        let text = String::from_utf8_lossy(&buffer[..got.bytes]).into_owned();
        let message = Message::parse(&text).unwrap_or_else(|| panic!("not a message: {text:?}"));
        (message, sockets)
    }
}

/// The hello a successor started with `addrs` and the state file `state` says.
pub fn hello(addrs: &Addrs, state: &Path) -> Message {
    Message::Hello {
        version: 1,
        reapi: addrs.reapi,
        worker: addrs.worker,
        api: Some(addrs.api),
        state: state.to_owned(),
    }
}

// ---------------------------------------------------------------------------------
// A daemon

/// The server's side of a worker session as a daemon sees it. A message the test is not
/// waiting for is kept for a later look.
pub struct Daemon {
    tx: UnboundedSender<DaemonMessage>,
    inbound: Streaming<ServerMessage>,
    kept: VecDeque<server_message::Message>,
    seq: u64,
    /// The lease epoch the Welcome named.
    pub epoch: u64,
}

impl Daemon {
    /// Opens a session to the worker listener at `addr` as node `node`, waits for the
    /// Welcome and sends a `NodeStatus`.
    pub async fn connect(addr: SocketAddr, node: &str) -> Self {
        let channel = Endpoint::from_shared(format!("http://{addr}"))
            .expect("endpoint")
            .connect()
            .await
            .expect("connect to the worker listener");
        let (tx, rx) = unbounded();
        let capability = |key: &str, value: &str| Capability {
            key: key.to_owned(),
            value: value.to_owned(),
        };
        let hello = Hello {
            protocol_version: 1,
            node_id: node.to_owned(),
            daemon_version: "test".to_owned(),
            capabilities: vec![
                capability("cpus", "4"),
                capability("mem_gib", "8"),
                capability("arch", "x86_64"),
                capability("os", "linux"),
                capability("drivers", "fake"),
            ],
            report_hash: Vec::new(),
            instance_id: format!("{node}-daemon"),
            phase: None,
            predecessor_instance_id: String::new(),
        };
        tx.unbounded_send(DaemonMessage {
            message: Some(daemon_message::Message::Hello(hello)),
        })
        .expect("queue hello");
        let inbound = WorkerClient::new(channel)
            .session(rx)
            .await
            .expect("a session")
            .into_inner();
        let mut daemon = Self {
            tx,
            inbound,
            kept: VecDeque::new(),
            seq: 0,
            epoch: 0,
        };
        let welcome = daemon
            .expect("Welcome", |m| match m {
                server_message::Message::Welcome(w) => Some(w.epoch),
                _ => None,
            })
            .await;
        daemon.epoch = welcome;
        daemon.send(daemon_message::Message::NodeStatus(NodeStatus {
            os_name: "Ubuntu".to_owned(),
            daemon_version: "test".to_owned(),
            ..NodeStatus::default()
        }));
        daemon
    }

    fn send(&self, message: daemon_message::Message) {
        self.tx
            .unbounded_send(DaemonMessage {
                message: Some(message),
            })
            .expect("the stream is open");
    }

    /// The first message `pick` takes, within the hang guard; the others are kept.
    async fn expect<T>(
        &mut self,
        what: &str,
        mut pick: impl FnMut(&server_message::Message) -> Option<T>,
    ) -> T {
        if let Some(at) = self.kept.iter().position(|m| pick(m).is_some()) {
            let message = self.kept.remove(at).expect("present");
            return pick(&message).expect("picked");
        }
        let deadline = tokio::time::Instant::now() + GUARD;
        loop {
            let next = timeout(
                deadline - tokio::time::Instant::now(),
                self.inbound.message(),
            )
            .await
            .unwrap_or_else(|_| panic!("no {what} within {GUARD:?}"))
            .unwrap_or_else(|status| panic!("the stream ended with {status} before {what}"))
            .unwrap_or_else(|| panic!("the stream closed before {what}"));
            let message = next.message.expect("a non-empty server message");
            if let Some(found) = pick(&message) {
                return found;
            }
            self.kept.push_back(message);
        }
    }

    /// The next `Start`: the lease to run, and the action digest it is for.
    pub async fn start(&mut self) -> Start {
        self.expect("Start", |m| match m {
            server_message::Message::Start(s) => Some(s.clone()),
            _ => None,
        })
        .await
    }

    /// The status the server ends the stream with. Panics if it closes without one.
    pub async fn ended(&mut self) -> tonic::Status {
        let ending = async {
            loop {
                match self.inbound.message().await {
                    Ok(Some(_)) => {}
                    Ok(None) => panic!("the stream ended without an error"),
                    Err(status) => return status,
                }
            }
        };
        timeout(GUARD, ending)
            .await
            .expect("the server ended the stream in time")
    }

    /// Sends a heartbeat listing `running` and waits for its acknowledgement.
    pub async fn heartbeat(&mut self, running: &[LeaseId]) {
        self.seq += 1;
        let seq = self.seq;
        self.send(daemon_message::Message::Heartbeat(Heartbeat {
            seq,
            report_hash: Vec::new(),
            running: running.to_vec(),
        }));
        self.expect("HeartbeatAck", |m| match m {
            server_message::Message::HeartbeatAck(a) if a.seq == seq => Some(()),
            _ => None,
        })
        .await;
    }

    /// Reports an OK result of `lease` carrying `result`, and returns the server's ack.
    pub async fn report(&mut self, lease: LeaseId, result: &ActionResult) -> ResultAck {
        self.send(daemon_message::Message::Result(kbf_proto::worker::Result {
            lease_id: Some(lease),
            status: Some(kbf_proto::google::rpc::Status::default()),
            action_result: Some(result.clone()),
            action_digest: None,
            memory_kill: 0,
            lost: 0,
        }));
        self.expect("ResultAck", |m| match m {
            server_message::Message::ResultAck(a) if a.lease_id == Some(lease) => Some(*a),
            _ => None,
        })
        .await
    }
}

// ---------------------------------------------------------------------------------
// A client

/// A blob: its bytes and its digest.
#[derive(Clone)]
pub struct Blob {
    pub data: Vec<u8>,
    pub digest: reapi::Digest,
}

impl Blob {
    pub fn new(data: impl Into<Vec<u8>>) -> Self {
        let data = data.into();
        let digest = reapi::Digest {
            hash: hex::encode(Sha256::digest(&data)),
            size_bytes: i64::try_from(data.len()).expect("size"),
        };
        Self { data, digest }
    }

    pub fn of(message: &impl prost::Message) -> Self {
        Self::new(message.encode_to_vec())
    }
}

/// An action and the blobs it needs.
pub struct Job {
    pub action: Blob,
    blobs: Vec<Blob>,
}

impl Job {
    /// A job whose command line is `argv`; two jobs with other `argv` are other actions.
    pub fn new(argv: &str) -> Self {
        let input = Blob::new(format!("input of {argv}"));
        let root = Blob::of(&Directory {
            files: vec![FileNode {
                name: "in".to_owned(),
                digest: Some(input.digest.clone()),
                ..Default::default()
            }],
            ..Default::default()
        });
        let command = Blob::of(&ReapiCommand {
            arguments: vec![argv.to_owned()],
            ..Default::default()
        });
        let action = Blob::of(&Action {
            command_digest: Some(command.digest.clone()),
            input_root_digest: Some(root.digest.clone()),
            ..Default::default()
        });
        Self {
            blobs: vec![action.clone(), command, root, input],
            action,
        }
    }
}

/// A REAPI client of one channel (one HTTP/2 connection, reconnecting when it is closed).
pub struct Client {
    pub channel: Channel,
}

impl Client {
    pub async fn connect(reapi: SocketAddr) -> Self {
        let channel = Endpoint::from_shared(format!("http://{reapi}"))
            .expect("endpoint")
            .connect()
            .await
            .expect("connect to the REAPI listener");
        Self { channel }
    }

    fn exec(&self) -> ExecutionClient<Channel> {
        ExecutionClient::new(self.channel.clone())
    }

    async fn upload(&self, blobs: &[&Blob]) {
        let request = BatchUpdateBlobsRequest {
            requests: blobs
                .iter()
                .map(|blob| batch_update_blobs_request::Request {
                    digest: Some(blob.digest.clone()),
                    data: blob.data.clone(),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        };
        let (response, _) = retrying(20, || {
            let mut cas = ContentAddressableStorageClient::new(self.channel.clone());
            let request = request.clone();
            async move { cas.batch_update_blobs(request).await }
        })
        .await;
        let response = response.into_inner();
        for r in response.responses {
            assert_eq!(
                r.status.expect("status").code,
                0,
                "upload of {:?}",
                r.digest
            );
        }
    }

    /// Uploads what `job` needs.
    pub async fn upload_job(&self, job: &Job) {
        let blobs: Vec<&Blob> = job.blobs.iter().collect();
        self.upload(&blobs).await;
    }

    /// Uploads an output and returns an ActionResult naming it.
    pub async fn output(&self, text: &str) -> ActionResult {
        let out = Blob::new(text);
        self.upload(&[&out]).await;
        ActionResult {
            output_files: vec![OutputFile {
                path: "out".to_owned(),
                digest: Some(out.digest),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    /// Starts an Execute of `job`'s action.
    pub async fn execute(&self, job: &Job) -> Result<Streaming<Operation>, tonic::Status> {
        self.exec()
            .execute(ExecuteRequest {
                instance_name: "main".to_owned(),
                action_digest: Some(job.action.digest.clone()),
                ..Default::default()
            })
            .await
            .map(tonic::Response::into_inner)
    }

    /// Starts a WaitExecution of the operation `name`.
    pub async fn wait(&self, name: &str) -> Result<Streaming<Operation>, tonic::Status> {
        self.exec()
            .wait_execution(WaitExecutionRequest {
                name: name.to_owned(),
            })
            .await
            .map(tonic::Response::into_inner)
    }
}

/// The first operation of `stream`: its name is what WaitExecution takes.
pub async fn first(stream: &mut Streaming<Operation>) -> Operation {
    timeout(GUARD, stream.message())
        .await
        .expect("an operation in time")
        .expect("a healthy stream")
        .expect("the stream ends only after the done operation")
}

/// Reads `stream` to the done operation and returns it, then checks the stream ends
/// cleanly right after it: one answer, no second one, no error.
pub async fn done_once(stream: &mut Streaming<Operation>) -> ExecuteResponse {
    let finished = loop {
        let op = first(stream).await;
        if op.done {
            break op;
        }
    };
    let end = timeout(GUARD, stream.message())
        .await
        .expect("the stream ends after the done operation");
    assert!(
        matches!(end, Ok(None)),
        "the stream went on after the done operation: {end:?}"
    );
    match finished.result {
        Some(operation::Result::Response(any)) => {
            ExecuteResponse::decode(any.value.as_slice()).expect("an ExecuteResponse")
        }
        other => panic!("a done operation without a response: {other:?}"),
    }
}

/// Whether the response carries `result`'s outputs and exit code (the server adds metadata
/// of its own).
pub fn answered_with(response: &ExecuteResponse, result: &ActionResult) -> bool {
    let got = response.result.as_ref();
    got.is_some_and(|got| {
        got.output_files == result.output_files
            && got.exit_code == result.exit_code
            && response.status.as_ref().is_none_or(|s| s.code == 0)
    })
}

/// The code of an error from a call, as a word for a failure message.
pub fn describe(status: &tonic::Status) -> String {
    let mut text = String::new();
    let _ = write!(text, "{:?}: {}", status.code(), status.message());
    text
}

/// Makes `call` until it succeeds, up to `tries` times, asking again only after an error
/// a client asks again after: UNAVAILABLE (the code the frontend refuses a call that
/// raced a handover with), and the CANCELLED a tonic channel reports for the one call it
/// makes on a connection the other end has closed after GOAWAY (it dials again for the
/// next). Any other code fails the test. Returns the answer and how many tries failed.
pub async fn retrying<T, F, Fut>(tries: usize, mut call: F) -> (T, usize)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, tonic::Status>>,
{
    let mut refused = Vec::new();
    for attempt in 0..tries {
        match call().await {
            Ok(answer) => return (answer, attempt),
            Err(status) if matches!(status.code(), Code::Unavailable | Code::Cancelled) => {
                refused.push(describe(&status));
            }
            Err(status) => panic!(
                "a call failed with an error a client does not ask again after: {}",
                describe(&status)
            ),
        }
    }
    panic!("{tries} tries all failed: {refused:?}");
}

/// Executes `job` through `client` (see [`retrying`]).
pub async fn execute_with_retry(
    client: &Client,
    job: &Job,
    tries: usize,
) -> (Streaming<Operation>, usize) {
    retrying(tries, || client.execute(job)).await
}
