//! Run records: which process group each action leads, written where the next daemon on
//! the node finds it, before the action's program runs.
//!
//! An action leads a process group of its own, so when the daemon is killed (SIGKILL,
//! the kernel's OOM killer, a panic that aborts) nothing ends the action: neither a
//! signal to the daemon nor one to the daemon's group reaches it. The next daemon's
//! start-up sweep ([`crate::sweep`]) reads these records and ends what they name
//! before the daemon says `Hello`.
//!
//! A record is the file `<scratch>/runs/lease-<term>-<seq>` holding one line: the
//! leader's pid (which is the group id) and its start time ([`Record`]). It is kept
//! outside the lease directory, which the action may write (the one place the sandbox
//! lets it write on macOS).
//!
//! [`Gate::spawn`] writes the record before the program runs. The child, between fork
//! and exec, sends its pid to the daemon and waits; the daemon reads the child's start
//! time, writes the record and only then sends the go byte. A daemon that dies before
//! that leaves a child that never runs the program: it reads end-of-file, or sees its
//! parent change, and exits. The child then execs the program itself ([`Exec`]).

use std::collections::BTreeMap;
use std::ffi::{CString, OsStr};
use std::io::{self, Read as _, Write as _};
use std::os::fd::AsRawFd as _;
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};

use tokio::process::{Child, Command};

use crate::procs;

/// The directory under the scratch root that holds the run records.
pub const RUNS: &str = "runs";

/// How long the child waits for the go byte between two checks that its parent, the
/// daemon, is still alive, in milliseconds.
const PARENT_CHECK_MS: libc::c_int = 100;

/// One running action's process group: its leader's pid, which is the group id, and the
/// leader's start time, which with the pid names the leader for its whole life.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Record {
    pub pgid: i32,
    pub start: u64,
}

impl Record {
    /// The record's file contents.
    #[must_use]
    pub fn line(self) -> String {
        format!("{} {}\n", self.pgid, self.start)
    }

    /// Reads a record's file contents; `None` unless it is one whole line of two
    /// numbers, the group id above 1 (so never every process of the user, nor init's
    /// group).
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let (pgid, start) = text.strip_suffix('\n')?.split_once(' ')?;
        let pgid: i32 = pgid.parse().ok().filter(|&pgid| pgid > 1)?;
        Some(Self {
            pgid,
            start: start.parse().ok()?,
        })
    }
}

/// The record of the run whose lease directory is named `lease` (`lease-<term>-<seq>`).
#[must_use]
pub fn path(scratch: &Path, lease: &str) -> PathBuf {
    scratch.join(RUNS).join(lease)
}

/// What the child execs once the gate opens, made before the fork: the child may not
/// allocate. The child calls `execve(2)` itself rather than return to the standard
/// library, which would call `execvp(3)` once a pre-exec step is set: that runs a
/// program the kernel refuses as no executable (ENOEXEC) through `/bin/sh` instead of
/// failing, which the spawn this replaces did not do.
#[derive(Debug)]
pub struct Exec {
    program: CString,
    /// Own the strings the pointer arrays point into.
    _strings: Vec<CString>,
    /// `argv` and `envp`, each ending in a null pointer.
    argv: Vec<*const libc::c_char>,
    envp: Vec<*const libc::c_char>,
}

// SAFETY: the pointers point into the `CString`s in `_strings` (each its own heap
// buffer, which a move of the vector does not move), and nothing writes through them.
unsafe impl Send for Exec {}
// SAFETY: as above; shared use only reads.
unsafe impl Sync for Exec {}

impl Exec {
    /// `program` run with `args` after it (argv[0] is `program`) and exactly `env`, in
    /// which a later entry wins over an earlier one of the same name.
    ///
    /// # Errors
    /// A string holds a NUL byte, which no argument or variable can.
    pub fn new<'a>(
        program: &Path,
        args: impl IntoIterator<Item = &'a OsStr>,
        env: impl IntoIterator<Item = (&'a OsStr, &'a OsStr)>,
    ) -> io::Result<Self> {
        let c = |bytes: &[u8]| CString::new(bytes).map_err(io::Error::other);
        let program = c(program.as_os_str().as_bytes())?;
        let mut argv = vec![program.clone()];
        for arg in args {
            argv.push(c(arg.as_bytes())?);
        }
        let env: BTreeMap<&OsStr, &OsStr> = env.into_iter().collect();
        let mut envp = Vec::new();
        for (name, value) in env {
            envp.push(c(&[name.as_bytes(), b"=", value.as_bytes()].concat())?);
        }
        let pointers = |strings: &[CString]| {
            let mut pointers: Vec<*const libc::c_char> =
                strings.iter().map(|s| s.as_ptr()).collect();
            pointers.push(std::ptr::null());
            pointers
        };
        let (argv_ptrs, envp_ptrs) = (pointers(&argv), pointers(&envp));
        argv.extend(envp);
        Ok(Self {
            program,
            _strings: argv,
            argv: argv_ptrs,
            envp: envp_ptrs,
        })
    }
}

