//! The doors the profile canaries try beyond a plain file write: a Mach service name
//! checked in with launchd, the sandbox's answer for Launch Services, a file system
//! mounted on a directory of the lease, and a launchd agent bootstrapped from a file
//! of the lease. Each is a call (or a system tool) that exists on a Mac alone; what it
//! says is read back as a [`Seen`] by a function that does not need a Mac, so the
//! reading is tested on Linux, and a [`Refusal`] is accepted only in the one form the
//! door gives ([`Op::refusals`]).

use std::fmt;
use std::io::{self, Read as _};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use super::{Fs, Op, Seen};

/// `BOOTSTRAP_NOT_PRIVILEGED`: launchd (or the sandbox in its place) refused the
/// name.
pub const BOOTSTRAP_NOT_PRIVILEGED: i32 = 1100;
/// The error `launchctl bootstrap` prints for a job launchd will not take from a
/// sandboxed process: `Bootstrap failed: 5: Input/output error`.
pub const BOOTSTRAP_IO_ERROR: i32 = 5;

/// How a door says no.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// A system call failed with this errno.
    Errno(i32),
    /// A Mach call answered this `kern_return_t`.
    Kern(i32),
    /// `launchctl bootstrap` failed with this error.
    Bootstrap(i32),
    /// `sandbox_check` answered this: the operation is denied.
    Sandbox(i32),
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Errno(n) => write!(f, "errno={n}"),
            Self::Kern(n) => write!(f, "kern={n}"),
            Self::Bootstrap(n) => write!(f, "bootstrap={n}"),
            Self::Sandbox(n) => write!(f, "sandbox={n}"),
        }
    }
}

impl Refusal {
    /// The refusal `text` spells, exactly as [`Display`](fmt::Display) writes it.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let (kind, n) = text.split_once('=')?;
        let n = n.parse::<i32>().ok()?;
        match kind {
            "errno" => Some(Self::Errno(n)),
            "kern" => Some(Self::Kern(n)),
            "bootstrap" => Some(Self::Bootstrap(n)),
            "sandbox" => Some(Self::Sandbox(n)),
            _ => None,
        }
    }
}

/// How long a tool the probe runs may take: a door that hangs is a door that proved
/// nothing, and the daemon waits [`super::WITHIN`] for the whole probe.
const TOOL_WITHIN: Duration = Duration::from_secs(3);

/// What a tool the probe ran gave back.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Out {
    /// The exit code; `None` if a signal ended it, or it was killed for taking too long.
    pub code: Option<i32>,
    /// What it printed, on its output and its error output together.
    pub text: String,
}

/// Runs `program` with `args`, no environment and no input, and waits for it at most
/// [`TOOL_WITHIN`] (a tool still running then is killed and its code is `None`).
///
/// # Errors
/// The program could not be started.
pub fn run(program: &str, args: &[String]) -> io::Result<Out> {
    run_within(program, args, TOOL_WITHIN)
}

/// [`run`] that waits for the tool at most `within`.
///
/// # Errors
/// The program could not be started.
pub fn run_within(program: &str, args: &[String], within: Duration) -> io::Result<Out> {
    let (mut reader, writer) = io::pipe()?;
    let mut child = Command::new(program)
        .args(args)
        .env_clear()
        .stdin(Stdio::null())
        .stdout(writer.try_clone()?)
        .stderr(writer)
        .spawn()?;
    let (send, read) = mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = reader.read_to_end(&mut bytes);
        let _ = send.send(bytes);
    });
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break Some(status);
        }
        if started.elapsed() > within {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let bytes = read
        .recv_timeout(Duration::from_secs(1))
        .unwrap_or_default();
    let mut text = String::from_utf8_lossy(&bytes).into_owned();
    if status.is_none() {
        text.push_str("\n(killed: still running after the tool's time)");
    }
    Ok(Out {
        code: status.and_then(|s| s.code()),
        text,
    })
}

