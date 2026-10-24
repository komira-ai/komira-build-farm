//! The binary's wiring: flags, the ports, the listener and the thread that drives rolls.
//!
//! Every setting is a flag (configuration is flags, not environment). Required:
//!
//! - `--state-dir`: where `roll.json` lives (`RollStore` takes an exclusive lock on it);
//! - `--inventory`: the root-owned host file;
//! - `--proxy-uid`: the one uid whose connections are served;
//! - `--operator-api`: `http://<loopback ip>:<port>`, the frontend's operator API, for
//!   safe-to-stop and the health of storage and daemon hosts; a host that is not a loopback
//!   address is refused;
//! - `--token-file`: the file holding the operator bearer token.
//!
//! And the socket, one of two (never both):
//!
//! - **production**: socket activation. systemd binds the socket (`SocketUser=<proxy user>`,
//!   `SocketMode=0600`) and passes it as fd 3 with `LISTEN_PID` and `LISTEN_FDS=1`
//!   ([`crate::listener::Activation`]); the updater serves it as it is;
//! - **tests and development**: `--socket`, a path the updater binds itself, mode 0600, its
//!   own user.
//!
//! The installer is [`SshInstaller`], for storage and daemon hosts. Its settings are flags:
//!
//! - `--ssh-key-file`: the private key file ssh logs in with (a path; the key is never on
//!   the command line);
//! - `--ssh-user`: the user the host's forced command is installed for;
//! - `--channel`: the package channel an install comes from;
//! - optional `--ssh-connect-timeout-secs` (default 10) and `--ssh-command-timeout-secs`
//!   (default 900, the time one install may take);
//! - optional `--drain-deadline-secs` (default 600, at least 1);
//! - optional `--apply-poll-secs` (default 5, at least 1) and `--apply-deadline-secs` (default
//!   600, at least 1), for `mac-daemon` hosts.
//!
//! A daemon host's node is drained at the frontend before its install and given back after
//! it runs the target ([`crate::node_drain`]); `--drain-deadline-secs` (default 600) is how long
//! its leases are waited for. The health check is real for storage and daemon hosts and refuses a
//! frontend ([`crate::node_health`]); the roll asks it before it installs, so ssh is never run
//! for one.
//!
//! A `mac-daemon` host is rolled as a daemon host, except that its install only starts an
//! apply on the Mac: the roll asks the host's `query` every `--apply-poll-secs` until the
//! apply is `done` for the target, for at most `--apply-deadline-secs`, and then holds for the
//! daemon's health as for any daemon host. A failed apply, or a deadline that runs out, ends
//! the roll with the node left drained.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::thread::JoinHandle;
use std::time::Duration;

use serde_json::json;

use crate::api::RollState;
use crate::inventory::Inventory;
use crate::listener::{Activation, Listen, open};
use crate::node_drain::NodeDrain;
use crate::node_health::NodeHealth;
use crate::operator::OperatorApi;
use crate::ports::SystemClock;
use crate::record::RollStore;
use crate::safe_http::HttpSafeToStop;
use crate::server::{AuditSink, Recent, Server, Settings};
use crate::ssh_installer::{Exec, SshInstaller, SystemExec};
use crate::{Ports, Timing, Updater};

pub const USAGE: &str = "usage: kbf-updater --version\n       kbf-updater --state-dir DIR \
--inventory FILE --proxy-uid UID --operator-api http://IP:PORT --token-file FILE \
--ssh-key-file FILE --ssh-user USER --channel NAME \
[--ssh-connect-timeout-secs N] [--ssh-command-timeout-secs N] \
[--drain-deadline-secs N] [--apply-poll-secs N] [--apply-deadline-secs N] [--socket PATH]\n\
the socket comes from systemd socket activation (LISTEN_PID, LISTEN_FDS=1) or from --socket, \
which is for tests and development\n";

const FLAGS: [&str; 14] = [
    "--state-dir",
    "--inventory",
    "--socket",
    "--proxy-uid",
    "--operator-api",
    "--token-file",
    "--ssh-key-file",
    "--ssh-user",
    "--channel",
    "--ssh-connect-timeout-secs",
    "--ssh-command-timeout-secs",
    "--drain-deadline-secs",
    "--apply-poll-secs",
    "--apply-deadline-secs",
];