/// The descriptors the child of one spawn attempt uses, set by the parent before each
/// attempt and read by the child after the fork (atomics: the child may only do what
/// is async-signal-safe), and what it execs.
#[derive(Debug)]
struct Gated {
    /// Where the child writes its pid.
    pid: AtomicI32,
    /// Where the child reads the go byte.
    go: AtomicI32,
    /// The parent's end of the go pipe, which the child closes, so that the parent's
    /// death shows as end-of-file.
    go_parent: AtomicI32,
    /// The daemon's pid: the child's parent while it waits.
    parent: i32,
    exec: Exec,
}

/// A command whose program runs only once its run record is written.
#[derive(Debug)]
pub struct Gate {
    child: Arc<Gated>,
}

impl Gate {
    /// Makes every spawn of `command` wait, before exec, for [`Gate::spawn`] to write
    /// its record, then exec `exec` (the command's own program and arguments are not
    /// used). A spawn that bypasses the gate fails.
    pub fn install(command: &mut Command, exec: Exec) -> Self {
        let child = Arc::new(Gated {
            pid: AtomicI32::new(-1),
            go: AtomicI32::new(-1),
            go_parent: AtomicI32::new(-1),
            parent: i32::try_from(std::process::id()).unwrap_or(i32::MAX),
            exec,
        });
        // SAFETY: the closure runs in the child between fork and exec; `in_child` calls
        // only async-signal-safe functions (close, getpid, write, poll, read, getppid,
        // execve) on descriptors and strings the parent made before the fork, and
        // allocates nothing.
        unsafe {
            command.pre_exec(child_steps(Arc::clone(&child)));
        }
        Self { child }
    }

    /// Spawns `command` once, writing `record` (the run's record file) after the fork
    /// and before the program runs. Returns the child and its pid.
    ///
    /// # Errors
    /// The spawn failed, or the record could not be written (then the program never
    /// ran). The record is removed on every error.
    pub fn spawn(&self, command: &mut Command, record: &Path) -> io::Result<(Child, i32)> {
        let spawned = self.attempt(command, record);
        // The attempt's descriptors are closed now: a spawn that bypasses the gate fails.
        let c = &self.child;
        for fd in [&c.pid, &c.go, &c.go_parent] {
            fd.store(-1, Ordering::SeqCst);
        }
        if spawned.is_err() {
            let _ = std::fs::remove_file(record);
        }
        spawned
    }

    fn attempt(&self, command: &mut Command, record: &Path) -> io::Result<(Child, i32)> {
        let (pid_read, pid_write) = io::pipe()?;
        let (go_read, go_write) = io::pipe()?;
        let c = &self.child;
        c.pid.store(pid_write.as_raw_fd(), Ordering::SeqCst);
        c.go.store(go_read.as_raw_fd(), Ordering::SeqCst);
        c.go_parent.store(go_write.as_raw_fd(), Ordering::SeqCst);
        std::thread::scope(|scope| {
            let recorder = scope.spawn(|| write_record(pid_read, go_write, record));
            let child = command.spawn();
            // The child has its own copies now. Closing these lets the recorder see
            // end-of-file when there was no fork.
            drop((pid_write, go_read));
            let recorded = recorder.join().map_err(recorder_panicked).and_then(|r| r);
            let child = child?;
            recorded?;
            // A child that has not been waited for always has its pid.
            let pid = child.id().and_then(|pid| i32::try_from(pid).ok());
            let pid = pid.ok_or(io::Error::other("the child has no pid"))?;
            Ok((child, pid))
        })
    }
}

/// The error of a recorder thread that panicked.
fn recorder_panicked(_: Box<dyn std::any::Any + Send>) -> io::Error {
    io::Error::other("the run recorder panicked")
}

/// The parent's half: reads the child's pid and writes its record, then sends the
/// child the go byte, 1 if the record was written and 0 if not. A refusal is sent, not
/// left to end-of-file: a sibling forked meanwhile can hold the pipe open until it
/// execs, and two siblings refused at once would each wait on the other.
fn write_record(
    pid_read: io::PipeReader,
    mut go_write: io::PipeWriter,
    record: &Path,
) -> io::Result<()> {
    let recorded = record_child(pid_read, record);
    let sent = go_write.write_all(&[u8::from(recorded.is_ok())]);
    recorded.and(sent)
}

