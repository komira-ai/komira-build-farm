//! Profile canaries: before a lease is handed a pool user, the daemon runs each door the
//! sandbox profile is meant to shut as that user, under the exact profile generated for
//! the lease, and the door must be refused (`EPERM` or `EACCES`). A slot with a canary not
//! refused is set aside, not offered and not counted among the node's action slots until
//! its next offer ([`first_passing`]); the lease takes the next slot, or waits as when
//! none is free. The profile is asserted, not trusted.
//!
//! A canary is a row of [`CANARIES`]: its name, the profile rule it stands for
//! (`deny`) and its [`Op`]. The probe is this binary's `__canary <name> <lease>` mode
//! ([`dispatch`]): it makes the call and prints one line, `canary <name> refused
//! <refusal>` (`errno=1`, `kern=1100`, ...) or `canary <name> allowed`, exiting
//! [`REFUSED`], [`ALLOWED`], [`FAILED`] (the call failed for another reason) or
//! [`USAGE`]. [`judge`] reads that back and
//! [`check`] runs the whole table and reports every canary that is not refused. A new
//! deny gets its canary by adding a row and an [`Op`] (the doors of [`doors`] are the
//! macOS-only calls: a Mach service name, Launch Services, a mount, launchd); the macOS
//! side ([`macos`](super::macos)) runs a probe as the slot's user under the lease's
//! profile.

use std::ffi::OsString;
use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub mod doors;

pub use doors::{Out, Refusal};

/// The argument after argv0 that selects this binary's canary probe mode.
pub const ARG: &str = "__canary";

/// How long one canary may take, launch included (a run took about 50 to 125 ms on the macOS runner).
pub const WITHIN: Duration = Duration::from_secs(10);

/// The probe's exit code when the door was refused.
pub const REFUSED: i32 = 0;
/// The probe's exit code when the door was open.
pub const ALLOWED: i32 = 10;
/// The probe's exit code when the call failed for a reason that is not a refusal.
pub const FAILED: i32 = 11;
/// The probe's exit code for arguments it does not know.
pub const USAGE: i32 = 12;

/// Where the write canary creates its file: outside the four paths a profile allows.
const OUTSIDE_DIR: &str = "/private/tmp";
/// The root-owned file the link canary tries to link into the lease directory.
const LINK_SOURCE: &str = "/etc/hosts";
/// The name the link canary uses in the lease directory.
const LINK_NAME: &str = ".kbf-canary-link";
/// The launchd job file the bootstrap canary writes in the lease directory.
const JOB_NAME: &str = ".kbf-canary-job.plist";
/// The directory the mount canary mounts a file system on, in the lease directory.
const MOUNT_NAME: &str = ".kbf-canary-mount";

/// What a canary does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    /// Create a file in [`OUTSIDE_DIR`].
    WriteOutside,
    /// `link(2)` [`LINK_SOURCE`] into the lease directory.
    LinkOutside,
    /// Check in a Mach service name with launchd (`bootstrap_check_in`).
    MachRegister,
    /// Ask the sandbox whether Launch Services may open an application (`sandbox_check`
    /// of `lsopen`).
    LsOpen,
    /// Ask the sandbox whether the protected stores
    /// ([`crate::network::PROTECTED_STORES`]) may be read or written (`sandbox_check` of
    /// `file-read-data` and `file-write-data` on each).
    ReadProtected,
    /// Mount a `devfs` on a directory of the lease (`mount(2)`).
    Mount,
    /// Bootstrap a `Background` `KeepAlive` launchd agent whose file is in the lease
    /// directory into the user's domain (`launchctl bootstrap`).
    LaunchdBootstrap,
}

impl Op {
    /// The refusals this op's door gives, exactly: any other outcome is not a refusal.
    #[must_use]
    pub const fn refusals(self) -> &'static [Refusal] {
        match self {
            Self::WriteOutside | Self::LinkOutside => {
                &[Refusal::Errno(libc::EPERM), Refusal::Errno(libc::EACCES)]
            }
            Self::MachRegister => &[Refusal::Kern(doors::BOOTSTRAP_NOT_PRIVILEGED)],
            Self::LsOpen | Self::ReadProtected => &[Refusal::Sandbox(1)],
            Self::Mount => &[Refusal::Errno(libc::EPERM)],
            Self::LaunchdBootstrap => &[Refusal::Bootstrap(doors::BOOTSTRAP_IO_ERROR)],
        }
    }
}

/// One door the profile shuts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Canary {
    /// Its name in the probe's arguments and in the log.
    pub name: &'static str,
    /// The line of the generated profile that shuts the door; for the launchd canary,
    /// `(allow default)`: no rule shuts that door, the sandbox does, whatever the profile.
    pub deny: &'static str,
    pub op: Op,
}

/// Every canary, in the order they run. A rule of the profile gets its row here.
pub const CANARIES: &[Canary] = &[
    Canary {
        name: "write-outside",
        deny: "(deny file-write*)",
        op: Op::WriteOutside,
    },
    Canary {
        name: "link-outside",
        deny: "(deny file-write*)",
        op: Op::LinkOutside,
    },
    Canary {
        name: "mach-register",
        deny: "(deny mach-register)",
        op: Op::MachRegister,
    },
    Canary {
        name: "lsopen",
        deny: "(deny lsopen)",
        op: Op::LsOpen,
    },
    Canary {
        name: "read-protected",
        deny: crate::network::PROTECTED_DENY,
        op: Op::ReadProtected,
    },
    Canary {
        name: "file-mount",
        deny: "(deny file-mount)",
        op: Op::Mount,
    },
    Canary {
        name: "launchd-bootstrap",
        deny: "(allow default)",
        op: Op::LaunchdBootstrap,
    },
];

/// The calls a probe makes, so a test can stand in for the kernel.
pub trait Fs {
    /// Creates `path`, which must not exist.
    ///
    /// # Errors
    /// The call's error.
    fn create(&self, path: &Path) -> io::Result<()>;
    /// Hard-links `from` as `to`.
    ///
    /// # Errors
    /// The call's error.
    fn link(&self, from: &Path, to: &Path) -> io::Result<()>;
    /// Removes `path` (a file, or an empty directory), if it is there.
    fn remove(&self, path: &Path);
    /// Writes `text` to `path`, which must not exist.
    ///
    /// # Errors
    /// The call's error.
    fn write(&self, path: &Path, text: &str) -> io::Result<()>;
    /// Makes the directory `path`.
    ///
    /// # Errors
    /// The call's error.
    fn make_dir(&self, path: &Path) -> io::Result<()>;
    /// Runs `program` with `args` and waits for it.
    ///
    /// # Errors
    /// The program could not be started.
    fn run(&self, program: &str, args: &[String]) -> io::Result<Out>;
    /// Asks launchd to check in the Mach service `name` (`bootstrap_check_in`): the
    /// `kern_return_t` it answers.
    ///
    /// # Errors
    /// The call does not exist on this platform.
    fn check_in(&self, name: &str) -> io::Result<i32>;
    /// Whether the sandbox of this process denies `operation` (`sandbox_check`).
    ///
    /// # Errors
    /// The question cannot be asked on this platform.
    fn sandbox_denies(&self, operation: &str) -> io::Result<bool>;
    /// Whether the sandbox of this process denies `operation` on `path` (`sandbox_check`
    /// with a path filter).
    ///
    /// # Errors
    /// The question cannot be asked on this platform.
    fn sandbox_denies_path(&self, operation: &str, path: &Path) -> io::Result<bool>;
    /// Mounts a `devfs` on the directory `dir` (`mount(2)`).
    ///
    /// # Errors
    /// The call's error.
    fn mount_devfs(&self, dir: &Path) -> io::Result<()>;
    /// Unmounts what is mounted on `dir`, by force.
    ///
    /// # Errors
    /// The call's error.
    fn unmount(&self, dir: &Path) -> io::Result<()>;
    /// The real uid of this process.
    fn uid(&self) -> u32;
}

/// The real calls.
#[derive(Debug)]
pub struct RealFs;

impl Fs for RealFs {
    fn create(&self, path: &Path) -> io::Result<()> {
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map(drop)
    }