/// How long ssh may take to connect, unless a flag says otherwise.
const DEFAULT_CONNECT_SECS: u64 = 10;
/// How long one call may take, unless a flag says otherwise: as long as a restart waits for
/// an install that is in progress.
const DEFAULT_COMMAND_SECS: u64 = 900;
/// How long a daemon host's node is given to finish its leases, unless a flag says otherwise.
const DEFAULT_DRAIN_SECS: u64 = 600;
/// How often a Mac host is asked how its apply stands, unless a flag says otherwise.
const DEFAULT_APPLY_POLL_SECS: u64 = 5;
/// How long a Mac host's apply may take, unless a flag says otherwise.
const DEFAULT_APPLY_DEADLINE_SECS: u64 = 600;

/// The settings of a run.
#[derive(Clone, Debug)]
pub struct Config {
    pub state_dir: PathBuf,
    pub inventory: PathBuf,
    /// The uid that must own the inventory: root, for the binary. A test says otherwise.
    pub inventory_owner: u32,
    pub listen: Listen,
    pub proxy_uid: u32,
    pub operator_api: SocketAddr,
    pub token_file: PathBuf,
    /// The private key file ssh logs in with.
    pub ssh_key_file: PathBuf,
    pub ssh_user: String,
    /// The package channel an install comes from.
    pub channel: String,
    pub ssh_connect_timeout: Duration,
    pub ssh_command_timeout: Duration,
    /// How long a daemon host's node is given to finish its leases once drained; its host is
    /// installed when that is out, leases or not.
    pub drain_deadline: Duration,
    /// How often a `mac-daemon` host is asked how its apply stands.
    pub apply_poll: Duration,
    /// How long a `mac-daemon` host's apply may take to be done for the target.
    pub apply_deadline: Duration,
}

/// The settings from the command line (without the program name) and from what the service
/// manager passed. Each flag is given once, as `--flag value` or `--flag=value`; all but
/// `--socket`, the two ssh timeouts, the drain deadline and the two apply bounds are required,
/// and the socket is either `--socket` or the activated descriptor.
pub fn parse_flags(args: &[String], activation: &Activation) -> Result<Config, String> {
    let mut found: BTreeMap<&str, String> = BTreeMap::new();
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        let (name, inline) = match arg.split_once('=') {
            Some((name, value)) => (name, Some(value.to_owned())),
            None => (arg.as_str(), None),
        };
        let Some(&flag) = FLAGS.iter().find(|f| **f == name) else {
            return Err(format!("unknown argument {name}"));
        };
        let value = match inline {
            Some(value) => value,
            None => rest
                .next()
                .cloned()
                .ok_or_else(|| format!("{flag} needs a value"))?,
        };
        if found.insert(flag, value).is_some() {
            return Err(format!("{flag} is given twice"));
        }
    }
    let mut take = |flag: &str| {
        found
            .remove(flag)
            .ok_or_else(|| format!("{flag} is required"))
    };
    let state_dir = PathBuf::from(take("--state-dir")?);
    let inventory = PathBuf::from(take("--inventory")?);
    let proxy_uid = take("--proxy-uid")?
        .parse::<u32>()
        .map_err(|_| "--proxy-uid must be a number".to_owned())?;
    let operator_api = parse_operator_url(&take("--operator-api")?)?;
    let token_file = PathBuf::from(take("--token-file")?);
    let ssh_key_file = PathBuf::from(take("--ssh-key-file")?);
    let ssh_user = take("--ssh-user")?;
    let channel = take("--channel")?;
    let mut seconds = |flag: &str, default: u64| match found.remove(flag) {
        None => Ok(Duration::from_secs(default)),
        Some(text) => text
            .parse::<u64>()
            .map(Duration::from_secs)
            .map_err(|_| format!("{flag} must be a number of seconds")),
    };
    let ssh_connect_timeout = seconds("--ssh-connect-timeout-secs", DEFAULT_CONNECT_SECS)?;
    let ssh_command_timeout = seconds("--ssh-command-timeout-secs", DEFAULT_COMMAND_SECS)?;
    let drain_deadline = seconds("--drain-deadline-secs", DEFAULT_DRAIN_SECS)?;
    if drain_deadline.is_zero() {
        return Err("--drain-deadline-secs must be at least 1".to_owned());
    }
    let apply_poll = seconds("--apply-poll-secs", DEFAULT_APPLY_POLL_SECS)?;
    if apply_poll.is_zero() {
        return Err("--apply-poll-secs must be at least 1".to_owned());
    }
    let apply_deadline = seconds("--apply-deadline-secs", DEFAULT_APPLY_DEADLINE_SECS)?;
    if apply_deadline.is_zero() {
        return Err("--apply-deadline-secs must be at least 1".to_owned());
    }
    let listen = match (
        found.remove("--socket"),
        activation.passed_to(std::process::id())?,
    ) {
        (Some(path), None) => Listen::Bind(PathBuf::from(path)),
        (None, Some(fd)) => Listen::Activated(fd),
        (Some(_), Some(_)) => {
            return Err(
                "--socket is given and systemd passed a socket (LISTEN_FDS): use one".to_owned(),
            );
        }
        (None, None) => {
            return Err(
                "--socket is required unless systemd passes the socket (LISTEN_PID, LISTEN_FDS)"
                    .to_owned(),
            );
        }
    };
    Ok(Config {
        state_dir,
        inventory,
        inventory_owner: 0,
        listen,
        proxy_uid,
        operator_api,
        token_file,
        ssh_key_file,
        ssh_user,
        channel,
        ssh_connect_timeout,
        ssh_command_timeout,
        drain_deadline,
        apply_poll,
        apply_deadline,
    })
}