/// Reads the child's pid and writes its record.
fn record_child(mut pid_read: io::PipeReader, record: &Path) -> io::Result<()> {
    let mut pid = [0_u8; 4];
    pid_read.read_exact(&mut pid)?;
    let pid = i32::from_ne_bytes(pid);
    let child = procs::process(pid)
        .ok_or_else(|| io::Error::other(format!("the child {pid} is not in the process table")))?;
    let line = Record {
        pgid: pid,
        start: child.start,
    }
    .line();
    std::fs::write(record, line)
}

/// The closure `pre_exec` runs in the child: [`in_child`].
fn child_steps(child: Arc<Gated>) -> impl FnMut() -> io::Result<()> + Send + Sync + 'static {
    move || in_child(&child)
}

/// The child's half: closes its copy of the parent's go end, sends its pid, waits for
/// the go byte, and execs. Returns only on failure. Async-signal-safe.
fn in_child(child: &Gated) -> io::Result<()> {
    // SAFETY: close(2) on a descriptor number; the parent set it for this attempt.
    unsafe { libc::close(child.go_parent.load(Ordering::SeqCst)) };
    // SAFETY: getpid takes nothing and cannot fail.
    let me = unsafe { libc::getpid() }.to_ne_bytes();
    // SAFETY: writes `me`, which outlives the call, to a pipe; 4 bytes are written
    // whole or not at all (less than PIPE_BUF).
    let wrote = unsafe { libc::write(child.pid.load(Ordering::SeqCst), me.as_ptr().cast(), 4) };
    if wrote != 4 {
        return Err(io::Error::last_os_error());
    }
    wait_for_go(child.go.load(Ordering::SeqCst), child.parent)?;
    let exec = &child.exec;
    // SAFETY: the program is a C string and both arrays are null-terminated arrays of C
    // strings, all owned by `exec`, which outlives the call. execve returns only on
    // failure.
    unsafe {
        libc::execve(
            exec.program.as_ptr(),
            exec.argv.as_ptr(),
            exec.envp.as_ptr(),
        )
    };
    Err(io::Error::last_os_error())
}

