//! The socket the key is served on.

use std::convert::Infallible;
use std::fmt;
use std::fs::{self, DirBuilder};
use std::io::{self, ErrorKind};
use std::ops::Deref;
use std::os::fd::OwnedFd;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::time::Duration;

use kbf_ca::{Algorithm, Authority, FileSigner, Policy, Signer};
use rustix::fs::{FlockOperation, Mode, OFlags};
use x509_parser::prelude::{FromDer, X509Certificate};

use crate::error::SignerError;
use crate::group::{ClientGroup, share};
use crate::validate::{Route, check_tbs};
use crate::wire;

/// How long a peer may take to send its request or take the answer.
const IO_TIMEOUT: Duration = Duration::from_secs(5);

/// Gives up the process's dumpable flag (`PR_SET_DUMPABLE` 0): no other process, even one
/// of the same account, can read its memory with ptrace or `/proc/<pid>/mem`, and a crash
/// writes no core file with the key in it. Call it before the key is loaded.
///
/// # Errors
/// What the system answers; on a system other than Linux, which this process refuses to
/// run on (it cannot read a peer's credentials there).
pub fn harden() -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        rustix::process::set_dumpable_behavior(rustix::process::DumpableBehavior::NotDumpable)?;
        Ok(())
    }
    #[cfg(not(target_os = "linux"))]
    {
        Err(io::Error::from(ErrorKind::Unsupported))
    }
}

/// The user id and effective group id of the process at the other end of `stream`, as the
/// kernel recorded them when the connection was made (`SO_PEERCRED`).
#[cfg(target_os = "linux")]
fn peer_ids(stream: &UnixStream) -> io::Result<(u32, u32)> {
    let cred = rustix::net::sockopt::socket_peercred(stream)?;
    Ok((cred.uid.as_raw(), cred.gid.as_raw()))
}

/// Without `SO_PEERCRED` no peer can be told from another: none is admitted.
#[cfg(not(target_os = "linux"))]
fn peer_ids(_stream: &UnixStream) -> io::Result<(u32, u32)> {
    Err(io::Error::from(ErrorKind::Unsupported))
}

/// Makes the socket's directory (mode 0700) or accepts an existing one that is a real
/// directory owned by the running account: of mode 0700, or, when the socket is shared
/// with a group (`shared`), of mode 0700 or 0750. A shared directory that lets others in
/// is [`SignerError::WorldAccessible`].
fn prepare_dir(dir: &Path, shared: bool) -> Result<(), SignerError> {
    match DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e.into()),
    }
    let meta = fs::symlink_metadata(dir)?;
    let mode = meta.mode() & 0o7777;
    if !meta.is_dir() || meta.uid() != rustix::process::geteuid().as_raw() {
        return Err(SignerError::SocketDir);
    }
    if shared && mode & 0o007 != 0 {
        return Err(SignerError::WorldAccessible(dir.to_path_buf()));
    }
    if mode != 0o700 && !(shared && mode == 0o750) {
        return Err(SignerError::SocketDir);
    }
    Ok(())
}

/// Removes the socket at `path` unless a signer answers on it: a connection that is
/// accepted means one is serving, and a second must not take its place; only a refused
/// connection (nobody listens) marks a stale socket. Any other answer (the socket
/// vanished, permission denied) is an error: the caller holds the lock, so nothing else
/// removes it, and what cannot be judged is not removed.
fn remove_if_stale(path: &Path) -> Result<(), SignerError> {
    match UnixStream::connect(path) {
        Ok(_) => Err(SignerError::AlreadyServing(path.to_path_buf())),
        Err(e) if e.kind() == ErrorKind::ConnectionRefused => Ok(fs::remove_file(path)?),
        Err(e) => Err(e.into()),
    }
}

/// Takes the exclusive lock of `path`'s starters: `flock` on `<path>.lock` (mode 0600,
/// never through a symlink) in the socket's directory. Two starters that both found a
/// stale socket would otherwise both remove it and both bind; the one that cannot take
/// the lock fails without touching the socket. The lock is the returned [`StartersLock`]:
/// it lasts until that is dropped.
fn lock_starters(path: &Path) -> Result<StartersLock, SignerError> {
    let mut name = path.as_os_str().to_owned();
    name.push(".lock");
    let fd = rustix::fs::open(
        Path::new(&name),
        OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o600),
    )
    .map_err(io::Error::from)?;
    // Whatever the reason the lock is refused (on a local file system: another starter
    // or a serving signer holds it), nothing is touched.
    rustix::fs::flock(&fd, FlockOperation::NonBlockingLockExclusive)
        .map_err(|_| SignerError::AlreadyServing(path.to_path_buf()))?;
    Ok(StartersLock(fd))
}

