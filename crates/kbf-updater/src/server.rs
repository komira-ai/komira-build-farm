//! The updater's socket and process on Linux (S4.1, S4.3).
//!
//! **Start-up.** [`startup_checks`] refuses to start on a kernel before 6.5 (no
//! `SO_PEERPIDFD`), on a node whose platform is not the pinned one, and while any process
//! running the installed `kbf-daemon` uses `--driver native`.
//!
//! **Socket.** [`bind`] makes the socket's directory mode 0750 and the socket mode 0660,
//! both in the helpers' dedicated group, never a group every user is in. The directory
//! must be a real directory owned by the updater's uid (root); a symbolic link or
//! another user's directory is refused. The directories above it must be root's too
//! (it is meant to live under `/run`); that is provisioning's job.
//!
//! **Protocol.** One request per connection. The caller is checked first
//! ([`check_caller`]); a refused caller's connection is closed without a reply. The
//! caller then writes one JSON request and shuts down its write side; the updater
//! replies with one JSON line and closes. A request is at most [`MAX_REQUEST`] bytes.
//!
//! ```text
//! {"verb":"status"}
//! {"verb":"stage","set":<envelope>,"statement":<envelope>}   statement optional
//! {"verb":"apply","set":<envelope>,"statement":<envelope>}   statement optional
//! -> {"ok":<status or outcome>} | {"error":"<why>"}
//! ```
//!
//! Requests are served one at a time, so verbs never interleave on the state file.

use std::convert::Infallible;
use std::fs;
use std::io::{self, Read as _, Write as _};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::Deserialize;
use serde_json::{Value, json};

use crate::Refusal;
use crate::apply::Applier;
use crate::caller::{
    DaemonPin, check_caller, find_native_daemon, hash_path, kernel_supports_peer_pidfd,
};
use crate::set::Platform;
use crate::signed::{Envelope, PublicKey, parse_key};
use crate::updater::{Config, Updater};

/// The largest request read, in bytes.
pub const MAX_REQUEST: u64 = 1 << 20;

/// How long a caller may take to send its request.
const READ_TIMEOUT: Duration = Duration::from_secs(10);

/// A request: one of the three verbs.
#[derive(Debug, Deserialize)]
#[serde(tag = "verb", rename_all = "lowercase", deny_unknown_fields)]
pub enum Request {
    /// What is installed, staged and in progress. A struct variant, so an unknown
    /// field is refused as it is for the other verbs.
    Status {},
    /// Verify and stage a set.
    Stage {
        /// The signed set.
        set: Envelope,
        /// A key statement to consider beside the stored one.
        #[serde(default)]
        statement: Option<Envelope>,
    },
    /// Verify and install the staged set.
    Apply {
        /// The signed set.
        set: Envelope,
        /// A key statement to consider beside the stored one.
        #[serde(default)]
        statement: Option<Envelope>,
    },
}

/// Everything the binary is started with.
#[derive(Clone, Debug)]
pub struct Settings {
    /// The socket's path; its directory is made mode 0750.
    pub socket: PathBuf,
    /// The helpers' dedicated group.
    pub socket_gid: u32,
    /// The installed daemon callers must be.
    pub daemon: DaemonPin,
    /// The updater's configuration.
    pub config: Config,
    /// Where processes are listed at start-up (`/proc`).
    pub proc_root: PathBuf,
}

/// Rust's name for a CPU architecture in the capability keys' spelling (`arm64`, not
/// `aarch64`).
#[must_use]
pub fn capability_arch(rust_arch: &str) -> &str {
    if rust_arch == "aarch64" {
        "arm64"
    } else {
        rust_arch
    }
}

/// This machine's platform in the capability keys' spelling.
#[must_use]
pub fn host_platform() -> Platform {
    Platform {
        os: std::env::consts::OS.to_owned(),
        arch: capability_arch(std::env::consts::ARCH).to_owned(),
    }
}

/// Reads the root public key: a file holding its hex, surrounding whitespace ignored.
///
/// # Errors
/// The file cannot be read or does not hold a key.
pub fn read_root_key(path: &Path) -> Result<PublicKey, String> {
    let text = fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    parse_key(text.trim()).map_err(|e| format!("{}: {e}", path.display()))
}

/// The start-up refusals: a kernel release before 6.5, a pin for another platform than
/// `host`, or a process under `proc_root` running the daemon at `daemon` with the native
/// driver.
///
/// # Errors
/// Which refusal, in words.
pub fn startup_checks(
    release: &str,
    pinned: &Platform,
    host: &Platform,
    proc_root: &Path,
    daemon: &Path,
) -> Result<(), String> {
    if !kernel_supports_peer_pidfd(release) {
        return Err(format!(
            "Linux {release} has no SO_PEERPIDFD; kbf-updater needs Linux 6.5 or later"
        ));
    }
    if pinned != host {
        return Err(format!(
            "pinned platform {}/{} is not this machine's {}/{}",
            pinned.os, pinned.arch, host.os, host.arch
        ));
    }
    let hash = hash_path(daemon).map_err(|e| format!("{}: {e}", daemon.display()))?;
    match find_native_daemon(proc_root, &hash) {
        Some(pid) => Err(format!(
            "kbf-daemon (pid {pid}) runs --driver native, whose actions share its uid; \
             kbf-updater does not run beside it"
        )),
        None => Ok(()),
    }
}