/// Waits for the go byte on `go`. Fails on a refusal (0: no record was written), on
/// end-of-file (the daemon is gone), and once `parent` is no longer this process's
/// parent: a sibling forked at the same moment may hold a copy of the go pipe's other
/// end, so end-of-file alone could come only when that sibling execs.
/// Async-signal-safe.
fn wait_for_go(go: libc::c_int, parent: i32) -> io::Result<()> {
    loop {
        let mut poll = libc::pollfd {
            fd: go,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one pollfd, which outlives the call.
        let ready = unsafe { libc::poll(&raw mut poll, 1, PARENT_CHECK_MS) };
        // SAFETY: getppid takes nothing and cannot fail.
        if ready == 0 && unsafe { libc::getppid() } != parent {
            return Err(io::Error::from_raw_os_error(libc::ECHILD));
        }
        if ready > 0 {
            let mut byte = 0_u8;
            // SAFETY: reads at most one byte into `byte`, which outlives the call.
            let read = unsafe { libc::read(go, (&raw mut byte).cast(), 1) };
            return if read == 1 && byte == 1 {
                Ok(())
            } else {
                Err(io::Error::from_raw_os_error(libc::ECANCELED))
            };
        }
        // A timeout with the parent still there, or a signal: poll again.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let dir = std::env::current_exe()
            .expect("test binary")
            .parent()
            .expect("deps")
            .join("kbf-driver-native-unit")
            .join(format!("record-{name}-{}", std::process::id()));
        let _ = kbf_outputs::remove_tree(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    /// A gated command running `program` with `args` and no environment.
    fn gated(program: &Path, args: &[&Path]) -> (Command, Gate) {
        let mut command = Command::new(program);
        let args = args.iter().map(|a| a.as_os_str());
        let exec = Exec::new(program, args, []).expect("exec");
        let gate = Gate::install(&mut command, exec);
        (command, gate)
    }

    /// Catches a record that does not round-trip, and one taken from a torn or foreign
    /// file (half a line, a missing number, a group id that would signal every process
    /// of the user or init's group).
    #[test]
    fn a_record_is_one_whole_line() {
        let record = Record {
            pgid: 4242,
            start: 987_654,
        };
        assert_eq!(record.line(), "4242 987654\n");
        assert_eq!(Record::parse(&record.line()), Some(record));
        for bad in [
            "",
            "4242 987654",
            "4242\n",
            "x 1\n",
            "4242 y\n",
            "0 5\n",
            "1 5\n",
            "-7 5\n",
        ] {
            assert_eq!(Record::parse(bad), None, "{bad:?}");
        }
        assert_eq!(
            path(Path::new("/s"), "lease-1-2"),
            Path::new("/s/runs/lease-1-2")
        );
    }

    /// Catches a program that runs before its record exists (the action prints its own
    /// record), a record that names another process or start time, arguments or an
    /// environment that differ from what was asked (an earlier variable winning, a
    /// variable of the daemon's own leaking in), and a gate that serves one spawn only.
    #[tokio::test]
    async fn the_record_is_written_before_the_program_runs() {
        let dir = dir("before");
        let record = dir.join("lease-1-1");
        // CARGO_MANIFEST_DIR is in this test's own environment, and must not leak.
        assert!(std::env::var_os("CARGO_MANIFEST_DIR").is_some());
        let script = format!(
            "cat {}; echo \"$A $B ${{CARGO_MANIFEST_DIR:-clean}}\"",
            record.display()
        );
        let program = Path::new("/bin/sh");
        let mut command = Command::new(program);
        command.stdout(std::process::Stdio::piped());
        let args = [OsStr::new("-c"), OsStr::new(&script)];
        let env =
            [("A", "first"), ("B", "b"), ("A", "a")].map(|(n, v)| (OsStr::new(n), OsStr::new(v)));
        let gate = Gate::install(&mut command, Exec::new(program, args, env).expect("exec"));
        for _ in 0..2 {
            let (child, pid) = gate.spawn(&mut command, &record).expect("spawn");
            let start = procs::process(pid).expect("the child").start;
            let out = child.wait_with_output().await.expect("wait");
            assert!(out.status.success(), "{out:?}");
            let expected = format!("{}a b clean\n", Record { pgid: pid, start }.line());
            assert_eq!(String::from_utf8_lossy(&out.stdout), expected);
        }
        assert!(Exec::new(program, [OsStr::new("a\0b")], []).is_err());
        assert!(Exec::new(program, [], [(OsStr::new("A"), OsStr::new("\0"))]).is_err());
    }

    /// Catches a program that runs although its record could not be written, a failed
    /// spawn that leaves its record behind, and a program that is no executable run
    /// through `/bin/sh` (the C library's `execvp` does that) instead of failing.
    #[tokio::test]
    async fn no_record_no_run() {
        let dir = dir("none");
        let marker = dir.join("ran");
        let (mut command, gate) = gated(Path::new("/usr/bin/touch"), &[&marker]);
        let unwritable = dir.join("absent").join("lease-1-1");
        let error = gate
            .spawn(&mut command, &unwritable)
            .expect_err("no record");
        assert_eq!(error.raw_os_error(), Some(libc::ECANCELED), "{error}");
        assert!(!marker.exists(), "the program ran without its record");

        let record = dir.join("lease-1-2");
        let (mut missing, gate) = gated(&dir.join("no-such-program"), &[]);
        let error = gate.spawn(&mut missing, &record).expect_err("no program");
        assert_eq!(error.kind(), io::ErrorKind::NotFound, "{error}");
        assert!(!record.exists(), "a failed spawn's record is removed");

        let script = dir.join("no-shebang");
        std::fs::write(&script, format!("touch {}\n", marker.display())).expect("write");
        std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .expect("chmod");
        let (mut command, gate) = gated(&script, &[]);
        let error = gate
            .spawn(&mut command, &record)
            .expect_err("no executable");
        assert_eq!(error.raw_os_error(), Some(libc::ENOEXEC), "{error}");
        assert!(!marker.exists(), "run through /bin/sh");
    }

    /// Catches a recorder that writes a record for a child it cannot find (the record
    /// would name no process, or a later one under a reused pid), and a recorder panic
    /// reported as anything but the recorder's.
    #[test]
    fn a_child_that_left_the_process_table_gets_no_record() {
        let dir = dir("gone");
        let record = dir.join("lease-1-1");
        let (pid_read, mut pid_write) = io::pipe().expect("pipe");
        let (mut go_read, go_write) = io::pipe().expect("pipe");
        // Above any kernel's pid limit, so no process has it.
        pid_write
            .write_all(&2_000_000_001_i32.to_ne_bytes())
            .expect("pid");
        let error = write_record(pid_read, go_write, &record).expect_err("no such child");
        assert!(error.to_string().contains("2000000001"), "{error}");
        assert!(!record.exists());
        let mut go = Vec::new();
        go_read.read_to_end(&mut go).expect("read");
        assert_eq!(go, [0], "a refusal, not a go byte");
        let panicked = recorder_panicked(Box::new("boom"));
        assert_eq!(panicked.to_string(), "the run recorder panicked");
    }

    /// What the daemon does while a child waits at the gate.
    enum Daemon {
        /// Sends the go byte at once.
        Go,
        /// Sends it after the child's first parent check.
        GoLate,
        /// Refuses: no record could be written.
        Refuses,
        /// Dies before sending it: its end of the pipe closes.
        Dies,
        /// Is gone, while a sibling keeps the pipe open.
        GoneSiblingHolds,
    }

    /// Runs the child's steps in this process, as a child of `parent`, against `daemon`;
    /// checks the pid it sends. The program to exec does not exist, so a child that
    /// passes the gate fails with ENOENT instead of replacing this process.
    fn at_gate(daemon: Daemon, parent: i32) -> io::Result<()> {
        let (mut pid_read, pid_write) = io::pipe().expect("pipe");
        let (go_read, mut go_write) = io::pipe().expect("pipe");
        // The child closes this copy, as a forked child closes the one it inherits.
        let go_parent = std::os::fd::IntoRawFd::into_raw_fd(go_write.try_clone().expect("dup"));
        let child = Arc::new(Gated {
            pid: AtomicI32::new(pid_write.as_raw_fd()),
            go: AtomicI32::new(go_read.as_raw_fd()),
            go_parent: AtomicI32::new(go_parent),
            parent,
            exec: Exec::new(Path::new("/nonexistent/kbf"), [], []).expect("exec"),
        });
        let mut sibling = None;
        let late = match daemon {
            Daemon::Go => {
                go_write.write_all(&[1]).expect("go");
                None
            }
            Daemon::GoLate => Some(std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(250));
                go_write.write_all(&[1]).expect("go");
            })),
            Daemon::Refuses => {
                go_write.write_all(&[0]).expect("refuse");
                // The sibling's copy: the refusal must not wait for end-of-file.
                sibling = Some(go_write);
                None
            }
            Daemon::Dies => {
                drop(go_write);
                None
            }
            Daemon::GoneSiblingHolds => {
                sibling = Some(go_write);
                None
            }
        };
        let outcome = child_steps(child)();
        drop(sibling);
        if let Some(late) = late {
            late.join().expect("the late go");
        }
        let mut pid = [0_u8; 4];
        pid_read.read_exact(&mut pid).expect("the child's pid");
        // SAFETY: getpid takes nothing and cannot fail.
        assert_eq!(i32::from_ne_bytes(pid), unsafe { libc::getpid() });
        outcome
    }

    /// Catches a child that runs its program when its daemon died before the go byte
    /// (end-of-file) or refused it (no record), one that waits on a refusal until a
    /// sibling holding the pipe open execs, one that waits for ever when its daemon is
    /// gone but a sibling
    /// forked at the same moment holds the pipe open (its parent changed), one that
    /// gives up while its daemon is merely slow, and one that does not send its pid
    /// first. Runs the child's steps in this process, whose parent is not itself.
    #[test]
    fn the_child_waits_for_go_and_gives_up_without_its_daemon() {
        // SAFETY: getppid takes nothing and cannot fail.
        let my_parent = unsafe { libc::getppid() };
        let not_my_parent = i32::try_from(std::process::id()).expect("pid");
        let passed = |outcome: io::Result<()>| outcome.err().and_then(|e| e.raw_os_error());
        assert_eq!(
            passed(at_gate(Daemon::Go, not_my_parent)),
            Some(libc::ENOENT)
        );
        assert_eq!(
            passed(at_gate(Daemon::GoLate, my_parent)),
            Some(libc::ENOENT)
        );
        assert_eq!(
            passed(at_gate(Daemon::Refuses, my_parent)),
            Some(libc::ECANCELED)
        );
        assert_eq!(
            passed(at_gate(Daemon::Dies, my_parent)),
            Some(libc::ECANCELED)
        );
        let orphaned = at_gate(Daemon::GoneSiblingHolds, not_my_parent);
        assert_eq!(passed(orphaned), Some(libc::ECHILD));

        // A pid that cannot be sent fails the child at once.
        let child = Gated {
            pid: AtomicI32::new(-1),
            go: AtomicI32::new(-1),
            go_parent: AtomicI32::new(-1),
            parent: my_parent,
            exec: Exec::new(Path::new("/nonexistent/kbf"), [], []).expect("exec"),
        };
        assert_eq!(passed(in_child(&child)), Some(libc::EBADF));
    }
}