    fn link(&self, from: &Path, to: &Path) -> io::Result<()> {
        std::fs::hard_link(from, to)
    }

    fn remove(&self, path: &Path) {
        if std::fs::remove_file(path).is_err() {
            let _ = std::fs::remove_dir(path);
        }
    }

    fn write(&self, path: &Path, text: &str) -> io::Result<()> {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt as _;
        // An explicit mode: launchd refuses a job file a group or others may write, and
        // that must not depend on the daemon's umask.
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o644)
            .open(path)?;
        file.write_all(text.as_bytes())
    }

    fn make_dir(&self, path: &Path) -> io::Result<()> {
        std::fs::create_dir(path)
    }

    fn run(&self, program: &str, args: &[String]) -> io::Result<Out> {
        doors::run(program, args)
    }

    fn check_in(&self, name: &str) -> io::Result<i32> {
        doors::check_in(name)
    }

    fn sandbox_denies(&self, operation: &str) -> io::Result<bool> {
        doors::sandbox_denies(operation)
    }

    fn sandbox_denies_path(&self, operation: &str, path: &Path) -> io::Result<bool> {
        doors::sandbox_denies_path(operation, path)
    }

    fn mount_devfs(&self, dir: &Path) -> io::Result<()> {
        doors::mount_devfs(dir)
    }

    fn unmount(&self, dir: &Path) -> io::Result<()> {
        doors::unmount(dir)
    }

    fn uid(&self) -> u32 {
        // SAFETY: `getuid` takes no arguments and cannot fail.
        unsafe { libc::getuid() }
    }
}

/// What a probe saw.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Seen {
    /// Refused in the way the door is refused (`EPERM`, a `kern_return`, ...).
    Refused(Refusal),
    /// The call succeeded: the door is open.
    Allowed,
    /// The call failed, but not with a refusal (say, the directory is missing): the
    /// canary proved nothing.
    Failed(String),
}

/// `result` as what the probe saw: only `EPERM` and `EACCES` are refusals.
fn classify(result: io::Result<()>) -> Seen {
    match result {
        Ok(()) => Seen::Allowed,
        Err(e) => match e.raw_os_error() {
            Some(n) if n == libc::EPERM || n == libc::EACCES => Seen::Refused(Refusal::Errno(n)),
            _ => Seen::Failed(e.to_string()),
        },
    }
}

/// Makes `op`'s call with `lease` as the lease directory, and removes what an open door
/// let it create.
pub fn probe(fs: &dyn Fs, op: Op, lease: &Path) -> Seen {
    let (made, path) = match op {
        Op::WriteOutside => {
            let path = Path::new(OUTSIDE_DIR).join(format!(".kbf-canary-{}", std::process::id()));
            (fs.create(&path), path)
        }
        Op::LinkOutside => {
            let path = lease.join(LINK_NAME);
            (fs.link(Path::new(LINK_SOURCE), &path), path)
        }
        Op::MachRegister => return doors::mach_register(fs),
        Op::LsOpen => return doors::ls_open(fs),
        Op::ReadProtected => return doors::read_protected(fs),
        Op::Mount => return doors::mount(fs, &lease.join(MOUNT_NAME)),
        Op::LaunchdBootstrap => return doors::bootstrap(fs, &lease.join(JOB_NAME)),
    };
    let seen = classify(made);
    if seen == Seen::Allowed {
        fs.remove(&path);
    }
    seen
}

/// The probe mode: when `args` (argv, with argv0) select it, runs the canary they name
/// against the lease directory they give and returns the exit code; `None` otherwise.
pub fn dispatch(args: &[OsString], fs: &dyn Fs, out: &mut dyn io::Write) -> Option<i32> {
    if args.get(1).is_none_or(|a| a != ARG) {
        return None;
    }
    let found = args.get(2).and_then(|name| {
        CANARIES
            .iter()
            .find(|c| name.to_str() == Some(c.name))
            .zip(args.get(3))
    });
    let Some((canary, lease)) = found else {
        let _ = writeln!(out, "canary: usage: {ARG} <name> <lease dir>");
        return Some(USAGE);
    };
    let seen = probe(fs, canary.op, &PathBuf::from(lease));
    let name = canary.name;
    let (line, code) = match seen {
        Seen::Refused(refusal) => (format!("canary {name} refused {refusal}"), REFUSED),
        Seen::Allowed => (format!("canary {name} allowed"), ALLOWED),
        Seen::Failed(why) => (format!("canary {name} failed {why}"), FAILED),
    };
    let _ = writeln!(out, "{line}");
    Some(code)
}

/// What running a probe gave the daemon.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ran {
    /// The exit code; `None` if a signal ended it.
    pub code: Option<i32>,
    pub stdout: String,
    /// Launch to exit, as the daemon measured it.
    pub took: Duration,
}

/// What a canary showed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Verdict {
    #[error("refused ({0})")]
    Refused(Refusal),
    /// The call succeeded (a check-in answered 0, a mount was made, a bootstrap loaded).
    #[error("NOT refused: the call succeeded")]
    Allowed,
    /// Anything else, including an answer that is no refusal of its door (a Mach
    /// check-in answering 1102 with the rule dropped): the log says what was seen.
    #[error("could not run: {0}")]
    CannotRun(String),
}

/// What `ran` shows for `canary`: refused only when the probe exited [`REFUSED`] and
/// said so with one of the refusals its op is refused with ([`Op::refusals`]); anything
/// unrecognised is a canary that could not run.
#[must_use]
pub fn judge(canary: &Canary, ran: &Ran) -> Verdict {
    let said = ran.stdout.trim();
    let name = canary.name;
    let refused = said
        .strip_prefix(&format!("canary {name} refused "))
        .and_then(Refusal::parse)
        .filter(|r| canary.op.refusals().contains(r));
    match (ran.code, refused) {
        (Some(REFUSED), Some(refusal)) => Verdict::Refused(refusal),
        (Some(ALLOWED), _) if said == format!("canary {name} allowed") => Verdict::Allowed,
        _ => Verdict::CannotRun(format!("exit {:?}, output {said:?}", ran.code)),
    }
}

/// A canary that was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Passed {
    pub name: &'static str,
    pub refusal: Refusal,
    pub took: Duration,
}

/// The first canary that was not refused, and why.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("canary {name} ({deny}): {verdict}")]
pub struct Failure {
    pub name: &'static str,
    pub deny: &'static str,
    pub verdict: Verdict,
}

/// What [`check`] found: the canaries that were refused and the ones that were not.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    pub passed: Vec<Passed>,
    pub failed: Vec<Failure>,
}

impl Report {
    /// Whether every canary was refused.
    #[must_use]
    pub fn all_refused(&self) -> bool {
        self.failed.is_empty()
    }

    /// The failures, one line each, for a log: `name (deny): result; ...`.
    #[must_use]
    pub fn failures(&self) -> String {
        let lines: Vec<String> = self.failed.iter().map(ToString::to_string).collect();
        lines.join("; ")
    }
}

/// Runs every canary of `canaries` with `run` (which starts the probe as the slot's user
/// under the lease's profile), none skipped after a failure, so each can fail on
/// its own. A canary that could not be started or timed out is a failure too. Every
/// result is logged with the canary's name and how long it took.
pub fn check(canaries: &[Canary], run: &mut dyn FnMut(&Canary) -> io::Result<Ran>) -> Report {
    check_until(canaries, &|| false, run)
}

/// [`check`] that starts no canary once `cancelled` says so: the report holds what ran.
pub fn check_until(
    canaries: &[Canary],
    cancelled: &dyn Fn() -> bool,
    run: &mut dyn FnMut(&Canary) -> io::Result<Ran>,
) -> Report {
    let mut report = Report::default();
    for canary in canaries {
        if cancelled() {
            break;
        }
        let verdict = match run(canary) {
            Ok(ran) => match judge(canary, &ran) {
                Verdict::Refused(refusal) => {
                    tracing::info!(canary = canary.name, %refusal, took = ?ran.took, "profile canary refused");
                    report.passed.push(Passed {
                        name: canary.name,
                        refusal,
                        took: ran.took,
                    });
                    continue;
                }
                other => other,
            },
            Err(e) => Verdict::CannotRun(e.to_string()),
        };
        tracing::error!(canary = canary.name, deny = canary.deny, %verdict, "profile canary failed: this slot is set aside");
        report.failed.push(Failure {
            name: canary.name,
            deny: canary.deny,
            verdict,
        });
    }
    report
}

