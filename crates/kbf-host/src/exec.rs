//! Where steps happen: the host ([`HostExec`]), or a recording fake in tests.

use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use crate::path::{HostPath, Root};
use crate::step::{Cmd, Filter, HttpUrl, Owner, Perms, Step};
use crate::trust::{self, Untrusted};
use crate::user;

/// What is at a path, as a plan compares it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Entry {
    File {
        perms: Perms,
        owner: Owner,
        bytes: Vec<u8>,
    },
    Dir {
        perms: Perms,
        owner: Owner,
    },
    /// A symbolic link, never followed: what a [`Step::Symlink`] makes, and what a
    /// file or directory step replaces (or fails on).
    Link {
        target: PathBuf,
    },
    /// A socket or device: never what a plan wants, so a file or directory step replaces
    /// it (or fails) and a removal removes it.
    Other,
}

/// What is at a path without its contents, for a file a plan must not read whole (a
/// swapfile, a binary it only checks is executable) or cannot read (another user's key).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Meta {
    pub kind: Kind,
    pub perms: Perms,
    pub owner: Owner,
    /// The size in bytes.
    pub len: u64,
}

/// The kind of thing a [`Meta`] describes; a link is [`Kind::Other`], never followed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
    Other,
}

/// Reads and changes a host. [`HostExec`] is the real one; tests use a fake that
/// records each step.
pub trait Exec {
    /// The effective uid the steps would run with.
    fn euid(&self) -> u32;

    /// What is at `path` now; `None` when nothing is.
    fn inspect(&self, path: &HostPath) -> io::Result<Option<Entry>>;

    /// What is at `path` now, without reading it; `None` when nothing is.
    fn stat(&self, path: &HostPath) -> io::Result<Option<Meta>>;

    /// Run `cmd` to read something while planning (does a bucket exist, what does a
    /// Service hold): in a dry run as in an apply, so it must change nothing. A
    /// non-zero exit is not an error here; the caller reads the status.
    fn query(&self, cmd: &Cmd) -> io::Result<Output>;

    /// One `GET` of `url` while planning; the status code of the answer.
    fn http_status(&self, url: &HttpUrl) -> io::Result<u16>;

    /// Carry out one step.
    fn apply(&mut self, step: &Step) -> io::Result<()>;

    /// Refuses unless the running binary and every directory above it are owned by root
    /// and writable by no one else; asked only before applying as root.
    fn check_binary(&self) -> Result<(), Untrusted>;
}

/// The real host, with every path joined to `root`.
#[derive(Clone, Debug)]
pub struct HostExec {
    root: Root,
}

impl HostExec {
    pub fn new(root: Root) -> HostExec {
        HostExec { root }
    }
}

impl Exec for HostExec {
    fn euid(&self) -> u32 {
        rustix::process::geteuid().as_raw()
    }

    fn inspect(&self, path: &HostPath) -> io::Result<Option<Entry>> {
        let Some(meta) = self.stat(path)? else {
            return Ok(None);
        };
        let (perms, owner) = (meta.perms, meta.owner);
        Ok(Some(match meta.kind {
            Kind::File => Entry::File {
                perms,
                owner,
                bytes: fs::read(self.root.join(path))?,
            },
            Kind::Dir => Entry::Dir { perms, owner },
            Kind::Other => link_or_other(fs::read_link(self.root.join(path)))?,
        }))
    }