/// The `<path>.lock` file of a socket path with its `flock` held.
///
/// An `flock` belongs to the open file description, not to the descriptor, and a
/// description lives until every copy of the descriptor is closed. A process forked while
/// the lock is held (any spawn of a child by another thread of this process) carries a
/// copy until it execs, so closing the file would leave the path locked for as long as
/// that takes. Dropping unlocks first, which no copy outlives, so the path can be bound
/// again once the drop returns.
#[derive(Debug)]
struct StartersLock(OwnedFd);

impl Drop for StartersLock {
    fn drop(&mut self) {
        // An error leaves the close that follows to release the lock, as before.
        let _ = rustix::fs::flock(&self.0, FlockOperation::Unlock);
    }
}

/// A socket bound by [`bind`], and the lock that keeps a second signer off its path. It
/// derefs to the [`UnixListener`]; dropping it closes the listener and frees the lock
/// (the socket file stays, stale, for the next start to replace).
#[derive(Debug)]
pub struct Bound {
    listener: UnixListener,
    _lock: StartersLock,
}

impl Deref for Bound {
    type Target = UnixListener;

    fn deref(&self) -> &UnixListener {
        &self.listener
    }
}

/// Binds the socket at `path`: in a directory of mode 0700 (made if missing), with mode
/// 0600. Starters are serialised by a lock held for the life of the result; a socket left
/// by a previous run (nobody listens on it) is replaced; one a signer is serving, or
/// anything else at `path`, is an error.
///
/// # Errors
/// [`SignerError::AlreadyServing`] for a live socket or a starter holding the lock;
/// otherwise other [`SignerError`]s.
pub fn bind(path: &Path) -> Result<Bound, SignerError> {
    bind_as(path, None)
}

/// [`bind`] for a socket shared with the group `gid`: the socket is mode 0660 and its
/// directory mode 0750, both owned by this account and group `gid`, and the directory is
/// opened to the group only once the socket is in place. A directory that lets others in
/// is refused ([`SignerError::WorldAccessible`]). The account must be a member of the
/// group (or root).
///
/// # Errors
/// As [`bind`], and [`SignerError::WorldAccessible`] and [`SignerError::Share`].
pub fn bind_for_group(path: &Path, gid: u32) -> Result<Bound, SignerError> {
    bind_as(path, Some(gid))
}

fn bind_as(path: &Path, gid: Option<u32>) -> Result<Bound, SignerError> {
    let dir = path.parent().ok_or(SignerError::SocketPath)?;
    prepare_dir(dir, gid.is_some())?;
    let lock = lock_starters(path)?;
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_socket() => remove_if_stale(path)?,
        Ok(_) => return Err(SignerError::SocketPath),
        // Nothing there (or nothing that can be looked at, which `bind` will say).
        Err(_) => {}
    }
    let listener = UnixListener::bind(path)?;
    match gid {
        None => fs::set_permissions(path, fs::Permissions::from_mode(0o600))?,
        Some(gid) => {
            share(path, gid, 0o660)?;
            share(dir, gid, 0o750)?;
        }
    }
    Ok(Bound {
        listener,
        _lock: lock,
    })
}

/// The signer's clock: Unix seconds.
type Clock = Box<dyn Fn() -> i64 + Send + Sync>;

/// The system clock in Unix seconds (negative before 1970, which every certificate's
/// validity is past, so all are refused).
pub(crate) fn system_now() -> i64 {
    rustix::time::clock_gettime(rustix::time::ClockId::Realtime).tv_sec
}

/// The CA key and the rules it signs by.
pub struct SignerServer<S: Signer> {
    clock: Clock,
    key: S,
    root_subject: Vec<u8>,
    allowed_uid: u32,
    client_group: Option<ClientGroup>,
    policy: Policy,
}

impl<S: Signer> fmt::Debug for SignerServer<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SignerServer")
    }
}

impl SignerServer<FileSigner> {
    /// The server of a CA directory (`ca.key`, `ca.pem`), which must be closed to others
    /// and owned by the running account, for peers running as `allowed_uid`.
    ///
    /// # Errors
    /// [`SignerError::Ca`] for a directory, key or root that is not fit to load.
    pub fn from_dir(dir: &Path, allowed_uid: u32, policy: Policy) -> Result<Self, SignerError> {
        let authority = Authority::load_dir(dir, policy)?;
        Self::new(
            FileSigner::load(dir)?,
            authority.root_der(),
            allowed_uid,
            policy,
        )
    }

    /// [`Self::from_dir`] for a CA directory shared with a group (`ensure_shared`): the
    /// directory may be readable by its group; the key must still be closed to it.
    ///
    /// # Errors
    /// As [`Self::from_dir`].
    pub fn from_shared_dir(
        dir: &Path,
        allowed_uid: u32,
        policy: Policy,
    ) -> Result<Self, SignerError> {
        let authority = Authority::load_dir_shared(dir, policy)?;
        Self::new(
            FileSigner::load_in_shared_dir(dir)?,
            authority.root_der(),
            allowed_uid,
            policy,
        )
    }
}