/// `bootstrap_check_in` of `name` with launchd: the `kern_return_t` it answers.
///
/// # Errors
/// Never on a Mac; elsewhere there is no such call.
#[cfg(target_os = "macos")]
pub fn check_in(name: &str) -> io::Result<i32> {
    use std::ffi::{CString, c_char};

    unsafe extern "C" {
        static bootstrap_port: u32;
        fn bootstrap_check_in(bootstrap: u32, name: *const c_char, service: *mut u32) -> i32;
    }
    let name = CString::new(name).map_err(io::Error::other)?;
    let mut port = 0_u32;
    // SAFETY: `name` is a NUL-terminated string that outlives the call, `port` is a
    // valid place for the receive right it writes (the process exit drops it), and
    // `bootstrap_port` is libSystem's own, set before `main`.
    Ok(unsafe { bootstrap_check_in(bootstrap_port, name.as_ptr(), &raw mut port) })
}

/// `bootstrap_check_in` of `name` with launchd.
///
/// # Errors
/// Always: there is no such call off a Mac.
#[cfg(not(target_os = "macos"))]
pub fn check_in(_name: &str) -> io::Result<i32> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "a Mach service can be checked in on macOS only",
    ))
}

/// `refusal` as a refusal of `op` if that is how `op`'s door says no; otherwise a
/// failure that names it, which proves nothing about the door.
fn refused(op: Op, refusal: Refusal, out: &Out) -> Seen {
    if op.refusals().contains(&refusal) {
        Seen::Refused(refusal)
    } else {
        Seen::Failed(format!(
            "not a refusal of this door: {refusal}; {}",
            said(out)
        ))
    }
}

/// What a tool said, for a failure: its lines joined, the first 400 characters.
fn said(out: &Out) -> String {
    let lines: Vec<&str> = out
        .text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let text: String = lines.join(" | ").chars().take(400).collect();
    format!("exit {:?}: {text}", out.code)
}

/// The integer written right after `marker` in `text`.
fn number_after(text: &str, marker: &str) -> Option<i32> {
    let rest = &text[text.find(marker)? + marker.len()..];
    let end = rest
        .char_indices()
        .find(|(i, c)| !(c.is_ascii_digit() || (*i == 0 && *c == '-')))
        .map_or(rest.len(), |(i, _)| i);
    rest[..end].parse().ok()
}

/// Checks in a Mach service name (unique to this process, declared by no job) with
/// launchd. With the sandbox's rule launchd is not reached and the answer is 1100; with
/// the rule dropped launchd looks the name up and answers 1102, which is a failure of
/// this door (see [`refused`]), never `Allowed`: that split is what the canary reads.
pub fn mach_register(fs: &dyn Fs) -> Seen {
    let name = format!("com.kbf.canary.{}", std::process::id());
    match fs.check_in(&name) {
        Ok(0) => Seen::Allowed,
        Ok(n) => refused(Op::MachRegister, Refusal::Kern(n), &Out::default()),
        Err(e) => Seen::Failed(e.to_string()),
    }
}

/// Mounts a `devfs` on `dir`.
///
/// # Errors
/// The call's error (`EPERM` when the sandbox denies `file-mount`).
#[cfg(target_os = "macos")]
pub fn mount_devfs(dir: &Path) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt as _;

    let dir = std::ffi::CString::new(dir.as_os_str().as_bytes()).map_err(io::Error::other)?;
    // SAFETY: both strings are NUL-terminated and outlive the call; `devfs` takes no data.
    let made = unsafe { libc::mount(c"devfs".as_ptr(), dir.as_ptr(), 0, std::ptr::null_mut()) };
    if made == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Mounts a `devfs` on `dir`.
///
/// # Errors
/// Always: there is no such mount off a Mac.
#[cfg(not(target_os = "macos"))]
pub fn mount_devfs(_dir: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "devfs can be mounted on macOS only",
    ))
}

/// Unmounts what is mounted on `dir`, by force.
///
/// # Errors
/// The call's error.
#[cfg(target_os = "macos")]
pub fn unmount(dir: &Path) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt as _;

    let dir = std::ffi::CString::new(dir.as_os_str().as_bytes()).map_err(io::Error::other)?;
    // SAFETY: the string is NUL-terminated and outlives the call.
    if unsafe { libc::unmount(dir.as_ptr(), libc::MNT_FORCE) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Unmounts what is mounted on `dir`.
///
/// # Errors
/// Always: there is nothing to unmount off a Mac.
#[cfg(not(target_os = "macos"))]
pub fn unmount(_dir: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "devfs can be mounted on macOS only",
    ))
}