    fn stat(&self, path: &HostPath) -> io::Result<Option<Meta>> {
        let meta = match fs::symlink_metadata(self.root.join(path)) {
            Ok(meta) => meta,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let kind = if meta.is_file() {
            Kind::File
        } else if meta.is_dir() {
            Kind::Dir
        } else {
            Kind::Other
        };
        Ok(Some(Meta {
            kind,
            perms: Perms::new(meta.mode() & 0o7777),
            owner: Owner {
                uid: meta.uid(),
                gid: meta.gid(),
            },
            len: meta.len(),
        }))
    }

    fn apply(&mut self, step: &Step) -> io::Result<()> {
        match step {
            Step::WriteFile {
                path,
                perms,
                owner,
                bytes,
            } => write_file(&self.root.join(path), *perms, *owner, bytes),
            Step::Mkdir { path, perms, owner } => {
                let full = self.root.join(path);
                // An existing directory (not a link to one) only gets its permissions
                // and owner set; anything else at the path fails the step.
                match fs::create_dir(&full) {
                    Ok(()) => {}
                    Err(e)
                        if e.kind() == io::ErrorKind::AlreadyExists
                            && fs::symlink_metadata(&full).is_ok_and(|m| m.is_dir()) => {}
                    Err(e) => return Err(e),
                }
                set_perms_owner(&full, *perms, *owner)
            }
            Step::Symlink { path, target } => symlink(&self.root.join(path), target),
            Step::Remove { path } => {
                let full = self.root.join(path);
                if fs::symlink_metadata(&full)?.is_dir() {
                    fs::remove_dir(&full)
                } else {
                    fs::remove_file(&full)
                }
            }
            Step::Run(cmd) => succeeded(cmd, output(cmd)?).map(drop),
            Step::Capture {
                cmd,
                filter,
                path,
                perms,
                owner,
            } => capture(cmd, *filter, &self.root.join(path), *perms, *owner),
            Step::WaitHttp { url, deadline } => wait_http(url, *deadline),
        }
    }

    fn query(&self, cmd: &Cmd) -> io::Result<Output> {
        output(cmd)
    }

    fn http_status(&self, url: &HttpUrl) -> io::Result<u16> {
        get_status(url, ATTEMPT)
    }

    fn check_binary(&self) -> Result<(), Untrusted> {
        trust::check_running_binary()
    }
}

/// What `read_link` on a path that is neither a file nor a directory says it is: a link
/// with its target, or (EINVAL, not a link) a socket or device.
fn link_or_other(read: io::Result<PathBuf>) -> io::Result<Entry> {
    match read {
        Ok(target) => Ok(Entry::Link { target }),
        Err(e) if e.kind() == io::ErrorKind::InvalidInput => Ok(Entry::Other),
        Err(e) => Err(e),
    }
}

/// Point the link at `full` to `target`, atomically: a fresh link is made beside it and
/// renamed over it, so a reader of `full` sees the old target or the new one, never no
/// link. Anything at `full` that is not a link (a file, a directory) is refused, never
/// replaced. The directory is synced after the rename, so the new link survives a power
/// cut once the step has succeeded.
fn symlink(full: &Path, target: &Path) -> io::Result<()> {
    match fs::symlink_metadata(full) {
        Ok(meta) if !meta.file_type().is_symlink() => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!(
                    "{} is not a symbolic link; refusing to replace it",
                    full.display()
                ),
            ));
        }
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    let tmp = temp_beside(full);
    // A link left by an interrupted run.
    match fs::remove_file(&tmp) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    std::os::unix::fs::symlink(target, &tmp)?;
    fs::rename(&tmp, full)?;
    // For a power cut, which no test makes: no test fails without this sync.
    let parent = full
        .parent()
        .expect("a HostPath names a file in a directory");
    fs::File::open(parent)?.sync_all()
}

/// The name a step makes its new file or link under before renaming it over `full`.
fn temp_beside(full: &Path) -> PathBuf {
    let name = full
        .file_name()
        .expect("a HostPath names a file")
        .to_string_lossy();
    full.with_file_name(format!(".{name}.kbf-host-tmp"))
}

/// Write `bytes` beside `full` and rename the copy over it, so a reader sees the old
/// file or the new one, never a part.
fn write_file(full: &Path, perms: Perms, owner: Option<Owner>, bytes: &[u8]) -> io::Result<()> {
    let tmp = temp_beside(full);
    // A copy left by an interrupted run; `create_new` below never follows a link there.
    match fs::remove_file(&tmp) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)?;
    file.write_all(bytes)?;
    set_perms_owner(&tmp, perms, owner)?;
    file.sync_all()?;
    fs::rename(&tmp, full)?;
    // The rename is in the directory, not yet on disk: without this a power cut can bring
    // back the old file after a caller was told the new one is there.
    // A path with no parent is `/`, which was not written; a file always has one.
    fs::File::open(full.parent().unwrap_or(Path::new("/")))?.sync_all()
}

fn set_perms_owner(full: &Path, perms: Perms, owner: Option<Owner>) -> io::Result<()> {
    if let Some(o) = owner {
        std::os::unix::fs::chown(full, Some(o.uid), Some(o.gid))?;
    }
    // After the chown, which clears set-id bits on some systems.
    fs::set_permissions(full, fs::Permissions::from_mode(perms.bits()))
}

