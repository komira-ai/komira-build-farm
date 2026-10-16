//! The daemon's helper re-execs, run through `kbf-all-in-one` (the design,
//! komira-build-farm#657, sections 3 and 7.3): the binary answers each helper mode
//! before it reads a flag, runs the helper, and exits with the helper's status; a
//! helper binds no listener, opens no server directory and starts no async runtime.
//! (The native driver's `slots` helper is macOS's, where this binary does not run.)

#![cfg(target_os = "linux")]

use std::io::{BufRead as _, BufReader, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

const BIN: &str = env!("CARGO_BIN_EXE_kbf-all-in-one");

/// A fresh directory for one test.
fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("kbf-all-in-one-helpers")
        .join(format!("{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch");
    dir
}

/// Kills the process when dropped, so a failed test leaves nothing running.
struct Running(Child);

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// What each open descriptor of process `pid` names (`/proc/<pid>/fd`), and whether it
/// is a directory.
fn descriptors(pid: u32) -> Vec<(String, bool)> {
    let fds = std::fs::read_dir(format!("/proc/{pid}/fd")).expect("list fds");
    fds.map(|fd| {
        let path = fd.expect("an fd").path();
        let target = std::fs::read_link(&path)
            .map_or_else(|e| format!("<{e}>"), |t| t.to_string_lossy().into_owned());
        let dir = std::fs::metadata(&path).is_ok_and(|m| m.is_dir());
        (target, dir)
    })
    .collect()
}

/// Catches a keeper re-exec of this binary that starts anything of the farm before it
/// runs as the keeper: dispatched after the flags are read (it would fail on them, not
/// answer READY), or after an async runtime, a listener or a server directory exists.
/// The keeper, waiting for its stdin to close as it does for a run's whole life, has
/// one thread (a runtime has its workers), no socket, no epoll or eventfd descriptor
/// (a runtime's driver), and no directory open; once its stdin closes it writes the
/// kill file as the cgroup mode does and exits 0.
#[test]
fn a_keeper_run_of_the_binary_holds_no_runtime_listener_or_directory() {
    let dir = scratch("keeper");
    let kill = dir.join("cgroup.kill");
    std::fs::write(&kill, "").expect("plant the kill file");
    let mut child = Command::new(BIN)
        .args(["__keeper", "cgroup"])
        .arg(&kill)
        .arg("1")
        .current_dir(&dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn");
    let stdin = child.stdin.take().expect("stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("stdout"));
    let running = Running(child);
    let mut line = String::new();
    stdout.read_line(&mut line).expect("read");
    assert_eq!(line, "kbf-keeper: ready\n", "not a keeper's answer");

    let pid = running.0.id();
    let threads = std::fs::read_dir(format!("/proc/{pid}/task"))
        .expect("list threads")
        .count();
    assert_eq!(threads, 1, "a helper started threads (a runtime?)");
    let open = descriptors(pid);
    let farm = open.iter().find(|(target, dir)| {
        *dir || target.starts_with("socket:") || target.starts_with("anon_inode:")
    });
    assert_eq!(farm, None, "a helper holds more than its pipes: {open:?}");
    assert_eq!(
        std::fs::read_dir(&dir).expect("list").count(),
        1,
        "the helper made files where it ran"
    );

    drop(stdin);
    let mut running = running;
    let status = running.0.wait().expect("wait");
    assert_eq!(status.code(), Some(0), "{status}");
    assert_eq!(std::fs::read_to_string(&kill).expect("read"), "1\n");
    // Nothing on stdout after READY: no server start line.
    let mut rest = String::new();
    std::io::Read::read_to_string(&mut stdout, &mut rest).expect("read");
    assert_eq!(rest, "");
}

/// Runs the binary with `args` in `dir`; its exit status, stdout and stderr.
fn run(dir: &Path, args: &[&str]) -> (Option<i32>, String, String) {
    let out = Command::new(BIN)
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::null())
        .output()
        .expect("run");
    let text = |b: Vec<u8>| String::from_utf8_lossy(&b).into_owned();
    (out.status.code(), text(out.stdout), text(out.stderr))
}

/// Catches the container driver's `__chown-dirs` helper not dispatched by this binary,
/// or dispatched after the flags are read (clap would refuse its arguments, exit 2,
/// and print its own usage): the helper runs and exits with each of its own statuses.
/// 0 for a tree it gave to this process's own user and group (which it may), 1 for a
/// directory that is not there, 2 for an owner it cannot read, each with the helper's
/// words, and none with a server start line.
#[test]
fn the_chown_dirs_helper_runs_and_exits_with_its_own_status() {
    use std::os::unix::fs::MetadataExt as _;

    let dir = scratch("chown");
    let tree = dir.join("tree");
    std::fs::create_dir_all(tree.join("a/b")).expect("mkdir");
    std::fs::File::create(tree.join("a/file"))
        .and_then(|mut f| f.write_all(b"x"))
        .expect("write");
    let meta = std::fs::metadata(&tree).expect("stat");
    let owner = format!("{}:{}", meta.uid(), meta.gid());
    let tree = tree.to_str().expect("UTF-8");

    let (code, stdout, stderr) = run(&dir, &["__chown-dirs", &owner, tree]);
    assert_eq!(code, Some(0), "{stderr}");
    assert_eq!((stdout.as_str(), stderr.as_str()), ("", ""));

    let gone = dir.join("gone");
    let gone = gone.to_str().expect("UTF-8");
    let (code, stdout, stderr) = run(&dir, &["__chown-dirs", &owner, gone]);
    assert_eq!(code, Some(1), "{stderr}");
    assert!(
        stderr.starts_with(&format!("kbf-daemon __chown-dirs: {gone}: ")),
        "{stderr}"
    );
    assert_eq!(stdout, "");

    let (code, stdout, stderr) = run(&dir, &["__chown-dirs", "nobody", tree]);
    assert_eq!(code, Some(2), "{stderr}");
    assert!(
        stderr.starts_with("kbf-daemon __chown-dirs: usage: __chown-dirs "),
        "{stderr}"
    );
    assert_eq!(stdout, "");
}