/// What probing a slot decided.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Probe {
    /// The slot may run the lease.
    Pass,
    /// The slot is not to run it; the text says which canaries failed.
    Aside(String),
}

/// The slots a lease is offered, for [`first_passing`]. The offer puts the slot it takes
/// in a cell it only borrows, owned by the caller (the runtime's scope, which drops the
/// lease directory before the slot), so that a failure of any step, a drop or a panic
/// leaves the slot with the caller and never frees it while the lease directory exists.
pub trait Offer {
    type Err;

    /// Takes the next free slot (waiting when none is) into the cell; or says why the
    /// lease ends instead (a kill while waiting, or while the lease was prepared with
    /// the slot held).
    fn next(&mut self) -> impl Future<Output = Result<(), Self::Err>> + Send;

    /// Hands the lease's directory to the held slot and runs its canaries.
    fn probe(&mut self) -> impl Future<Output = Result<Probe, Self::Err>> + Send;

    /// Takes the lease's directory back from the held slot and sets the slot aside;
    /// `why` says which canaries failed. The cell is empty afterwards. If the directory
    /// cannot be taken back the slot stays in the cell and the lease ends with the
    /// error, so that the directory is removed before the slot is released.
    fn aside(&mut self, why: &str) -> impl Future<Output = Result<(), Self::Err>> + Send;
}

/// Takes slots until one passes its probe, setting each one that does not aside (it is
/// not offered again until its next offer). A canary failure never fails the action: the
/// next slot is taken, and with none free [`Offer::next`] waits, as it does when every
/// slot is held. The slot that passes stays in the offer's cell.
///
/// # Errors
/// What [`Offer::next`], [`Offer::probe`] or [`Offer::aside`] returns (a kill, a
/// hand-over that failed).
pub async fn first_passing<O: Offer + Send>(offer: &mut O) -> Result<(), O::Err> {
    loop {
        offer.next().await?;
        match offer.probe().await? {
            Probe::Pass => return Ok(()),
            Probe::Aside(why) => offer.aside(&why).await?,
        }
    }
}

/// How `work` ended when it was raced against a kill.
#[derive(Debug, PartialEq, Eq)]
pub enum Raced<T, K> {
    /// It finished first, with this.
    Done(T),
    /// The kill came first; `work` has stopped (it saw `cancel` and returned).
    Killed(K),
}

/// Sets the flag when dropped while armed.
struct CancelOnDrop<'a> {
    cancel: &'a std::sync::atomic::AtomicBool,
    armed: bool,
}

impl Drop for CancelOnDrop<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.cancel.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }
}

/// Runs `work`, which does blocking work that cannot be cancelled from outside and
/// checks `cancel` between its steps, until it finishes or `killed` resolves. On a
/// kill it sets `cancel` and **waits for `work` to return**, so that nothing it does
/// (a canary launched as the slot's user, an owner change) happens after the caller
/// has been told the lease stopped and goes on to remove the directory and release the
/// slot. If this future is itself dropped before it ends (the runtime shut down, or a
/// panic unwinding through a poll), the flag is set too, so the blocking work stops
/// without a kill message; it is not waited for then, but it stops at its next step.
pub async fn until_killed<T, K>(
    work: impl Future<Output = T>,
    cancel: &std::sync::atomic::AtomicBool,
    killed: impl Future<Output = Option<K>>,
) -> Raced<T, K> {
    let mut guard = CancelOnDrop {
        cancel,
        armed: true,
    };
    tokio::pin!(work);
    tokio::pin!(killed);
    let raced = tokio::select! {
        done = &mut work => Raced::Done(done),
        Some(by) = &mut killed => {
            cancel.store(true, std::sync::atomic::Ordering::SeqCst);
            let _ = work.await;
            Raced::Killed(by)
        }
    };
    guard.armed = false;
    raced
}

