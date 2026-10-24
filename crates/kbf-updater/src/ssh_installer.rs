//! The real [`Installer`]: `ssh` to the forced command of `kbf-host-install`.
//!
//! One call is one `ssh` process with an argument vector built here and no shell on this
//! side (the other side's sshd runs the forced command, which matches the request as a whole
//! string):
//!
//! ```text
//! ssh -F /dev/null -i KEY -o BatchMode=yes -o ControlMaster=no -o ControlPath=none
//!     -o IdentitiesOnly=yes -o StrictHostKeyChecking=yes -o UserKnownHostsFile=FILE
//!     -o GlobalKnownHostsFile=/dev/null -o ConnectTimeout=N -o ServerAliveInterval=15
//!     -o ServerAliveCountMax=3 [-p PORT] -T -- USER@ADDRESS 'COMMAND'
//! ```
//!
//! - `FILE` is a 0600 file in a fresh 0700 directory holding the one line
//!   `ADDRESS KEY` from the inventory, so the only key that is trusted is the pinned one; both
//!   are removed when the call ends.
//! - `COMMAND` is exactly `install kbf=<version> from <channel>` or `query`. The version and
//!   the channel are checked here against the grammar the host applies, and a request the host
//!   would refuse never starts a process.
//! - The environment is `PATH`, `HOME` (ssh wants it) and `LANG=C`; nothing else is passed,
//!   so no agent socket, no `SSH_ASKPASS`, no proxy variable.
//!
//! The answer is read by [`answer`]: one capped JSON line out of a closed set of shapes whose
//! exit status must agree. An install answers `installed`, `already_installed` or, from a
//! Mac, `apply_started` for the version asked ([`InstallAnswer`]); a Mac's `query` carries
//! one more field, `apply` (`running`, `done` or `failed:<reason>`, or null). An [`PortError`] holds one of a few fixed texts or a reason code,
//! never ssh's or apt's own words; stderr is only classified (exit 255, ssh's own failure,
//! is the one kind the roll tells apart: [`PortError::is_unreachable`]) and then dropped.
//!
//! `command_timeout` bounds the whole call. On expiry ssh is killed and the call is the
//! unreachable kind, `PortError::unreachable("timeout")`, for install and query alike. A
//! timeout does not stop an install: the wrapper runs it in a systemd unit, so a later `query`
//! shows `InProgress`. The roll treats an unreachable install as "ask the host" (it re-queries
//! through its recovery path); any other kind would fail the roll while the install is still
//! going, so a timeout must never be reported as one.

mod answer;
mod exec;

use std::ffi::OsString;
use std::fs::{self, DirBuilder, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub use exec::{Exec, ExecError, Exited, Spawn, SystemExec};

use crate::inventory::{Host, valid_address, valid_host_key};
use crate::ports::{InstallAnswer, Installer, PortError, Report};
use crate::version::Version;

/// Where ssh is looked for when the caller names no other.
pub const DEFAULT_SSH: &str = "/usr/bin/ssh";

/// The longest channel name, in bytes (the host's bound).
pub const MAX_CHANNEL_LEN: usize = 32;

/// The longest version, in bytes, once `+` and `~` count twice: the host builds a unit name
/// from it and that has a bound of its own.
const MAX_UNIT_VERSION_LEN: usize = 230;

/// A channel name as the host reads one: `[a-z0-9][a-z0-9-]{0,31}`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Channel(String);

impl Channel {
    pub fn parse(text: &str) -> Result<Channel, ConfigError> {
        let lower_or_digit = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit();
        let mut bytes = text.bytes();
        let well_formed = bytes.next().is_some_and(lower_or_digit)
            && text.len() <= MAX_CHANNEL_LEN
            && bytes.all(|b| lower_or_digit(b) || b == b'-');
        if well_formed {
            Ok(Channel(text.to_owned()))
        } else {
            Err(ConfigError::Channel)
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Why an installer could not be made.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    #[error("the channel must be 1..=32 of a-z, 0-9 and -, starting with a letter or digit")]
    Channel,
    #[error("the ssh user must be 1..=32 of a-z, 0-9, _ and -, not starting with - or a digit")]
    User,
    #[error("a timeout must be more than zero")]
    Timeout,
}

/// Whether `version` is one the host accepts: the grammar of its request parser, restated
/// here on the text (not on [`Version`]'s own check) so that a loosening of either side is
/// caught by the contract test. The host's extras are the room in a unit name and a revision
/// that is not empty.
fn host_accepts(version: &str) -> bool {
    let unit_len = version.len() + version.bytes().filter(|b| b"+~".contains(b)).count();
    version.len() <= crate::version::MAX_LEN
        && unit_len <= MAX_UNIT_VERSION_LEN
        && version.bytes().next().is_some_and(|b| b.is_ascii_digit())
        && version
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".+~-".contains(&b))
        && !version.ends_with('-')
}

fn user_ok(user: &str) -> bool {
    let mut bytes = user.bytes();
    bytes
        .next()
        .is_some_and(|b| b.is_ascii_lowercase() || b == b'_')
        && user.len() <= 32
        && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'-'))
}