/// Binds the socket: directory mode 0750 and socket mode 0660, both in group `gid`. A
/// stale socket from an earlier run is replaced. The directory must be a real directory
/// (not a symbolic link) owned by the updater's own uid, which is root in production
/// (S4.3); its group and mode are set through a descriptor, never through a link.
///
/// # Errors
/// The directory or socket cannot be made, the directory is a symbolic link or another
/// uid's (`PermissionDenied`), or a group or mode cannot be set.
pub fn bind(socket: &Path, gid: u32) -> io::Result<UnixListener> {
    bind_owned_by(socket, gid, rustix::process::geteuid().as_raw())
}

/// [`bind`], with the directory's required owner given.
fn bind_owned_by(socket: &Path, gid: u32, owner: u32) -> io::Result<UnixListener> {
    use rustix::fs::{Mode, OFlags};
    let dir = socket.parent().ok_or(io::ErrorKind::InvalidInput)?;
    let gid = Some(rustix::fs::Gid::from_raw(gid));
    fs::create_dir_all(dir)?;
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let dir_fd = rustix::fs::open(dir, flags, Mode::empty())?;
    let uid = rustix::fs::fstat(&dir_fd)?.st_uid;
    if uid != owner {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} is owned by uid {uid}, not {owner}", dir.display()),
        ));
    }
    rustix::fs::fchown(&dir_fd, None, gid)?;
    rustix::fs::fchmod(&dir_fd, Mode::from_raw_mode(0o750))?;
    match fs::remove_file(socket) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    let listener = UnixListener::bind(socket)?;
    rustix::fs::chown(socket, None, gid)?;
    fs::set_permissions(socket, fs::Permissions::from_mode(0o660))?;
    Ok(listener)
}

/// The reply to one request body.
pub fn respond<A: Applier>(body: &[u8], updater: &mut Updater<A>, now: u64) -> Value {
    if body.len() as u64 > MAX_REQUEST {
        return json!({ "error": format!("request over {MAX_REQUEST} bytes") });
    }
    let result = match serde_json::from_slice::<Request>(body) {
        Err(e) => Err(Refusal::Malformed(format!("request: {e}"))),
        Ok(Request::Status {}) => Ok(json!(updater.status())),
        Ok(Request::Stage { set, statement }) => updater
            .stage(&set, statement.as_ref(), now)
            .map(|o| json!(o)),
        Ok(Request::Apply { set, statement }) => updater
            .apply(&set, statement.as_ref(), now)
            .map(|o| json!(o)),
    };
    match result {
        Ok(v) => json!({ "ok": v }),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

/// Serves one connection: the caller check, then one request and its reply.
///
/// # Errors
/// Reading the request or writing the reply failed.
pub fn handle<A: Applier>(
    mut stream: UnixStream,
    updater: &mut Updater<A>,
    daemon: &DaemonPin,
    now: u64,
) -> io::Result<()> {
    if let Err(e) = check_caller(&stream, daemon) {
        eprintln!("kbf-updater: refused a caller: {e}");
        return Ok(());
    }
    stream.set_read_timeout(Some(READ_TIMEOUT))?;
    let mut body = Vec::new();
    (&stream).take(MAX_REQUEST + 1).read_to_end(&mut body)?;
    let reply = respond(&body, updater, now);
    stream.write_all(format!("{reply}\n").as_bytes())
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Starts the updater: the start-up checks, the state file, the socket, then serves
/// until killed.
///
/// # Errors
/// Any start-up step fails; serving itself only logs.
pub fn run<A: Applier>(settings: Settings, applier: A) -> Result<Infallible, String> {
    let release = rustix::system::uname()
        .release()
        .to_string_lossy()
        .into_owned();
    startup_checks(
        &release,
        &settings.config.pin.platform,
        &host_platform(),
        &settings.proc_root,
        &settings.daemon.path,
    )?;
    let mut updater = Updater::open(settings.config, applier).map_err(|e| e.to_string())?;
    let listener = bind(&settings.socket, settings.socket_gid)
        .map_err(|e| format!("{}: {e}", settings.socket.display()))?;
    loop {
        let served = listener
            .accept()
            .and_then(|(stream, _)| handle(stream, &mut updater, &settings.daemon, unix_now()));
        if let Err(e) = served {
            eprintln!("kbf-updater: {e}");
        }
    }
}

#[cfg(test)]
#[path = "server_tests.rs"]
mod tests;