/// `http://<ip>:<port>` with a loopback ip. The operator token goes over this connection in
/// clear, so any other host, a name, or `https` is refused.
pub fn parse_operator_url(url: &str) -> Result<SocketAddr, String> {
    let flag = "--operator-api";
    let Some(rest) = url.strip_prefix("http://") else {
        return Err(format!(
            "{flag} must start with http:// (the call travels in clear, on loopback)"
        ));
    };
    let addr: SocketAddr = rest
        .trim_end_matches('/')
        .parse()
        .map_err(|_| format!("{flag} must be http://<ip>:<port>, with an IP address"))?;
    if !addr.ip().to_canonical().is_loopback() {
        return Err(format!("{flag} must be a loopback address"));
    }
    Ok(addr)
}

/// Stops a running [`Daemon`] from another thread.
#[derive(Clone)]
pub struct Stopper {
    flag: Arc<AtomicBool>,
    socket: PathBuf,
    /// Whether the updater made the socket file, and so removes it.
    owned: bool,
}

impl Stopper {
    /// Ask the accept loop to end: set the flag and make one connection to wake it. (An
    /// activated socket that only the proxy's user may connect to cannot be woken by the
    /// updater's user; the process then ends by signal, as a service does.)
    pub fn stop(&self) {
        self.flag.store(true, Ordering::SeqCst);
        let _ = UnixStream::connect(&self.socket);
    }
}

/// A running updater: the listener thread and the thread that drives rolls.
pub struct Daemon {
    stopper: Stopper,
    accept: JoinHandle<()>,
    worker: JoinHandle<()>,
}

impl Daemon {
    pub fn stopper(&self) -> Stopper {
        self.stopper.clone()
    }

    /// Block until the listener has been stopped and the roll in hand (if any) is over;
    /// remove the socket if the updater made it.
    pub fn wait(self) {
        let _ = self.accept.join();
        let _ = self.worker.join();
        if self.stopper.owned {
            let _ = std::fs::remove_file(&self.stopper.socket);
        }
    }
}

/// An audit sink that writes each line to stderr, where the journal takes it.
pub fn stderr_sink() -> AuditSink {
    Arc::new(|line| eprintln!("{line}"))
}

/// The ports of this build: the real safe-to-stop, health check, clock and ssh installer.
pub fn build_ports(cfg: &Config) -> Result<Ports, String> {
    build_ports_with_exec(cfg, Arc::new(SystemExec))
}