/// Run `cmd` to its end with its input written to a pipe, keeping both outputs. The
/// input is written before the output is read, so it must fit in a pipe's buffer (64
/// KiB on Linux) unless the tool reads all of it before writing much.
///
/// A tool may exit, or close its input, without reading all of it (`false`, or a tool
/// that refuses its arguments first). The write then fails with EPIPE, or succeeds into
/// the pipe's buffer, depending on timing and the input's size; neither says anything
/// about the tool. So EPIPE is not an error here: the tool's exit status is the verdict,
/// as in a shell pipeline, and the caller reads it ([`succeeded`]).
///
/// With `cmd.user` set, the child takes that user's groups, group and uid before it
/// starts the tool ([`user::become_user`]); as anyone but root that fails and the
/// tool never starts.
fn output(cmd: &Cmd) -> io::Result<Output> {
    let mut command = Command::new(cmd.program.as_str());
    command
        .args(&cmd.args)
        .env_clear()
        .envs(cmd.env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(u) = &cmd.user {
        // SAFETY: the closure runs in the child between fork and exec; `become_user`
        // makes three system calls on numbers the parent copied in, and allocates
        // nothing. Not `Command::uid`: std runs that before any `pre_exec` closure and
        // clears the groups with it, after which they can no longer be set.
        unsafe {
            command.pre_exec(user::switch_to(u));
        }
    }
    let mut child = command.spawn()?;
    // Dropped at the end of the statement, which closes the pipe: the tool reads EOF.
    fed(child
        .stdin
        .take()
        .expect("stdin is piped")
        .write_all(&cmd.stdin))?;
    child.wait_with_output()
}

/// The result of writing a child's input, with EPIPE (the child stopped reading) read as
/// done: the child's exit status decides, see [`output`].
fn fed(written: io::Result<()>) -> io::Result<()> {
    match written {
        Err(e) if e.kind() != io::ErrorKind::BrokenPipe => Err(e),
        _ => Ok(()),
    }
}

/// `out` when `cmd` exited 0; otherwise an error naming the exit and its stderr.
pub fn succeeded(cmd: &Cmd, out: Output) -> io::Result<Output> {
    if out.status.success() {
        return Ok(out);
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    Err(io::Error::other(format!(
        "{} exited with {}: {}",
        cmd.program,
        out.status,
        stderr.trim_end()
    )))
}

fn capture(
    cmd: &Cmd,
    filter: Filter,
    full: &Path,
    perms: Perms,
    owner: Option<Owner>,
) -> io::Result<()> {
    let out = succeeded(cmd, output(cmd)?)?;
    let bytes = (filter.apply)(&out.stdout).map_err(io::Error::other)?;
    write_file(full, perms, owner, &bytes)
}

/// How long one attempt may take, and the pause between attempts.
const ATTEMPT: Duration = Duration::from_secs(2);
const PAUSE: Duration = Duration::from_millis(200);

fn wait_http(url: &HttpUrl, deadline: Duration) -> io::Result<()> {
    let end = Instant::now() + deadline;
    loop {
        let last = match get_status(url, ATTEMPT) {
            Ok(status) if (200..300).contains(&status) => return Ok(()),
            Ok(status) => format!("status {status}"),
            Err(e) => e.to_string(),
        };
        let now = Instant::now();
        if now >= end {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("{url} did not answer 2xx within {deadline:?}; last: {last}"),
            ));
        }
        std::thread::sleep(PAUSE.min(end - now));
    }
}

/// One `GET`; the status code of the answer.
fn get_status(url: &HttpUrl, timeout: Duration) -> io::Result<u16> {
    let addr = (url.host(), url.port())
        .to_socket_addrs()?
        .next()
        .ok_or(io::Error::from(io::ErrorKind::AddrNotAvailable))?;
    let mut stream = TcpStream::connect_timeout(&addr, timeout)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    let request = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
        url.path(),
        url.authority()
    );
    stream.write_all(request.as_bytes())?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    // `HTTP/1.1 200 OK`
    line.split(' ')
        .nth(1)
        .and_then(|code| code.parse().ok())
        .ok_or_else(|| io::Error::other(format!("not an HTTP status line: {:?}", line.trim_end())))
}

#[cfg(test)]
#[path = "exec_tests.rs"]
mod tests;