/// Whether this process's sandbox denies `operation` (`sandbox_check`, the question
/// Launch Services and the kernel ask the sandbox before they act).
///
/// # Errors
/// The operation name cannot be passed; elsewhere there is no sandbox to ask.
#[cfg(target_os = "macos")]
pub fn sandbox_denies(operation: &str) -> io::Result<bool> {
    use std::ffi::{CString, c_char, c_int};

    unsafe extern "C" {
        fn sandbox_check(pid: libc::pid_t, operation: *const c_char, filter: c_int, ...) -> c_int;
    }
    /// `SANDBOX_FILTER_NONE`: the operation alone, no object.
    const FILTER_NONE: c_int = 0;
    /// `SANDBOX_CHECK_NO_REPORT`: a denial found by the check is not logged as a violation.
    const NO_REPORT: c_int = 0x4000_0000;
    let operation = CString::new(operation).map_err(io::Error::other)?;
    // SAFETY: `operation` is a NUL-terminated string that outlives the call, the filter
    // `SANDBOX_FILTER_NONE` takes no further argument (the report flag is a modifier of the
    // filter word), and the pid is our own.
    let answer =
        unsafe { sandbox_check(libc::getpid(), operation.as_ptr(), FILTER_NONE | NO_REPORT) };
    let errno = io::Error::last_os_error();
    read_sandbox_answer(answer, errno)
}

/// Whether this process's sandbox denies `operation` on `path` (`sandbox_check` with a
/// path filter). The profile is asked, not the file system: the answer does not depend
/// on whether `path` exists, nor on a data-vault protection that refuses a user anyway.
///
/// # Errors
/// The operation name or the path cannot be passed; elsewhere there is no sandbox to ask.
#[cfg(target_os = "macos")]
pub fn sandbox_denies_path(operation: &str, path: &std::path::Path) -> io::Result<bool> {
    use std::ffi::{CString, c_char, c_int};
    use std::os::unix::ffi::OsStrExt as _;

    unsafe extern "C" {
        fn sandbox_check(pid: libc::pid_t, operation: *const c_char, filter: c_int, ...) -> c_int;
    }
    /// `SANDBOX_FILTER_PATH`: the object is a path, passed as a C string.
    const FILTER_PATH: c_int = 1;
    /// `SANDBOX_CHECK_NO_REPORT`: a denial found by the check is not logged as a violation.
    const NO_REPORT: c_int = 0x4000_0000;
    let operation = CString::new(operation).map_err(io::Error::other)?;
    let path = CString::new(path.as_os_str().as_bytes()).map_err(io::Error::other)?;
    // SAFETY: `operation` and `path` are NUL-terminated strings that outlive the call, the
    // filter `SANDBOX_FILTER_PATH` takes exactly one `const char *` after it, and the pid
    // is our own.
    let answer = unsafe {
        sandbox_check(
            libc::getpid(),
            operation.as_ptr(),
            FILTER_PATH | NO_REPORT,
            path.as_ptr(),
        )
    };
    let errno = io::Error::last_os_error();
    read_sandbox_answer(answer, errno)
}

/// Whether this process's sandbox denies `operation` on `path`.
///
/// # Errors
/// Always: there is no sandbox to ask off a Mac.
#[cfg(not(target_os = "macos"))]
pub fn sandbox_denies_path(_operation: &str, _path: &std::path::Path) -> io::Result<bool> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "the sandbox can be asked on macOS only",
    ))
}

/// What `sandbox_check` answered: 1 denied, 0 allowed, anything else (it answers -1
/// when it cannot tell) is an error carrying `errno`, so it is never read as a denial.
///
/// # Errors
/// The answer is neither 0 nor 1.
pub fn read_sandbox_answer(answer: i32, errno: io::Error) -> io::Result<bool> {
    match answer {
        0 => Ok(false),
        1 => Ok(true),
        n => Err(io::Error::other(format!(
            "sandbox_check answered {n} instead of 0 or 1: {errno}"
        ))),
    }
}

/// Whether this process's sandbox denies `operation`.
///
/// # Errors
/// Always: there is no sandbox to ask off a Mac.
#[cfg(not(target_os = "macos"))]
pub fn sandbox_denies(_operation: &str) -> io::Result<bool> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "the sandbox can be asked on macOS only",
    ))
}