/// [`build_ports`], running the installer's processes through `exec`. A test passes a
/// recorder in place of the system, so that no `ssh` runs.
pub fn build_ports_with_exec(cfg: &Config, exec: Arc<dyn Exec>) -> Result<Ports, String> {
    // The key is read by ssh, not here; a path that is no file is told at start-up.
    let key_ok = std::fs::metadata(&cfg.ssh_key_file).is_ok_and(|m| m.is_file());
    if !key_ok {
        return Err(format!(
            "--ssh-key-file {} is not a file",
            cfg.ssh_key_file.display()
        ));
    }
    let installer = SshInstaller::new(
        cfg.ssh_key_file.clone(),
        &cfg.ssh_user,
        &cfg.channel,
        cfg.ssh_connect_timeout,
        cfg.ssh_command_timeout,
    )
    .map_err(|e| e.to_string())?
    .with_exec(exec);
    // Shared: the roll installs with it, and the health check asks it what a daemon runs.
    let installer = Arc::new(installer);
    // One client, checked once: both ports read the same token files.
    let api = OperatorApi::new(cfg.operator_api, cfg.token_file.clone())?;
    Ok(Ports {
        safe: Box::new(HttpSafeToStop::from_api(api.clone())),
        installer: Box::new(Arc::clone(&installer)),
        drain: Box::new(NodeDrain::with_api(api.clone())),
        health: Box::new(NodeHealth::with_api(api, installer)),
        clock: Box::new(SystemClock::new()),
    })
}

/// The bounds of the roll: the defaults, and the three the flags set.
pub fn timing(cfg: &Config) -> Timing {
    Timing {
        drain_deadline: cfg.drain_deadline,
        apply_poll: cfg.apply_poll,
        apply_deadline: cfg.apply_deadline,
        ..Timing::default()
    }
}

/// Start with the ports of this build.
pub fn start(cfg: &Config) -> Result<Daemon, String> {
    start_with(
        cfg,
        build_ports(cfg)?,
        timing(cfg),
        Settings::new(cfg.proxy_uid),
        stderr_sink(),
    )
}

/// [`start`], then `ready` with a handle that stops it, then block until it is stopped. The
/// exit code and text of the process.
pub fn run_config(cfg: &Config, ready: impl FnOnce(Stopper)) -> (u8, String) {
    match start(cfg) {
        Err(why) => (1, format!("kbf-updater: {why}\n")),
        Ok(running) => {
            ready(running.stopper());
            running.wait();
            (0, String::new())
        }
    }
}

/// Open the store and the inventory, bind the socket, start the threads. A roll found
/// `running` is resumed.
pub fn start_with(
    cfg: &Config,
    ports: Ports,
    timing: Timing,
    settings: Settings,
    audit: AuditSink,
) -> Result<Daemon, String> {
    let inventory = Inventory::load(&cfg.inventory, cfg.inventory_owner)
        .map_err(|e| format!("inventory {}: {e}", cfg.inventory.display()))?;
    let store = RollStore::open(&cfg.state_dir)
        .map_err(|e| format!("state dir {}: {e}", cfg.state_dir.display()))?;
    let updater = Updater::open(inventory, store, ports, timing)
        .map_err(|e| format!("roll record in {}: {e}", cfg.state_dir.display()))?;
    let updater = Arc::new(updater);
    let opened = open(&cfg.listen)?;
    let (drive, queue) = channel();
    if let Ok(Some(roll)) = updater.current()
        && roll.state == RollState::Running
    {
        let _ = drive.send(roll.id);
    }
    let recent = Arc::new(Recent::default());
    let worker = {
        let (updater, recent, audit) = (Arc::clone(&updater), Arc::clone(&recent), audit.clone());
        std::thread::spawn(move || drive_loop(&updater, queue, &recent, &audit))
    };
    let server = Arc::new(Server::new(updater, settings, audit, drive, recent));
    let stopper = Stopper {
        flag: Arc::new(AtomicBool::new(false)),
        socket: opened.path,
        owned: opened.owned,
    };
    let flag = Arc::clone(&stopper.flag);
    let accept = std::thread::spawn(move || {
        let listener: UnixListener = opened.listener;
        server.serve(&listener, &flag);
    });
    Ok(Daemon {
        stopper,
        accept,
        worker,
    })
}

/// Run each queued roll to its end, one after the other (the updater allows one at a time).
pub(crate) fn drive_loop(
    updater: &Updater,
    queue: Receiver<String>,
    recent: &Recent,
    audit: &AuditSink,
) {
    for id in queue {
        let line = match updater.drive(&id) {
            Ok(roll) => {
                recent.push(&roll);
                json!({"event": "roll_finished", "roll_id": roll.id, "state": roll.state,
                       "failure": roll.failure})
            }
            Err(e) => json!({"event": "roll_not_driven", "roll_id": id, "error": e.to_string()}),
        };
        audit(&line.to_string());
    }
}