impl<S: Signer> SignerServer<S> {
    /// A server that signs with `key` for the CA whose self-signed root is `root_der`,
    /// for peers running as `allowed_uid`.
    ///
    /// # Errors
    /// [`SignerError::Root`] when the root does not parse or is for another key.
    pub fn new(
        key: S,
        root_der: &[u8],
        allowed_uid: u32,
        policy: Policy,
    ) -> Result<Self, SignerError> {
        let (_, root) = X509Certificate::from_der(root_der).map_err(|_| SignerError::Root)?;
        if root.public_key().raw != key.public_key() {
            return Err(SignerError::Root);
        }
        Ok(Self {
            clock: Box::new(system_now),
            key,
            root_subject: root.subject().as_raw().to_vec(),
            allowed_uid,
            client_group: None,
            policy,
        })
    }

    /// The same server, also serving the members of `group`: a peer is served when its
    /// user id is the configured one or `group` admits it. Every other peer is closed on
    /// without a byte, as before.
    #[must_use]
    pub fn with_client_group(mut self, group: ClientGroup) -> Self {
        self.client_group = Some(group);
        self
    }

    /// The same server reading `clock` (Unix seconds) instead of the system clock: for
    /// tests that fix time.
    #[must_use]
    pub fn with_clock(mut self, clock: impl Fn() -> i64 + Send + Sync + 'static) -> Self {
        self.clock = Box::new(clock);
        self
    }

    /// Serves `listener`, a thread to a connection, for as long as it accepts.
    ///
    /// # Errors
    /// Why `accept` failed.
    pub fn serve(&self, listener: &UnixListener) -> io::Result<Infallible> {
        std::thread::scope(|scope| {
            loop {
                let (stream, _) = listener.accept()?;
                scope.spawn(|| self.handle(stream));
            }
        })
    }

    /// One connection. A peer that goes away mid-request is not an error of the signer.
    fn handle(&self, mut stream: UnixStream) {
        let _ = self.answer(&mut stream);
    }

    fn answer(&self, stream: &mut UnixStream) -> io::Result<()> {
        let (uid, gid) = peer_ids(stream)?;
        let member = self
            .client_group
            .as_ref()
            .is_some_and(|g| g.admits(uid, gid));
        if uid != self.allowed_uid && !member {
            return Ok(());
        }
        stream.set_read_timeout(Some(IO_TIMEOUT))?;
        stream.set_write_timeout(Some(IO_TIMEOUT))?;
        let (kind, body) = wire::read_frame(stream)?;
        let (tag, payload) = self.respond(kind, &body);
        wire::write_frame(stream, tag, &payload)
    }

    fn respond(&self, kind: u8, body: &[u8]) -> (u8, Vec<u8>) {
        match kind {
            wire::PUBLIC_KEY => (wire::OK, self.key.public_key().to_vec()),
            wire::SIGN_NODE => self.sign(Route::Node, body),
            wire::SIGN_FRONTEND => self.sign(Route::Frontend, body),
            _ => (wire::FAILED, b"unknown request".to_vec()),
        }
    }

    fn sign(&self, route: Route, tbs: &[u8]) -> (u8, Vec<u8>) {
        if let Err(refusal) = check_tbs(tbs, route, self.policy, &self.root_subject, (self.clock)())
        {
            return (wire::REFUSED, refusal.to_string().into_bytes());
        }
        match self.key.sign(tbs, Algorithm::EcdsaP256Sha256) {
            Ok(signature) => (wire::OK, signature),
            Err(e) => (wire::FAILED, e.0.into_bytes()),
        }
    }
}

#[cfg(test)]
mod tests {
    use kbf_testkit::dir::TestDir;
    use kbf_testkit::forked::ForkedChild;

    use super::*;

    /// Catches: a starters' lock the drop releases only by closing its descriptor, which a
    /// process forked while it was held keeps held until it execs: a second starter is
    /// then refused with `AlreadyServing` for as long as a child is between fork and exec.
    /// The child here is held before exec until the lock has been taken again, so the
    /// interleaving is the failing one every time. The lock alone is taken, not `bind`:
    /// the bound socket has the same property (a forked child keeps the listener open,
    /// and so answering), which is not what this test is about.
    ///
    /// Catches: the `Drop` of `StartersLock` removed.
    #[test]
    fn a_child_forked_while_the_lock_was_held_does_not_keep_it_after_the_drop() {
        // `prepare_dir` wants a directory of mode 0700, which it makes itself: it is made
        // inside the test's own directory, which is removed with it.
        let base = TestDir::under(std::env::temp_dir(), "kbf-signer-unit");
        let dir = base.join("socket-dir");
        prepare_dir(&dir, false).expect("a directory of mode 0700");
        let socket = dir.join("sock");
        let first = lock_starters(&socket).expect("the first lock");
        let child = ForkedChild::before_exec();
        drop(first);
        let again = lock_starters(&socket);
        drop(again.expect("a lock after the drop, with the forked child still alive"));
        child.finish();
    }
}