/// Asks the sandbox whether Launch Services may open anything (`lsopen`). This shows
/// that the rule is in the profile the process runs under; it does not show Launch
/// Services refusing an open. A slot user
/// has no login session on a node, so an application cannot be opened to see the rule
/// work (`open` and `LSOpenCFURLRef` fail or wait with the rule dropped as with it
/// kept); the question Launch Services asks before it starts one is asked instead.
pub fn ls_open(fs: &dyn Fs) -> Seen {
    match fs.sandbox_denies("lsopen") {
        Ok(true) => Seen::Refused(Refusal::Sandbox(1)),
        Ok(false) => Seen::Allowed,
        Err(e) => Seen::Failed(e.to_string()),
    }
}

/// Asks the sandbox whether this process may read or write each of the protected stores
/// ([`crate::network::PROTECTED_STORES`]): refused only if every one of the four
/// questions is denied. As for `lsopen`, the question is asked of the profile instead of
/// opening a folder: the stores are data-vault folders that refuse a user whatever the
/// profile says, and may not exist on a fresh Mac, so an open would be refused with the
/// rule dropped as with it kept.
pub fn read_protected(fs: &dyn Fs) -> Seen {
    for store in crate::network::PROTECTED_STORES {
        for operation in ["file-read-data", "file-write-data"] {
            match fs.sandbox_denies_path(operation, Path::new(store)) {
                Ok(true) => {}
                Ok(false) => return Seen::Allowed,
                Err(e) => return Seen::Failed(e.to_string()),
            }
        }
    }
    Seen::Refused(Refusal::Sandbox(1))
}

/// Mounts a `devfs` on the new directory `mount`, which a user may do on a directory
/// it owns when the sandbox lets it (`mount(2)` is `EPERM` otherwise); what an open
/// door mounted is unmounted, and the directory is removed.
pub fn mount(fs: &dyn Fs, mount: &Path) -> Seen {
    if let Err(e) = fs.make_dir(mount) {
        return Seen::Failed(format!("mount point: {e}"));
    }
    let seen = match fs.mount_devfs(mount) {
        Ok(()) => match fs.unmount(mount) {
            Ok(()) => Seen::Allowed,
            Err(e) => {
                return Seen::Failed(format!(
                    "mounted, and could not unmount {}: {e}",
                    mount.display()
                ));
            }
        },
        Err(e) => e.raw_os_error().map_or_else(
            || Seen::Failed(e.to_string()),
            |n| refused(Op::Mount, Refusal::Errno(n), &Out::default()),
        ),
    };
    fs.remove(mount);
    seen
}

fn path(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// The label of the job the bootstrap canary offers launchd.
fn job_label() -> String {
    format!("com.kbf.canary.{}", std::process::id())
}

/// The job file: a `Background` agent that launchd would keep alive.
fn job_plist(label: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
<plist version=\"1.0\">\n\
<dict>\n\
<key>Label</key><string>{label}</string>\n\
<key>ProgramArguments</key><array><string>/bin/sleep</string><string>300</string></array>\n\
<key>ProcessType</key><string>Background</string>\n\
<key>KeepAlive</key><true/>\n\
<key>RunAtLoad</key><true/>\n\
</dict>\n\
</plist>\n"
    )
}

/// What `launchctl bootstrap` said: loaded, or `Bootstrap failed: <n>`.
fn read_bootstrap(out: &Out) -> Seen {
    if out.code == Some(0) {
        return Seen::Allowed;
    }
    number_after(&out.text, "Bootstrap failed: ").map_or_else(
        || Seen::Failed(said(out)),
        |n| refused(Op::LaunchdBootstrap, Refusal::Bootstrap(n), out),
    )
}