/// The installer over ssh. Made with [`SshInstaller::new`]; the channel is its
/// configuration, not an argument of a call.
pub struct SshInstaller {
    ssh: PathBuf,
    key: PathBuf,
    user: String,
    channel: Channel,
    connect_timeout: Duration,
    command_timeout: Duration,
    work_dir: PathBuf,
    exec: Arc<dyn Exec>,
}

impl SshInstaller {
    /// An installer that logs in as `user` with the private key at `key`, installs from
    /// `channel`, gives up connecting after `connect_timeout` and gives up on a whole call
    /// after `command_timeout`.
    ///
    /// # Errors
    /// [`ConfigError`] for a user or channel the host would not take, or a zero timeout.
    pub fn new(
        key: PathBuf,
        user: &str,
        channel: &str,
        connect_timeout: Duration,
        command_timeout: Duration,
    ) -> Result<SshInstaller, ConfigError> {
        if !user_ok(user) {
            return Err(ConfigError::User);
        }
        if connect_timeout.is_zero() || command_timeout.is_zero() {
            return Err(ConfigError::Timeout);
        }
        Ok(SshInstaller {
            ssh: PathBuf::from(DEFAULT_SSH),
            key,
            user: user.to_owned(),
            channel: Channel::parse(channel)?,
            connect_timeout,
            command_timeout,
            work_dir: std::env::temp_dir(),
            exec: Arc::new(SystemExec),
        })
    }

    /// Run this `ssh` instead of [`DEFAULT_SSH`].
    #[must_use]
    pub fn with_ssh(mut self, ssh: PathBuf) -> SshInstaller {
        self.ssh = ssh;
        self
    }

    /// Make the private directory of the `known_hosts` file under `dir` (default: the
    /// system's temporary directory).
    #[must_use]
    pub fn with_work_dir(mut self, dir: PathBuf) -> SshInstaller {
        self.work_dir = dir;
        self
    }

    /// Run processes through `exec`.
    #[must_use]
    pub fn with_exec(mut self, exec: Arc<dyn Exec>) -> SshInstaller {
        self.exec = exec;
        self
    }

    /// The text of the remote command for an install.
    fn install_command(&self, version: &Version) -> String {
        format!("install kbf={version} from {}", self.channel.as_str())
    }

    /// The argument vector of one call.
    fn argv(&self, host: &Host, known_hosts: &Path, command: &str) -> Vec<OsString> {
        let (name, port) = split_address(&host.address);
        let mut args: Vec<OsString> = vec!["-F".into(), "/dev/null".into(), "-i".into()];
        args.push(self.key.clone().into());
        let mut option = |text: String| {
            args.push("-o".into());
            args.push(text.into());
        };
        option("BatchMode=yes".into());
        option("ControlMaster=no".into());
        option("ControlPath=none".into());
        option("IdentitiesOnly=yes".into());
        option("StrictHostKeyChecking=yes".into());
        option(format!("UserKnownHostsFile={}", known_hosts.display()));
        option("GlobalKnownHostsFile=/dev/null".into());
        // Whole seconds, rounded up: a timeout of 1.2 s must not become 1.
        let seconds =
            self.connect_timeout.as_secs() + u64::from(self.connect_timeout.subsec_nanos() > 0);
        option(format!("ConnectTimeout={seconds}"));
        option("ServerAliveInterval=15".into());
        option("ServerAliveCountMax=3".into());
        if let Some(port) = port {
            args.push("-p".into());
            args.push(port.into());
        }
        args.push("-T".into());
        args.push("--".into());
        args.push(format!("{}@{name}", self.user).into());
        args.push(command.into());
        args
    }