/// What a canary can leave in the lease directory: the link the link canary made, the
/// job file and the mount point of the others. The daemon removes them itself
/// after the canaries, without following them, so that a link to a file of the
/// daemon's is never left in a tree that is handed to the slot's user.
pub fn remove_leftovers(fs: &dyn Fs, lease: &Path) {
    for name in [LINK_NAME, JOB_NAME, MOUNT_NAME] {
        fs.remove(&lease.join(name));
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    type Answer = fn() -> io::Result<()>;

    /// A kernel that answers every file call with `answer`, every tool with `out` and
    /// every check-in with `kern`, and records them.
    struct Fake {
        answer: Answer,
        out: Out,
        kern: i32,
        /// What the sandbox is asked: denies, allows, or cannot be asked.
        denies: Option<bool>,
        /// What the sandbox is asked about a path: the operations it denies on the paths
        /// (an operation not listed is allowed), or `None` when it cannot be asked.
        path_denies: Option<&'static [&'static str]>,
        /// What mounting a `devfs` answers.
        mount: Answer,
        /// What unmounting answers.
        unmount: Answer,
        /// Whether starting a tool fails.
        no_tools: bool,
        calls: RefCell<Vec<String>>,
    }

    impl Fake {
        fn new(answer: Answer) -> Self {
            Self {
                answer,
                out: Out::default(),
                kern: 0,
                denies: Some(false),
                path_denies: Some(&[]),
                mount: || Ok(()),
                unmount: || Ok(()),
                no_tools: false,
                calls: RefCell::default(),
            }
        }

        fn note(&self, call: String) {
            self.calls.borrow_mut().push(call);
        }
    }

    impl Fs for Fake {
        fn create(&self, path: &Path) -> io::Result<()> {
            self.note(format!("create {}", path.display()));
            (self.answer)()
        }
        fn link(&self, from: &Path, to: &Path) -> io::Result<()> {
            self.note(format!("link {} {}", from.display(), to.display()));
            (self.answer)()
        }
        fn remove(&self, path: &Path) {
            self.note(format!("remove {}", path.display()));
        }
        fn write(&self, path: &Path, text: &str) -> io::Result<()> {
            self.note(format!("write {} {text}", path.display()));
            (self.answer)()
        }
        fn make_dir(&self, path: &Path) -> io::Result<()> {
            self.note(format!("make_dir {}", path.display()));
            (self.answer)()
        }
        fn run(&self, program: &str, args: &[String]) -> io::Result<Out> {
            self.note(format!("run {program} {}", args.join(" ")));
            if self.no_tools {
                return Err(io::Error::from_raw_os_error(libc::ENOENT));
            }
            let helper = ["bootout"];
            if args.first().is_some_and(|a| helper.contains(&a.as_str())) {
                return Ok(Out {
                    code: Some(0),
                    text: String::new(),
                });
            }
            Ok(self.out.clone())
        }
        fn check_in(&self, name: &str) -> io::Result<i32> {
            self.note(format!("check_in {name}"));
            Ok(self.kern)
        }
        fn sandbox_denies(&self, operation: &str) -> io::Result<bool> {
            self.note(format!("sandbox_denies {operation}"));
            self.denies.ok_or_else(|| io::Error::other("cannot ask"))
        }
        fn sandbox_denies_path(&self, operation: &str, path: &Path) -> io::Result<bool> {
            self.note(format!(
                "sandbox_denies_path {operation} {}",
                path.display()
            ));
            let denied = self
                .path_denies
                .ok_or_else(|| io::Error::other("cannot ask"))?;
            Ok(denied.contains(&operation))
        }
        fn mount_devfs(&self, dir: &Path) -> io::Result<()> {
            self.note(format!("mount_devfs {}", dir.display()));
            (self.mount)()
        }
        fn unmount(&self, dir: &Path) -> io::Result<()> {
            self.note(format!("unmount {}", dir.display()));
            (self.unmount)()
        }
        fn uid(&self) -> u32 {
            501
        }
    }

    /// What a world looks like to a probe.
    #[derive(Clone, Copy, Debug)]
    enum World {
        /// The door is shut, in the way the op's door is.
        Shut,
        /// The door is open.
        Open,
        /// The call fails, but not with a refusal.
        Broken,
    }

    /// A fake kernel in which `op`'s door is as `world` says.
    fn world(op: Op, world: World) -> Fake {
        let tool = |code: i32, stderr: &str| Out {
            code: Some(code),
            text: stderr.to_owned(),
        };
        let mut fake = Fake::new(match (op, world) {
            (_, World::Open) => || Ok(()),
            (Op::WriteOutside | Op::LinkOutside, World::Shut) => || errno(libc::EACCES),
            (_, World::Shut) => || Ok(()),
            (_, World::Broken) => || errno(libc::ENOENT),
        });
        match (op, world) {
            (Op::MachRegister, World::Shut) => fake.kern = 1100,
            (Op::MachRegister, World::Broken) => fake.kern = 1102,
            (Op::LsOpen, World::Shut) => fake.denies = Some(true),
            (Op::LsOpen, World::Broken) => fake.denies = None,
            (Op::ReadProtected, World::Shut) => {
                fake.path_denies = Some(&["file-read-data", "file-write-data"]);
            }
            (Op::ReadProtected, World::Broken) => fake.path_denies = None,
            (Op::Mount, World::Shut) => fake.mount = || errno(libc::EPERM),
            (Op::Mount, World::Broken) => fake.mount = || errno(libc::ENOENT),
            (Op::LaunchdBootstrap, World::Shut) => {
                fake.out = tool(5, "Bootstrap failed: 5: Input/output error");
            }
            (Op::LaunchdBootstrap, World::Broken) => fake.out = tool(1, "no"),
            (Op::LaunchdBootstrap, World::Open) => fake.out = tool(0, ""),
            _ => {}
        }
        fake
    }

    /// A log sink the tests read back.
    #[derive(Clone, Default)]
    struct Log(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl io::Write for Log {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().expect("log").extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Log {
        /// Runs `f` with this log as the subscriber (a `tracing` macro evaluates its
        /// fields only with one), and returns the text logged.
        fn during<T>(&self, f: impl FnOnce() -> T) -> (T, String) {
            let writer = self.clone();
            let subscriber = tracing_subscriber::fmt()
                .with_writer(move || writer.clone())
                .with_ansi(false)
                .finish();
            let out = tracing::subscriber::with_default(subscriber, f);
            io::Write::flush(&mut self.clone()).expect("flush");
            let text = String::from_utf8(self.0.lock().expect("log").clone()).expect("utf8");
            (out, text)
        }
    }

    fn errno(n: i32) -> io::Result<()> {
        Err(io::Error::from_raw_os_error(n))
    }

    fn row(name: &str) -> &'static Canary {
        CANARIES.iter().find(|c| c.name == name).expect("row")
    }

    fn ran(code: i32, stdout: &str) -> Ran {
        Ran {
            code: Some(code),
            stdout: stdout.to_owned(),
            took: Duration::from_millis(80),
        }
    }

    fn args(rest: &[&str]) -> Vec<OsString> {
        let mut all = vec![OsString::from("kbf")];
        all.extend(rest.iter().map(OsString::from));
        all
    }

    /// Catches a probe that reads `EPERM` or `EACCES` as anything but a refusal, or
    /// takes another error (a missing directory) or a success for one: only a refusal
    /// may pass a canary.
    #[test]
    fn only_eperm_and_eacces_are_refusals() {
        assert_eq!(
            classify(errno(libc::EPERM)),
            Seen::Refused(Refusal::Errno(libc::EPERM))
        );
        assert_eq!(
            classify(errno(libc::EACCES)),
            Seen::Refused(Refusal::Errno(libc::EACCES))
        );
        assert_eq!(classify(Ok(())), Seen::Allowed);
        assert_eq!(
            classify(errno(libc::ENOENT)),
            Seen::Failed(io::Error::from_raw_os_error(libc::ENOENT).to_string())
        );
        assert!(matches!(
            classify(Err(io::Error::other("no errno"))),
            Seen::Failed(_)
        ));
    }

    /// Catches a write canary that does not write outside the allowed paths, a link
    /// canary that does not link a root-owned file into the lease directory, an open
    /// door that is left open (the file stays), and a refused one that is "cleaned up".
    #[test]
    fn each_op_makes_its_call_and_removes_what_an_open_door_made() {
        let lease = Path::new("/lease");
        let open = Fake::new(|| Ok(()));
        assert_eq!(probe(&open, Op::WriteOutside, lease), Seen::Allowed);
        assert_eq!(probe(&open, Op::LinkOutside, lease), Seen::Allowed);
        let calls = open.calls.borrow();
        let pid = std::process::id();
        assert_eq!(
            *calls,
            [
                format!("create /private/tmp/.kbf-canary-{pid}"),
                format!("remove /private/tmp/.kbf-canary-{pid}"),
                "link /etc/hosts /lease/.kbf-canary-link".to_owned(),
                "remove /lease/.kbf-canary-link".to_owned(),
            ]
        );
        let shut = Fake::new(|| errno(libc::EPERM));
        assert_eq!(
            probe(&shut, Op::LinkOutside, lease),
            Seen::Refused(Refusal::Errno(libc::EPERM))
        );
        assert_eq!(shut.calls.borrow().len(), 1, "nothing to remove");
    }

    /// Catches a door canary that makes another call than its door (a different service
    /// name per process is needed, so that two probes do not meet), an open door left
    /// open (an agent still loaded, a mount still there, files left), a refused one
    /// "cleaned up" with calls that were never needed, and a job that is not a
    /// `Background` agent that would be kept alive.
    #[test]
    fn the_door_ops_make_their_calls_and_clean_up_after_an_open_door() {
        let lease = Path::new("/lease");
        let pid = std::process::id();
        let calls = |op, state| {
            let fake = world(op, state);
            let seen = probe(&fake, op, lease);
            (seen, fake.calls.into_inner())
        };
        let (seen, got) = calls(Op::MachRegister, World::Open);
        assert_eq!(seen, Seen::Allowed);
        assert_eq!(got, [format!("check_in com.kbf.canary.{pid}")]);

        let (seen, got) = calls(Op::LsOpen, World::Open);
        assert_eq!(
            (seen, got),
            (Seen::Allowed, vec!["sandbox_denies lsopen".to_owned()])
        );
        let (seen, _) = calls(Op::LsOpen, World::Shut);
        assert_eq!(seen, Seen::Refused(Refusal::Sandbox(1)));

        let (seen, got) = calls(Op::ReadProtected, World::Open);
        assert_eq!(
            (seen, got),
            (
                Seen::Allowed,
                vec![
                    "sandbox_denies_path file-read-data /private/var/protected/sfanalytics"
                        .to_owned()
                ]
            ),
            "the first question answered allowed ends the probe"
        );
        let (seen, got) = calls(Op::ReadProtected, World::Shut);
        assert_eq!(seen, Seen::Refused(Refusal::Sandbox(1)));
        assert_eq!(got.len(), 4, "{got:?}");
        for store in crate::network::PROTECTED_STORES {
            for operation in ["file-read-data", "file-write-data"] {
                assert!(
                    got.contains(&format!("sandbox_denies_path {operation} {store}")),
                    "{operation} {store}: {got:?}"
                );
            }
        }

        // A store that is read-denied and not write-denied (or the reverse) is an open door.
        for only in ["file-read-data", "file-write-data"] {
            let mut half = world(Op::ReadProtected, World::Open);
            half.path_denies = Some(if only == "file-read-data" {
                &["file-read-data"]
            } else {
                &["file-write-data"]
            });
            assert_eq!(
                probe(&half, Op::ReadProtected, lease),
                Seen::Allowed,
                "{only}"
            );
        }

        let mount = "/lease/.kbf-canary-mount";
        let (seen, got) = calls(Op::Mount, World::Open);
        assert_eq!(seen, Seen::Allowed);
        assert_eq!(
            got,
            [
                format!("make_dir {mount}"),
                format!("mount_devfs {mount}"),
                format!("unmount {mount}"),
                format!("remove {mount}"),
            ]
        );
        let mut stuck = world(Op::Mount, World::Open);
        stuck.unmount = || errno(libc::EBUSY);
        let lease = Path::new("/lease");
        let seen = doors::mount(&stuck, &lease.join(MOUNT_NAME));
        assert!(
            matches!(&seen, Seen::Failed(t) if t.contains("could not unmount")),
            "{seen:?}: a mount left behind is a failure, not an open door"
        );
        let (seen, got) = calls(Op::Mount, World::Shut);
        assert_eq!(seen, Seen::Refused(Refusal::Errno(libc::EPERM)));
        assert_eq!(
            got,
            [
                format!("make_dir {mount}"),
                format!("mount_devfs {mount}"),
                format!("remove {mount}"),
            ],
            "a shut door unmounts nothing"
        );

        let job = "/lease/.kbf-canary-job.plist";
        let label = format!("com.kbf.canary.{pid}");
        let (seen, got) = calls(Op::LaunchdBootstrap, World::Open);
        assert_eq!(seen, Seen::Allowed);
        assert_eq!(got.len(), 4, "{got:?}");
        let written = got[0]
            .strip_prefix(&format!("write {job} "))
            .expect(&got[0]);
        for want in [
            format!("<string>{label}</string>"),
            "<key>ProcessType</key><string>Background</string>".to_owned(),
            "<key>KeepAlive</key><true/>".to_owned(),
            "<?xml".to_owned(),
        ] {
            assert!(written.contains(&want), "{want} not in {written}");
        }
        assert_eq!(
            got[1..],
            [
                format!("run /bin/launchctl bootstrap user/501 {job}"),
                format!("run /bin/launchctl bootout user/501/{label}"),
                format!("remove {job}"),
            ]
        );
        let (seen, got) = calls(Op::LaunchdBootstrap, World::Shut);
        assert_eq!(seen, Seen::Refused(Refusal::Bootstrap(5)));
        assert_eq!(got.len(), 3, "a shut door boots out nothing: {got:?}");
        // A tool killed after it may have loaded the job, and a failure of another
        // kind, are not refusals: the job is booted out all the same.
        let killed = {
            let mut f = world(Op::LaunchdBootstrap, World::Shut);
            f.out = Out {
                code: None,
                text: "\n(killed: still running after the tool's time)".to_owned(),
            };
            f
        };
        let other = {
            let mut f = world(Op::LaunchdBootstrap, World::Shut);
            f.out = Out {
                code: Some(1),
                text: "no".to_owned(),
            };
            f
        };
        for fake in [other, killed] {
            let seen = probe(&fake, Op::LaunchdBootstrap, lease);
            assert!(matches!(seen, Seen::Failed(_)), "{seen:?}");
            let got = fake.calls.into_inner();
            assert!(
                got.contains(&format!("run /bin/launchctl bootout user/501/{label}")),
                "a failed bootstrap still boots its job out: {got:?}"
            );
            assert_eq!(got.last(), Some(&format!("remove {job}")));
        }
    }

    /// Catches a door canary that reads a failure that is not a refusal as one (a tool
    /// that is missing, a file that cannot be written, a service that is unknown, an
    /// error code of another door, a mount point that cannot be made), which would let a
    /// slot pass on a canary that proved nothing.
    #[test]
    fn a_door_that_fails_otherwise_proves_nothing() {
        let lease = Path::new("/lease");
        for op in [
            Op::MachRegister,
            Op::LsOpen,
            Op::ReadProtected,
            Op::Mount,
            Op::LaunchdBootstrap,
        ] {
            let fake = world(op, World::Broken);
            let seen = probe(&fake, op, lease);
            assert!(matches!(seen, Seen::Failed(_)), "{op:?}: {seen:?}");
        }
        // A real refusal text with a code that is not the door's.
        let mut fake = world(Op::LaunchdBootstrap, World::Shut);
        fake.out = Out {
            code: Some(1),
            text: "Bootstrap failed: 113: Could not find service".to_owned(),
        };
        let seen = probe(&fake, Op::LaunchdBootstrap, lease);
        assert!(matches!(seen, Seen::Failed(_)), "{seen:?}");
        // The wrong errno for a mount, one that is no errno, and the wrong Mach error.
        let mut fake = world(Op::Mount, World::Shut);
        fake.mount = || errno(libc::ENODEV);
        assert!(matches!(probe(&fake, Op::Mount, lease), Seen::Failed(_)));
        fake.mount = || Err(io::Error::other("no errno"));
        assert!(matches!(probe(&fake, Op::Mount, lease), Seen::Failed(_)));
        let mut fake = world(Op::MachRegister, World::Shut);
        fake.kern = 1102;
        // Not "NOT refused: the call succeeded", which is what a check-in answering 0
        // reads as: 1102 is a failure that names the answer.
        let seen = probe(&fake, Op::MachRegister, lease);
        assert!(
            matches!(&seen, Seen::Failed(t) if t.contains("not a refusal of this door: kern=1102")),
            "{seen:?}"
        );
        // A tool that cannot be started, and a platform without the call.
        let mut fake = world(Op::LaunchdBootstrap, World::Shut);
        fake.no_tools = true;
        let seen = probe(&fake, Op::LaunchdBootstrap, lease);
        assert!(matches!(seen, Seen::Failed(_)), "{seen:?}");
        #[cfg(not(target_os = "macos"))]
        for op in [Op::MachRegister, Op::LsOpen, Op::ReadProtected] {
            let seen = probe(&RealFs, op, lease);
            assert!(matches!(seen, Seen::Failed(_)), "{op:?}: {seen:?}");
        }
        // Failures before the door is tried.
        let writes = Fake::new(|| errno(libc::EROFS));
        for op in [Op::Mount, Op::LaunchdBootstrap] {
            let seen = probe(&writes, op, lease);
            assert!(matches!(seen, Seen::Failed(_)), "{op:?}: {seen:?}");
        }
    }

    /// Catches the real calls that do not write a new file only, make a directory, remove
    /// a directory as well as a file, run a program or report the uid, and the Mach call
    /// that runs off a Mac.
    #[test]
    fn the_real_door_calls_write_make_run_and_report() {
        let scratch = crate::unit_dir("canary-real-doors");
        let dir = scratch.path();
        let file = dir.join("f");
        RealFs.write(&file, "text").expect("write");
        assert_eq!(std::fs::read_to_string(&file).expect("read"), "text");
        assert!(RealFs.write(&file, "again").is_err(), "a new file only");
        let sub = dir.join("d");
        RealFs.make_dir(&sub).expect("dir");
        assert!(RealFs.make_dir(&sub).is_err());
        RealFs.remove(&sub);
        RealFs.remove(&file);
        assert_eq!([sub.exists(), file.exists()], [false, false]);
        // SAFETY: `getuid` takes no arguments and cannot fail.
        assert_eq!(RealFs.uid(), unsafe { libc::getuid() });
        let out = RealFs
            .run("/bin/ls", &["/nonexistent/kbf-canary".into()])
            .expect("run");
        assert!(out.code.is_some_and(|c| c != 0), "{out:?}");
        assert!(out.text.contains("kbf-canary"), "{out:?}");
        let ok = RealFs.run("/bin/ls", &["/".into()]).expect("run");
        assert_eq!(ok.code, Some(0));
        assert!(RealFs.run("/nonexistent/tool", &[]).is_err());
        #[cfg(not(target_os = "macos"))]
        for off_a_mac in [
            RealFs.sandbox_denies("lsopen").map(drop),
            RealFs.sandbox_denies_path("file-read-data", dir).map(drop),
            RealFs.mount_devfs(dir),
            RealFs.unmount(dir),
        ] {
            assert_eq!(
                off_a_mac.expect_err("off a Mac").kind(),
                io::ErrorKind::Unsupported
            );
        }
        #[cfg(not(target_os = "macos"))]
        assert_eq!(
            RealFs.check_in("x").expect_err("off a Mac").kind(),
            io::ErrorKind::Unsupported
        );
    }

    /// Catches real calls that do not create, link and remove (a create that
    /// overwrites would let a stale file hide an open door).
    #[test]
    fn the_real_calls_create_link_and_remove() {
        let scratch = crate::unit_dir("canary-real-calls");
        let dir = scratch.path();
        let file = dir.join("f");
        RealFs.create(&file).expect("create");
        assert!(
            matches!(classify(RealFs.create(&file)), Seen::Failed(_)),
            "creating twice is not a refusal"
        );
        let linked = dir.join("l");
        RealFs.link(&file, &linked).expect("link");
        RealFs.remove(&linked);
        RealFs.remove(&file);
        RealFs.remove(&file);
        assert_eq!([linked.exists(), file.exists()], [false, false]);
    }

    /// Catches a probe mode that answers a process that did not ask for it, that runs a
    /// canary it does not know, or that prints a line the daemon's [`judge`] cannot
    /// read back (the line and the exit code are the whole protocol).
    #[test]
    fn the_probe_mode_prints_what_judge_reads() {
        let mut out = Vec::new();
        assert_eq!(dispatch(&args(&[]), &RealFs, &mut out), None);
        assert_eq!(dispatch(&args(&["other"]), &RealFs, &mut out), None);
        assert!(out.is_empty());
        for bad in [&[ARG][..], &[ARG, "nope", "/l"], &[ARG, "write-outside"]] {
            assert_eq!(
                dispatch(&args(bad), &RealFs, &mut out),
                Some(USAGE),
                "{bad:?}"
            );
        }
        for canary in CANARIES {
            for (state, code) in [
                (World::Shut, REFUSED),
                (World::Open, ALLOWED),
                (World::Broken, FAILED),
            ] {
                let mut out = Vec::new();
                let fake = world(canary.op, state);
                let got = dispatch(&args(&[ARG, canary.name, "/lease"]), &fake, &mut out);
                assert_eq!(got, Some(code), "{} {state:?}", canary.name);
                let text = String::from_utf8(out).expect("utf8");
                let verdict = judge(canary, &ran(code, &text));
                match code {
                    REFUSED => assert_eq!(verdict, Verdict::Refused(shut_with(canary)), "{text}"),
                    ALLOWED => assert_eq!(verdict, Verdict::Allowed, "{text}"),
                    _ => assert!(matches!(verdict, Verdict::CannotRun(_)), "{verdict:?}"),
                }
            }
        }
    }

    /// The refusal the [`World::Shut`] of `canary`'s op gives.
    fn shut_with(canary: &Canary) -> Refusal {
        match canary.op {
            Op::WriteOutside | Op::LinkOutside => Refusal::Errno(libc::EACCES),
            other => other.refusals()[0],
        }
    }

    /// The line a probe prints for `canary` refused as its op is.
    fn refusal_line(canary: &Canary) -> String {
        format!("canary {} refused {}", canary.name, shut_with(canary))
    }

    /// Catches a judge that accepts a refusal another door gives (an errno on the Mach
    /// canary, a kern_return on the write canary, a launchd error on the open canary),
    /// or an errno that is not the door's: each op is refused in exactly the forms it
    /// lists, and no op accepts another's.
    #[test]
    fn judge_accepts_each_op_only_its_own_refusal() {
        let all = [
            Refusal::Errno(libc::EPERM),
            Refusal::Errno(libc::EACCES),
            Refusal::Errno(libc::ENOENT),
            Refusal::Kern(1100),
            Refusal::Kern(1102),
            Refusal::Bootstrap(5),
            Refusal::Bootstrap(113),
            Refusal::Sandbox(1),
            Refusal::Sandbox(0),
        ];
        for canary in CANARIES {
            let mut accepted = 0;
            for refusal in all {
                let line = format!("canary {} refused {refusal}", canary.name);
                let verdict = judge(canary, &ran(REFUSED, &line));
                if canary.op.refusals().contains(&refusal) {
                    accepted += 1;
                    assert_eq!(verdict, Verdict::Refused(refusal), "{line}");
                } else {
                    assert!(matches!(verdict, Verdict::CannotRun(_)), "{line}");
                }
            }
            assert_eq!(accepted, canary.op.refusals().len(), "{}", canary.name);
        }
        let exact = |name: &str| row(name).op.refusals();
        assert_eq!(exact("mach-register"), [Refusal::Kern(1100)]);
        assert_eq!(exact("lsopen"), [Refusal::Sandbox(1)]);
        assert_eq!(exact("read-protected"), [Refusal::Sandbox(1)]);
        assert_eq!(exact("file-mount"), [Refusal::Errno(libc::EPERM)]);
        assert_eq!(exact("launchd-bootstrap"), [Refusal::Bootstrap(5)]);
    }

    /// Catches a refusal that does not survive the line the probe prints (a form that
    /// prints but is not read back, or one that reads back as another).
    #[test]
    fn a_refusal_is_read_back_as_it_was_printed() {
        for refusal in [
            Refusal::Errno(1),
            Refusal::Kern(1100),
            Refusal::Bootstrap(5),
            Refusal::Sandbox(1),
        ] {
            assert_eq!(Refusal::parse(&refusal.to_string()), Some(refusal));
        }
        for bad in [
            "",
            "errno",
            "errno=",
            "errno=x",
            "kern=1100x",
            "other=1",
            "=1",
            "errno=1=1",
        ] {
            assert_eq!(Refusal::parse(bad), None, "{bad:?}");
        }
        assert_eq!(Refusal::Kern(1100).to_string(), "kern=1100");
        assert_eq!(Refusal::Sandbox(1).to_string(), "sandbox=1");
    }

    /// Catches a judge that passes a canary on the exit code alone (a probe that died
    /// early exits 0 on some path), on a line for another canary, on a refusal with an
    /// errno that is not one, or on a signal; and one that reports an open door as
    /// anything but open.
    #[test]
    fn judge_passes_only_the_exact_refusal() {
        let c = row("write-outside");
        let good = "canary write-outside refused errno=1\n";
        assert_eq!(
            judge(c, &ran(REFUSED, good)),
            Verdict::Refused(Refusal::Errno(1))
        );
        for bad in [
            ran(REFUSED, ""),
            ran(REFUSED, "canary link-outside refused errno=1"),
            ran(REFUSED, "canary write-outside refused errno=2"),
            ran(REFUSED, "canary write-outside refused errno=x"),
            ran(FAILED, good),
            ran(127, ""),
            Ran {
                code: None,
                stdout: good.to_owned(),
                took: Duration::ZERO,
            },
        ] {
            assert!(matches!(judge(c, &bad), Verdict::CannotRun(_)), "{bad:?}");
        }
        assert_eq!(
            judge(c, &ran(ALLOWED, "canary write-outside allowed")),
            Verdict::Allowed
        );
        assert!(matches!(
            judge(c, &ran(ALLOWED, "something else")),
            Verdict::CannotRun(_)
        ));
    }

    /// Catches a check that stops at the first failure (a second broken door is never
    /// seen, so a profile that breaks only the later canary cannot be shown red), that
    /// passes when one canary is allowed (the "not offered" branch) or cannot run (the
    /// "cannot run" branch), or that does not log the canary, its deny and the result.
    #[test]
    fn check_runs_every_canary_and_reports_every_failure() {
        let refused = |c: &Canary| Ok(ran(REFUSED, &refusal_line(c)));
        let (report, log) = Log::default().during(|| check(CANARIES, &mut |c| refused(c)));
        assert!(report.all_refused());
        assert_eq!(report.passed.len(), CANARIES.len());
        assert_eq!(report.passed[0].name, "write-outside");
        assert_eq!(report.passed[0].took, Duration::from_millis(80));
        assert!(log.contains("profile canary refused"), "{log}");
        assert!(log.contains("link-outside"), "{log}");

        // Only one canary is broken, in turn: each is seen alone, the other refused.
        for broken in CANARIES {
            let (report, log) = Log::default().during(|| {
                check(CANARIES, &mut |c| {
                    if c.name == broken.name {
                        Ok(ran(ALLOWED, &format!("canary {} allowed", c.name)))
                    } else {
                        refused(c)
                    }
                })
            });
            assert!(!report.all_refused());
            assert_eq!(report.failed.len(), 1, "{}", report.failures());
            assert_eq!(report.passed.len(), CANARIES.len() - 1);
            let failed = &report.failed[0];
            assert_eq!(
                (failed.name, &failed.verdict),
                (broken.name, &Verdict::Allowed)
            );
            assert_eq!(
                report.failures(),
                format!(
                    "canary {} ({}): NOT refused: the call succeeded",
                    broken.name, broken.deny
                )
            );
            for want in ["ERROR", "profile canary failed", broken.name, "NOT refused"] {
                assert!(log.contains(want), "{want} not in {log}");
            }
        }

        let mut seen = 0;
        let (report, log) = Log::default().during(|| {
            check(CANARIES, &mut |_| {
                seen += 1;
                Err(io::Error::new(io::ErrorKind::TimedOut, "timed out"))
            })
        });
        assert_eq!(seen, CANARIES.len(), "every canary runs");
        assert_eq!(report.failed.len(), CANARIES.len());
        assert_eq!(
            report.failed[0].verdict,
            Verdict::CannotRun("timed out".to_owned())
        );
        assert!(log.contains("could not run: timed out"), "{log}");
        assert_eq!(check(&[], &mut |_| unreachable!()), Report::default());
    }

    /// A slot that records when it is released (dropped), unless it was set aside.
    struct TestSlot {
        id: u32,
        events: Events,
        asided: bool,
    }

    type Events = std::sync::Arc<std::sync::Mutex<Vec<String>>>;

    impl Drop for TestSlot {
        fn drop(&mut self) {
            if !self.asided {
                let event = format!("released {}", self.id);
                self.events.lock().expect("events").push(event);
            }
        }
    }

    /// Where the offer fails, for the slot taken first.
    #[derive(Clone, Copy, PartialEq)]
    enum Fail {
        Never,
        /// A kill while the lease is prepared, with the slot held.
        Prepare,
        /// The hand-over of the lease directory fails.
        HandOver,
        /// A canary could not be started.
        Canary,
        /// The lease directory cannot be taken back from a slot that failed its canaries.
        Reclaim,
    }

    /// Slots from a queue (taken from the back); the probe passes the slots in `good`.
    struct TestOffer<'a> {
        events: Events,
        queue: Vec<u32>,
        good: Vec<u32>,
        fail: Fail,
        /// The caller's cell, like the runtime's scope: the offer owns no slot.
        cell: &'a mut Option<TestSlot>,
    }

    impl<'a> TestOffer<'a> {
        fn new(
            cell: &'a mut Option<TestSlot>,
            events: &Events,
            (queue, good): (&[u32], &[u32]),
            fail: Fail,
        ) -> Self {
            Self {
                events: Events::clone(events),
                queue: queue.to_vec(),
                good: good.to_vec(),
                fail,
                cell,
            }
        }

        fn push(&self, event: &str) {
            self.events.lock().expect("events").push(event.to_owned());
        }
    }

    impl Offer for TestOffer<'_> {
        type Err = String;

        async fn next(&mut self) -> Result<(), String> {
            let Some(id) = self.queue.pop() else {
                return Err("killed while waiting".to_owned());
            };
            *self.cell = Some(TestSlot {
                id,
                events: Events::clone(&self.events),
                asided: false,
            });
            if self.fail == Fail::Prepare {
                return Err("killed during prepare".to_owned());
            }
            Ok(())
        }

        async fn probe(&mut self) -> Result<Probe, String> {
            let id = self.cell.as_ref().expect("held").id;
            match self.fail {
                Fail::HandOver => return Err("hand over failed".to_owned()),
                Fail::Canary => return Err("canary could not start".to_owned()),
                Fail::Never | Fail::Prepare | Fail::Reclaim => {}
            }
            Ok(if self.good.contains(&id) {
                Probe::Pass
            } else {
                Probe::Aside(format!("slot {id} bad"))
            })
        }

        async fn aside(&mut self, why: &str) -> Result<(), String> {
            if self.fail == Fail::Reclaim {
                return Err("reclaim failed".to_owned());
            }
            self.push("reclaim the lease directory");
            let mut slot = self.cell.take().expect("held");
            slot.asided = true;
            self.push(&format!("aside {}: {why}", slot.id));
            Ok(())
        }
    }

    /// The lease as `run` does it: acquire into the scope's cell, then remove the lease
    /// directory, then release the slot. What happened, in order.
    fn lease(slots: (&[u32], &[u32]), fail: Fail) -> (Result<(), String>, Vec<String>, bool) {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime");
        let events = Events::default();
        let mut cell = None;
        let result = rt.block_on(first_passing(&mut TestOffer::new(
            &mut cell, &events, slots, fail,
        )));
        events
            .lock()
            .expect("events")
            .push("lease directory removed".to_owned());
        let held = cell.is_some();
        drop(cell);
        let events = events.lock().expect("events").clone();
        (result, events, held)
    }

    /// Catches a lease whose slot is freed before its directory is removed (another
    /// lease could then take the uid while the failed lease's half-written inputs, owned
    /// by that uid, are still there): on a kill during prepare, a failed hand-over and a
    /// canary that could not start, the slot stays with the caller, which removes the
    /// directory and only then releases it; and after a kill while waiting for the next
    /// slot, with one set aside, no slot is held and the directory is removed. Catches
    /// the offer releasing the slot itself on an error (the slot is released
    /// before the directory event).
    #[test]
    fn on_every_failure_the_directory_goes_before_the_slot_is_released() {
        for fail in [Fail::Prepare, Fail::HandOver, Fail::Canary] {
            let (result, events, held) = lease((&[1], &[1]), fail);
            assert!(result.is_err());
            assert!(held, "the caller holds the slot");
            assert_eq!(events, ["lease directory removed", "released 1"]);
        }
        // The directory cannot be taken back from a slot that failed its canaries: it is
        // not set aside, the lease fails with the slot held, and the directory goes first.
        let (result, events, held) = lease((&[1], &[]), Fail::Reclaim);
        assert_eq!(result, Err("reclaim failed".to_owned()));
        assert!(held);
        assert_eq!(events, ["lease directory removed", "released 1"]);
        // Slot 1 fails its canaries and is set aside; the wait for the next ends in a kill.
        let (result, events, held) = lease((&[1], &[]), Fail::Never);
        assert_eq!(result, Err("killed while waiting".to_owned()));
        assert!(!held);
        assert_eq!(
            events,
            [
                "reclaim the lease directory",
                "aside 1: slot 1 bad",
                "lease directory removed"
            ]
        );
    }

    /// Catches a lease that fails, or runs on a slot that failed its canaries, when a
    /// later slot would pass: the bad slots are set aside, each after the directory is
    /// taken back from it, and the lease runs on the good one, which the caller holds.
    #[test]
    fn a_failed_canary_sets_the_slot_aside_and_the_lease_takes_the_next() {
        let (result, events, held) = lease((&[3, 2, 1], &[3]), Fail::Never);
        assert_eq!(result, Ok(()));
        assert!(held);
        assert_eq!(
            events,
            [
                "reclaim the lease directory",
                "aside 1: slot 1 bad",
                "reclaim the lease directory",
                "aside 2: slot 2 bad",
                "lease directory removed",
                "released 3"
            ]
        );
    }

    /// Catches blocking work that goes on after the future waiting for it was dropped (the
    /// runtime shutting down, or a panic unwinding through the poll): the drop sets the
    /// cancel flag, so the work records no step after the one in flight. A run that
    /// finishes does not leave the flag set (the canaries reuse it after the hand-over).
    /// Catches the drop guard being removed.
    #[test]
    fn a_dropped_wait_stops_the_blocking_work_too() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::{Arc, Mutex};
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("runtime");
        let steps = Arc::new(Mutex::new(0_u32));
        let cancel = Arc::new(AtomicBool::new(false));
        let (work_steps, work_cancel) = (Arc::clone(&steps), Arc::clone(&cancel));
        let work = async move {
            let _ = tokio::task::spawn_blocking(move || {
                while !work_cancel.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(20));
                    *work_steps.lock().expect("steps") += 1;
                }
            })
            .await;
        };
        // The wait is dropped by the timeout, as a shutdown or a panic would drop it.
        let ended = rt.block_on(async {
            tokio::time::timeout(
                Duration::from_millis(70),
                until_killed(work, &cancel, std::future::pending::<Option<()>>()),
            )
            .await
        });
        assert!(ended.is_err(), "the wait was not dropped");
        assert!(
            cancel.load(Ordering::SeqCst),
            "the drop did not set the flag"
        );
        let at_drop = *steps.lock().expect("steps");
        std::thread::sleep(Duration::from_millis(150));
        assert!(
            *steps.lock().expect("steps") <= at_drop + 1,
            "work went on after the wait was dropped"
        );
        let finished = AtomicBool::new(false);
        let done = rt.block_on(until_killed(
            async { 1 },
            &finished,
            std::future::pending::<Option<()>>(),
        ));
        assert_eq!(done, Raced::Done(1));
        assert!(
            !finished.load(Ordering::SeqCst),
            "a finished run left the flag set"
        );
    }

    /// Catches a canary loop that starts another canary after it was told to stop (a
    /// canary launched as the slot's user after the slot was released): with the flag set
    /// after the first, the second is not run and the report holds the first.
    #[test]
    fn no_canary_starts_once_cancelled() {
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let mut runs = 0;
        let report = check_until(
            CANARIES,
            &|| cancel.load(std::sync::atomic::Ordering::SeqCst),
            &mut |c| {
                runs += 1;
                cancel.store(true, std::sync::atomic::Ordering::SeqCst);
                Ok(ran(REFUSED, &refusal_line(c)))
            },
        );
        assert_eq!(runs, 1);
        assert_eq!(report.passed.len(), 1);
        assert!(report.failed.is_empty());
    }

    /// Catches a kill that is answered while blocking work of the slot goes on (a canary
    /// launched as the slot's user, or an owner change, after the lease was reported
    /// killed and its slot released): the work records a step every 20 ms and stops at
    /// its next step once the flag is set; after `until_killed` returns, no step is
    /// recorded, and fewer than all of them ran. A run that finishes first is `Done`.
    /// Catches `until_killed` returning on the kill without awaiting the work.
    #[test]
    fn a_kill_waits_for_the_blocking_work_to_stop() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::{Arc, Mutex};
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("runtime");
        let steps = Arc::new(Mutex::new(0_u32));
        let cancel = Arc::new(AtomicBool::new(false));
        let (work_steps, work_cancel) = (Arc::clone(&steps), Arc::clone(&cancel));
        let work = async move {
            tokio::task::spawn_blocking(move || {
                while !work_cancel.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(20));
                    *work_steps.lock().expect("steps") += 1;
                }
            })
            .await
            .expect("work")
        };
        let killed = async {
            tokio::time::sleep(Duration::from_millis(70)).await;
            Some("kill")
        };
        let raced = rt.block_on(until_killed(work, &cancel, killed));
        assert_eq!(raced, Raced::Killed("kill"));
        let at_ack = *steps.lock().expect("steps");
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(
            *steps.lock().expect("steps"),
            at_ack,
            "work after the kill was answered"
        );
        assert!(at_ack < 50, "the work was not stopped");

        let cancel = AtomicBool::new(false);
        let done = rt.block_on(until_killed(
            async { 7 },
            &cancel,
            std::future::pending::<Option<()>>(),
        ));
        assert_eq!(done, Raced::Done(7));
        assert!(!cancel.load(Ordering::SeqCst));
    }

    /// Catches a removal of the link canary's leftovers that follows the link, or misses
    /// it: a hard link to a file outside is removed from the lease directory and the
    /// file outside is untouched.
    #[test]
    fn the_daemon_removes_the_link_a_canary_left() {
        let scratch = crate::unit_dir("canary-leftovers");
        let outside = scratch.path().join("outside");
        std::fs::write(&outside, b"x").expect("outside");
        let lease = scratch.path().join("lease");
        std::fs::create_dir(&lease).expect("lease");
        let link = lease.join(LINK_NAME);
        std::fs::hard_link(&outside, &link).expect("link");
        remove_leftovers(&RealFs, &lease);
        assert!(!link.exists(), "the link stayed");
        assert!(outside.exists(), "the file outside was removed");
        remove_leftovers(&RealFs, &lease);
    }

    /// Catches a failed canary that does not make the slot dirty in the book (it would
    /// stay offered and counted), a lease that fails or runs on that slot instead of the
    /// next one, a recovery that offers the slot again without running its canaries at
    /// the next hand-out, and a slot that stays dirty after its canaries pass. Changes
    /// seen to turn it red: `Book::set_aside` giving the permit back instead of making
    /// the slot dirty (the dirty-set and count assertions); `first_passing` returning a
    /// slot whose probe said aside (the lease runs on slot 0); `first_passing` skipping
    /// the probe (slot 0 is handed out without its canaries).
    #[test]
    fn a_failed_canary_makes_the_slot_dirty_and_recovery_offers_it_with_its_canaries_run_again() {
        use std::collections::BTreeSet;
        use std::sync::{Arc, Mutex};

        use super::super::book::{Backoff, Book, Cleaner, Slotted};
        use super::super::clean::Outcome;

        struct Stub(u32);
        impl Slotted for Stub {
            fn uid(&self) -> u32 {
                700 + self.0
            }
        }

        type Held = (usize, tokio::sync::OwnedSemaphorePermit);

        /// The book's slots offered to one lease; the canaries of `failing` fail.
        struct BookOffer<'a> {
            book: Arc<Book<Stub>>,
            failing: Arc<Mutex<BTreeSet<usize>>>,
            probed: &'a mut Vec<usize>,
            cell: &'a mut Option<Held>,
        }

        impl Offer for BookOffer<'_> {
            type Err = String;

            async fn next(&mut self) -> Result<(), String> {
                *self.cell = Some(self.book.take().await?);
                Ok(())
            }

            async fn probe(&mut self) -> Result<Probe, String> {
                let index = self.cell.as_ref().expect("held").0;
                self.probed.push(index);
                let bad = self.failing.lock().expect("failing").contains(&index);
                Ok(if bad {
                    Probe::Aside(
                        "canary write-outside ((deny file-write*)): NOT refused".to_owned(),
                    )
                } else {
                    Probe::Pass
                })
            }

            async fn aside(&mut self, why: &str) -> Result<(), String> {
                let (index, permit) = self.cell.take().expect("held");
                self.book.set_aside(index, permit, why);
                Ok(())
            }
        }

        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime");
        let cleaner: Cleaner<Stub> = Arc::new(|_: &Stub| Outcome::Clean);
        let never = Backoff {
            first: Duration::from_secs(3600),
            max: Duration::from_secs(3600),
        };
        let book = Book::start(vec![Stub(0), Stub(1)], cleaner, never);
        let failing = Arc::new(Mutex::new(BTreeSet::from([0_usize])));
        let mut probed = Vec::new();
        let mut hold = None;
        let round = |probed: &mut Vec<usize>, hold: &mut Option<Held>| {
            let mut offer = BookOffer {
                book: Arc::clone(&book),
                failing: Arc::clone(&failing),
                probed,
                cell: hold,
            };
            rt.block_on(first_passing(&mut offer)).expect("a slot");
        };

        // Slot 0 fails its canaries: dirty, uncounted; the lease runs on slot 1.
        round(&mut probed, &mut hold);
        let (index, permit) = hold.take().expect("held");
        assert_eq!(index, 1, "the lease takes the next slot");
        assert_eq!(probed, [0, 1]);
        assert_eq!(book.dirty(), BTreeSet::from([0]));
        assert_eq!(book.count(), 1, "a dirty slot is not counted");
        book.held(index, permit).give();

        // The recovery pass cleans slot 0 and frees it; its canaries run again at the next
        // hand-out and fail again: dirty again, the lease runs on slot 1.
        assert_eq!(book.retry(), 1);
        assert_eq!(book.count(), 2);
        probed.clear();
        round(&mut probed, &mut hold);
        let (index, permit) = hold.take().expect("held");
        assert_eq!((index, probed.clone()), (1, vec![0, 1]));
        assert_eq!(book.dirty(), BTreeSet::from([0]));
        book.held(index, permit).give();

        // Its canaries pass now: recovered, probed once, handed out.
        failing.lock().expect("failing").clear();
        assert_eq!(book.retry(), 1);
        probed.clear();
        round(&mut probed, &mut hold);
        let (index, permit) = hold.take().expect("held");
        assert_eq!((index, probed.clone()), (0, vec![0]));
        assert!(book.dirty().is_empty());
        book.held(index, permit).give();
    }
}