/// Writes the job file `job` and asks launchd to bootstrap it into the user's domain;
/// the job is booted out after every result but a refusal (an open door loaded it, and
/// a tool killed for taking too long may have loaded it before it was killed), and the
/// file is removed.
///
/// No profile rule governs this door: a bootstrap from a sandboxed process is refused
/// whatever the profile says. So a refusal proves a bootstrap is refused in the slot
/// environment, not that the sandbox is the reason.
pub fn bootstrap(fs: &dyn Fs, job: &Path) -> Seen {
    let label = job_label();
    if let Err(e) = fs.write(job, &job_plist(&label)) {
        return Seen::Failed(format!("job file: {e}"));
    }
    let domain = format!("user/{}", fs.uid());
    let seen = match fs.run(
        "/bin/launchctl",
        &["bootstrap".into(), domain.clone(), path(job)],
    ) {
        Ok(out) => read_bootstrap(&out),
        Err(e) => Seen::Failed(format!("could not start launchctl: {e}")),
    };
    if !matches!(seen, Seen::Refused(_)) {
        let _ = fs.run(
            "/bin/launchctl",
            &["bootout".into(), format!("{domain}/{label}")],
        );
    }
    fs.remove(job);
    seen
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches a `sandbox_check` that failed (-1, or any answer but 0 and 1) read as a
    /// denial, which would let the `lsopen` canary pass with its rule removed.
    #[test]
    fn a_sandbox_check_that_failed_is_not_a_denial() {
        let errno = || io::Error::from_raw_os_error(libc::EINVAL);
        assert!(!read_sandbox_answer(0, errno()).unwrap());
        assert!(read_sandbox_answer(1, errno()).unwrap());
        for n in [-1, 2, i32::MIN] {
            let e = read_sandbox_answer(n, errno()).unwrap_err().to_string();
            assert!(e.contains(&n.to_string()), "{e}");
        }
    }

    fn out(code: i32, text: &str) -> Out {
        Out {
            code: Some(code),
            text: text.to_owned(),
        }
    }

    /// Catches a number read from a tool's text that takes another one (a digit run
    /// that continues, a sign in the middle), reads none as zero, or panics at the end
    /// of the text.
    #[test]
    fn a_number_is_read_after_its_marker_only() {
        assert_eq!(number_after("Code=-54 x", "Code="), Some(-54));
        assert_eq!(number_after("Code=-54", "Code="), Some(-54));
        assert_eq!(number_after("a Code=7, Code=8", "Code="), Some(7));
        assert_eq!(number_after("Code=", "Code="), None);
        assert_eq!(number_after("Code=x", "Code="), None);
        assert_eq!(number_after("Code=-", "Code="), None);
        assert_eq!(number_after("Code=5-4", "Code="), Some(5));
        assert_eq!(number_after("none", "Code="), None);
    }

    /// Catches a bootstrap read as refused for the wrong text, and a failure report that
    /// loses what the tool said.
    #[test]
    fn bootstrap_is_read_by_its_own_words() {
        assert_eq!(read_bootstrap(&out(0, "")), Seen::Allowed);
        let five = "Bootstrap failed: 5: Input/output error";
        assert_eq!(
            read_bootstrap(&out(5, five)),
            Seen::Refused(Refusal::Bootstrap(5))
        );
        assert!(matches!(read_bootstrap(&out(1, "usage")), Seen::Failed(_)));
        let other = read_bootstrap(&out(1, "\nBootstrap failed: 113\nCould not find\n"));
        assert_eq!(
            other,
            Seen::Failed(
                "not a refusal of this door: bootstrap=113; exit Some(1): Bootstrap failed: 113 | Could not find"
                    .to_owned()
            )
        );
    }

    /// Catches a tool run that hangs the probe (a door that waits for a window server
    /// holds the whole canary until the daemon gives up on it), that loses what the
    /// tool printed on either output, or that reports a killed tool as one that exited.
    #[test]
    fn a_tool_is_waited_for_a_while_and_its_output_is_kept() {
        let out =
            run_within("/bin/ls", &["/nonexistent/kbf-door".into()], TOOL_WITHIN).expect("ls");
        assert!(out.code.is_some_and(|c| c != 0), "{out:?}");
        assert!(out.text.contains("kbf-door"), "{out:?}");
        let started = Instant::now();
        let slow =
            run_within("/bin/sleep", &["30".into()], Duration::from_millis(100)).expect("sleep");
        assert!(started.elapsed() < Duration::from_secs(10));
        assert_eq!(slow.code, None);
        assert!(slow.text.contains("killed"), "{slow:?}");
        assert!(run("/nonexistent/tool", &[]).is_err());
    }
}