    /// One call: check the host, write the pinned key, run ssh, and turn "ssh itself failed"
    /// and "it timed out" into their errors. The rest is for the caller to read.
    fn call(&self, host: &Host, command: &str) -> Result<Exited, PortError> {
        if !valid_address(&host.address) || !valid_host_key(&host.host_key) {
            return Err(PortError::other("bad host"));
        }
        let (name, port) = split_address(&host.address);
        let entry = match port {
            Some(port) => format!("[{name}]:{port} {}\n", host.host_key),
            None => format!("{name} {}\n", host.host_key),
        };
        let known = KnownHosts::write(&self.work_dir, &entry)
            .map_err(|_| PortError::other("cannot write known hosts"))?;
        let args = self.argv(host, known.file(), command);
        let env = [("PATH", "PATH"), ("HOME", "HOME")]
            .iter()
            .filter_map(|(k, var)| Some((OsString::from(k), std::env::var_os(var)?)))
            .chain([(OsString::from("LANG"), OsString::from("C"))])
            .collect();
        let spawn = Spawn {
            program: self.ssh.clone(),
            args,
            env,
            timeout: self.command_timeout,
            stdout_cap: answer::MAX_STDOUT,
        };
        let done = self.exec.run(&spawn).map_err(|e| match e {
            ExecError::Timeout => PortError::unreachable("timeout"),
            ExecError::Spawn => PortError::other("cannot run ssh"),
        })?;
        drop(known);
        if done.code == Some(SSH_FAILED) {
            return Err(answer::unreachable_error(&done.stderr));
        }
        Ok(done)
    }
}

/// The exit status ssh itself uses for its own failure.
const SSH_FAILED: i32 = 255;

/// The host and the port of an inventory address (already checked).
fn split_address(address: &str) -> (&str, Option<&str>) {
    match address.rsplit_once(':') {
        // Port 22 is the default: OpenSSH looks such a host up under the bare name, so the
        // entry and the argv must not spell it out.
        Some((host, "22")) => (host, None),
        Some((host, port)) => (host, Some(port)),
        None => (address, None),
    }
}

impl Installer for SshInstaller {
    fn install(&self, host: &Host, version: &Version) -> Result<InstallAnswer, PortError> {
        if !host_accepts(version.as_str()) {
            return Err(PortError::other("refused: bad_version"));
        }
        let done = self.call(host, &self.install_command(version))?;
        let (code, stdout) = answer::checked(&done)?;
        answer::install_answer(code, stdout, version)
    }

    fn report(&self, host: &Host) -> Result<Report, PortError> {
        let done = self.call(host, "query")?;
        let (code, stdout) = answer::checked(&done)?;
        answer::query_answer(code, stdout)
    }
}

/// The `known_hosts` file of one call and the private directory it lives in. Both are
/// removed when this is dropped.
struct KnownHosts {
    dir: PathBuf,
    file: PathBuf,
}

static SEQUENCE: AtomicU64 = AtomicU64::new(0);

impl KnownHosts {
    fn write(parent: &Path, entry: &str) -> std::io::Result<KnownHosts> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        let name = format!(
            "kbf-updater-ssh-{}-{}-{nanos}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        );
        // Not recursive and not tolerant of an existing name: a directory someone else made
        // is an error, never reused.
        let dir = parent.join(name);
        DirBuilder::new().mode(0o700).create(&dir)?;
        Self::fill(dir, entry)
    }

    fn fill(dir: PathBuf, entry: &str) -> std::io::Result<KnownHosts> {
        let file = dir.join("known_hosts");
        let made = KnownHosts {
            dir,
            file: file.clone(),
        };
        // `ssh -o Key=value` splits on blanks and quotes, and expands `%` tokens in this
        // option: a path that has any of them is refused rather than escaped.
        let plain = file
            .to_str()
            .is_some_and(|p| !p.chars().any(|c| c.is_whitespace() || "\"'\\%".contains(c)));
        if !plain {
            return Err(std::io::ErrorKind::InvalidInput.into());
        }
        let mut out = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&file)?;
        out.write_all(entry.as_bytes())?;
        out.sync_all()?;
        Ok(made)
    }

    fn file(&self) -> &Path {
        &self.file
    }
}

impl Drop for KnownHosts {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.file);
        let _ = fs::remove_dir(&self.dir);
    }
}

#[cfg(test)]
mod contract_tests;
#[cfg(test)]
mod tests;
