use std::io::Read;
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;

use super::*;
use crate::plan::{HostError, Mode, Planner, RunAs, execute};
use crate::testkit::{ScratchHost, scratch};

fn p(s: &str) -> HostPath {
    HostPath::new(s).expect("valid test path")
}

fn me() -> Owner {
    Owner {
        uid: rustix::process::getuid().as_raw(),
        gid: rustix::process::getgid().as_raw(),
    }
}

fn host(name: &str) -> (HostExec, std::path::PathBuf) {
    let dir = scratch(name);
    (HostExec::new(Root::new(&dir).expect("absolute")), dir)
}

fn mode_of(path: &Path) -> u32 {
    fs::symlink_metadata(path)
        .expect("stat")
        .permissions()
        .mode()
        & 0o7777
}

/// Catches the real host writing outside its root, ignoring the permissions or owner
/// a step names, or a second plan that is not empty after an apply on a real file
/// system (the fake could hide a comparison the real `inspect` gets wrong).
#[test]
fn steps_land_under_the_root_and_a_rerun_plans_nothing() {
    let (mut exec, dir) = host("apply");
    fs::write(dir.join("old"), "x").expect("seed");
    fs::create_dir(dir.join("olddir")).expect("seed");
    let plan_once = |exec: &HostExec| {
        let mut planner = Planner::new(exec);
        planner
            .dir(p("/etc"), Perms::new(0o750), Some(me()))
            .expect("plan");
        planner
            .file(
                p("/etc/a.env"),
                Perms::new(0o640),
                Some(me()),
                b"A=1\n".to_vec(),
            )
            .expect("plan");
        planner
            .file(p("/etc/b"), Perms::new(0o600), None, Vec::new())
            .expect("plan");
        planner.absent(p("/old")).expect("plan");
        planner.absent(p("/olddir")).expect("plan");
        planner.finish()
    };
    let plan = plan_once(&exec);
    assert_eq!(plan.steps().len(), 5);
    execute(&plan, RunAs::User, Mode::Apply, &mut exec, &mut Vec::new()).expect("apply");

    assert_eq!(fs::read(dir.join("etc/a.env")).expect("written"), b"A=1\n");
    assert_eq!(mode_of(&dir.join("etc")), 0o750);
    assert_eq!(mode_of(&dir.join("etc/a.env")), 0o640);
    assert_eq!(mode_of(&dir.join("etc/b")), 0o600);
    assert!(!dir.join("old").exists() && !dir.join("olddir").exists());
    let names: Vec<_> = fs::read_dir(dir.join("etc"))
        .expect("list")
        .map(|e| e.expect("entry").file_name())
        .collect();
    assert_eq!(names.len(), 2, "a temporary copy was left: {names:?}");

    assert!(plan_once(&exec).is_empty(), "the re-run planned steps");
}

/// Catches a write that edits the file in place (a reader could see half of it) or
/// trips over a copy an interrupted run left behind.
#[test]
fn a_write_replaces_the_file_through_a_fresh_copy() {
    let (mut exec, dir) = host("replace");
    fs::write(dir.join("f"), "old").expect("seed");
    let held = fs::File::open(dir.join("f")).expect("open the old file");
    fs::write(dir.join(".f.kbf-host-tmp"), "stale").expect("stale copy");
    let step = Step::WriteFile {
        path: p("/f"),
        perms: Perms::new(0o644),
        owner: None,
        bytes: b"new".to_vec(),
    };
    exec.apply(&step).expect("write");
    assert_eq!(fs::read(dir.join("f")).expect("read"), b"new");
    let mut old = String::new();
    (&held)
        .read_to_string(&mut old)
        .expect("read the held file");
    assert_eq!(old, "old", "the file was rewritten in place");
    assert!(!dir.join(".f.kbf-host-tmp").exists());
}

/// Catches `Mkdir` treating a file at the path as the directory (it would chmod the
/// file, and every later run would plan the directory again), and an existing
/// directory refused instead of having its permissions set.
#[test]
fn mkdir_sets_an_existing_directory_and_refuses_a_file() {
    let (mut exec, dir) = host("mkdir");
    fs::create_dir(dir.join("d")).expect("seed");
    fs::write(dir.join("f"), "").expect("seed");
    let mkdir = |path| Step::Mkdir {
        path: p(path),
        perms: Perms::new(0o700),
        owner: None,
    };
    fs::set_permissions(dir.join("f"), fs::Permissions::from_mode(0o644)).expect("chmod");
    exec.apply(&mkdir("/d")).expect("an existing directory");
    assert_eq!(mode_of(&dir.join("d")), 0o700);
    let err = exec
        .apply(&mkdir("/f"))
        .expect_err("a file is not a directory");
    assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
    assert_eq!(mode_of(&dir.join("f")), 0o644, "the file was chmodded");
    let err = exec.apply(&mkdir("/no/parent")).expect_err("no parent");
    assert_eq!(err.kind(), io::ErrorKind::NotFound);
}

/// Catches `inspect` following a link (a link to a file would read as that file, and a
/// plan could never replace the link) or losing its target (a link plan could never
/// see it is done), and errors other than "not found" read
/// as absent.
#[test]
fn inspect_sees_a_link_and_its_target_and_reports_errors() {
    let (mut exec, dir) = host("inspect");
    fs::write(dir.join("target"), "x").expect("seed");
    std::os::unix::fs::symlink(dir.join("target"), dir.join("link")).expect("link");
    assert_eq!(
        exec.inspect(&p("/link")).expect("inspect"),
        Some(Entry::Link {
            target: dir.join("target")
        })
    );
    assert_eq!(exec.inspect(&p("/none")).expect("inspect"), None);
    let err = exec
        .inspect(&p("/target/below"))
        .expect_err("a file has no children");
    assert_eq!(err.kind(), io::ErrorKind::NotADirectory);
    exec.apply(&Step::Remove { path: p("/link") })
        .expect("remove the link");
    assert!(
        dir.join("target").exists(),
        "removing a link removed its target"
    );
    let err = exec
        .apply(&Step::Remove { path: p("/link") })
        .expect_err("gone");
    assert_eq!(err.kind(), io::ErrorKind::NotFound);
}

/// A write whose copy cannot be made fails, and leaves nothing behind.
#[test]
fn a_write_into_a_missing_directory_fails() {
    let (mut exec, dir) = host("nodir");
    let step = Step::WriteFile {
        path: p("/no/f"),
        perms: Perms::new(0o644),
        owner: None,
        bytes: Vec::new(),
    };
    let err = exec.apply(&step).expect_err("no parent directory");
    assert_eq!(err.kind(), io::ErrorKind::NotFound);
    assert_eq!(fs::read_dir(&dir).expect("list").count(), 0);
}

/// Catches a stale copy the run cannot remove being ignored (a directory there would
/// make every write fail later, with a less clear error).
#[test]
fn a_stale_copy_that_cannot_be_removed_fails_the_write() {
    let (mut exec, dir) = host("stuck");
    fs::create_dir(dir.join(".f.kbf-host-tmp")).expect("a directory where the copy goes");
    let step = Step::WriteFile {
        path: p("/f"),
        perms: Perms::new(0o644),
        owner: None,
        bytes: Vec::new(),
    };
    exec.apply(&step)
        .expect_err("the copy's path is a directory");
    assert!(!dir.join("f").exists());
}

/// Catches a command that inherits the caller's environment (a secret or a `PATH` the
/// kit never chose would reach it) or loses the variables the step names, and a
/// failing command read as a success. `printenv NAME` exits 1 when NAME is unset.
#[test]
fn a_command_gets_only_the_environment_it_names() {
    let (mut exec, _dir) = host("run");
    let printenv = || Cmd::new(p("/usr/bin/printenv"));
    let inherited = Step::Run(printenv().arg("HOME"));
    let err = exec
        .apply(&inherited)
        .expect_err("HOME must not be inherited");
    let msg = err.to_string();
    assert!(msg.starts_with("/usr/bin/printenv exited with "), "{msg}");
    exec.apply(&Step::Run(printenv().arg("KBF_X").env("KBF_X", "1")))
        .expect("the named variable is set");
    let missing = Step::Run(Cmd::new(p("/nonexistent/tool")));
    let err = exec.apply(&missing).expect_err("no such program");
    assert_eq!(err.kind(), io::ErrorKind::NotFound);
}

/// Answers each connection with the next status in `statuses`, then closes.
/// The number of answers sent so far is counted before each is written, so a client
/// that has read an answer sees it counted.
fn server(statuses: Vec<&'static str>) -> (u16, Arc<AtomicUsize>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let answered = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&answered);
    let handle = thread::spawn(move || {
        for status in statuses {
            let (mut conn, _) = listener.accept().expect("accept");
            let mut reader = BufReader::new(&conn);
            let mut request = String::new();
            while !request.ends_with("\r\n\r\n") {
                reader.read_line(&mut request).expect("read the request");
            }
            let want = format!(
                "GET /readyz HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
            );
            assert_eq!(request, want);
            count.fetch_add(1, Ordering::SeqCst);
            conn.write_all(status.as_bytes()).expect("answer");
        }
    });
    (port, answered, handle)
}

fn wait(exec: &mut HostExec, url: &str, deadline: Duration) -> io::Result<()> {
    let url = HttpUrl::new(url).expect("url");
    exec.apply(&Step::WaitHttp { url, deadline })
}

/// Catches a wait that gives up at the first non-2xx answer (a service still starting
/// answers 503) or takes a non-2xx answer as ready.
#[test]
fn a_wait_retries_until_the_url_answers_2xx() {
    let (mut exec, _dir) = host("wait_ok");
    let (port, answered, server) = server(vec![
        "HTTP/1.1 503 Service Unavailable\r\n\r\n",
        "garbage\r\n",
        "HTTP/1.1 204 No Content\r\n\r\n",
    ]);
    wait(
        &mut exec,
        &format!("http://127.0.0.1:{port}/readyz"),
        Duration::from_secs(10),
    )
    .expect("ready on the third answer");
    // Checked before joining: a wait that stopped early leaves the server in `accept`.
    assert_eq!(
        answered.load(Ordering::SeqCst),
        3,
        "ready before the 2xx answer"
    );
    server.join().expect("the server saw three requests");
}

/// Catches a wait that never ends, or ends without an error, when the URL never
/// answers 2xx; the error names the last answer.
#[test]
fn a_wait_fails_at_its_deadline() {
    let (mut exec, _dir) = host("wait_late");
    let (port, answered, server) = server(vec!["HTTP/1.1 500 Oops\r\n\r\n"]);
    let url = format!("http://127.0.0.1:{port}/readyz");
    let err = wait(&mut exec, &url, Duration::ZERO).expect_err("never ready");
    assert_eq!(err.kind(), io::ErrorKind::TimedOut);
    assert!(err.to_string().ends_with("last: status 500"), "{err}");
    assert_eq!(answered.load(Ordering::SeqCst), 1);
    server.join().expect("one request");

    // Nothing listens on the port now: each attempt fails to connect.
    let start = Instant::now();
    let err = wait(&mut exec, &url, Duration::from_millis(500)).expect_err("refused");
    assert_eq!(err.kind(), io::ErrorKind::TimedOut);
    assert!(start.elapsed() >= Duration::from_millis(500));
    let err = wait(&mut exec, "http://[::1]:1/readyz", Duration::ZERO).expect_err("refused");
    assert_eq!(err.kind(), io::ErrorKind::TimedOut);
    let err = wait(&mut exec, "http://host.invalid/", Duration::ZERO).expect_err("no name");
    assert_eq!(err.kind(), io::ErrorKind::TimedOut);
}

/// The real binary check refuses the user-owned test binary, through the trait.
#[test]
fn the_host_checks_the_running_binary() {
    let (exec, _dir) = host("binary");
    assert!(exec.check_binary().is_err());
}

fn upper(b: &[u8]) -> Result<Vec<u8>, String> {
    Ok(b.to_ascii_uppercase())
}

fn refuse(_: &[u8]) -> Result<Vec<u8>, String> {
    Err("the output is not a key pair".to_owned())
}

/// Catches a query that loses the command's input or output, inherits the caller's
/// environment, or reads a non-zero exit as an error (the kit reads the status: `mc
/// ls` of a missing bucket exits 1).
#[test]
fn a_query_feeds_stdin_and_returns_the_output() {
    let (exec, _dir) = host("query");
    let cat = Cmd::new(p("/bin/cat")).stdin(b"policy\n".to_vec());
    let out = exec.query(&cat).expect("cat runs");
    assert!(out.status.success());
    assert_eq!(out.stdout, b"policy\n");
    let unset = exec
        .query(&Cmd::new(p("/usr/bin/printenv")).arg("HOME"))
        .expect("printenv runs");
    assert_eq!(unset.status.code(), Some(1), "HOME was inherited");
    let missing = exec.query(&Cmd::new(p("/nonexistent/tool")));
    assert_eq!(
        missing.err().map(|e| e.kind()),
        Some(io::ErrorKind::NotFound)
    );
}

/// Catches a capture that writes the raw output instead of the filtered bytes, leaves
/// the mode to the umask, or writes a file when the tool fails or the filter refuses
/// (a half-made key file would be kept by the next run).
#[test]
fn a_capture_writes_the_filtered_output_or_nothing() {
    let (mut exec, dir) = host("capture");
    let step = |program: &str, filter| Step::Capture {
        cmd: Cmd::new(p(program)).stdin(b"key\n".to_vec()),
        filter: Filter {
            name: "upper",
            apply: filter,
        },
        path: p("/s3.env"),
        perms: Perms::new(0o600),
        owner: None,
    };
    exec.apply(&step("/bin/cat", upper)).expect("capture");
    assert_eq!(fs::read(dir.join("s3.env")).expect("written"), b"KEY\n");
    assert_eq!(mode_of(&dir.join("s3.env")), 0o600);
    fs::remove_file(dir.join("s3.env")).expect("clean");

    let err = exec
        .apply(&step("/bin/cat", refuse))
        .expect_err("the filter refuses");
    assert_eq!(err.to_string(), "the output is not a key pair");
    let err = exec
        .apply(&step("/usr/bin/false", upper))
        .expect_err("the tool fails");
    assert!(
        err.to_string().starts_with("/usr/bin/false exited with "),
        "{err}"
    );
    let err = exec
        .apply(&step("/nonexistent/tool", upper))
        .expect_err("no such tool");
    assert_eq!(err.kind(), io::ErrorKind::NotFound);
    assert!(
        !dir.join("s3.env").exists(),
        "a failed capture wrote the file"
    );
}

/// More input than a pipe holds (64 KiB on Linux and macOS): the write blocks until
/// the child exits, and then fails with EPIPE, every time.
const UNREAD: usize = 1 << 20;

/// Catches a step whose child exits without reading its input being reported as
/// "Broken pipe" instead of by the child's status (komira-build-farm#488). The input
/// is larger than a pipe, so the write meets a closed pipe on every run, not by luck
/// of timing: a failing tool reads as its exit, and one that exits 0 succeeds.
#[test]
fn a_child_that_exits_without_reading_its_input_is_judged_by_its_status() {
    let (mut exec, dir) = host("unread_input");
    let step = |program: &str| Step::Capture {
        cmd: Cmd::new(p(program)).stdin(vec![b'k'; UNREAD]),
        filter: Filter {
            name: "upper",
            apply: upper,
        },
        path: p("/s3.env"),
        perms: Perms::new(0o600),
        owner: None,
    };
    let err = exec
        .apply(&step("/usr/bin/false"))
        .expect_err("the tool fails");
    assert!(
        err.to_string().starts_with("/usr/bin/false exited with "),
        "{err}"
    );
    assert!(
        !dir.join("s3.env").exists(),
        "a failed capture wrote the file"
    );

    exec.apply(&step("/usr/bin/true"))
        .expect("the tool succeeds");
    assert_eq!(fs::read(dir.join("s3.env")).expect("written"), b"");
    let run = Step::Run(Cmd::new(p("/usr/bin/true")).stdin(vec![b'k'; UNREAD]));
    exec.apply(&run).expect("a run that ignores its input");
}

/// Catches a planning-time GET that reports a status other than the one served.
#[test]
fn http_status_reads_one_answer() {
    let (port, answered, server) = server(vec!["HTTP/1.1 503 Service Unavailable\r\n\r\n"]);
    let (exec, _dir) = host("http_status");
    let url = HttpUrl::new(&format!("http://127.0.0.1:{port}/readyz")).expect("url");
    assert_eq!(exec.http_status(&url).expect("answered"), 503);
    assert_eq!(answered.load(Ordering::SeqCst), 1);
    server.join().expect("server");
}

/// Catches `stat` reading a file (it must not: a swapfile is 128 GiB, a key file may be
/// another user's), following a link, or reporting a size other than the file's; and
/// a removal or directory step that reads what is at its path.
#[test]
fn stat_reports_without_reading_and_absent_never_reads() {
    let (exec, dir) = host("stat");
    fs::write(dir.join("locked"), "12345").expect("seed");
    fs::set_permissions(dir.join("locked"), fs::Permissions::from_mode(0o000)).expect("chmod");
    std::os::unix::fs::symlink("locked", dir.join("link")).expect("link");
    fs::create_dir(dir.join("d")).expect("dir");

    let m = exec.stat(&p("/locked")).expect("stat").expect("there");
    assert_eq!(
        (m.kind, m.perms, m.owner, m.len),
        (Kind::File, Perms::new(0), me(), 5)
    );
    assert_eq!(
        exec.stat(&p("/link")).expect("stat").map(|m| m.kind),
        Some(Kind::Other)
    );
    assert_eq!(
        exec.stat(&p("/d")).expect("stat").map(|m| m.kind),
        Some(Kind::Dir)
    );
    assert_eq!(exec.stat(&p("/none")).expect("stat"), None);
    assert!(
        exec.inspect(&p("/locked")).is_err(),
        "reading a 0000 file must fail here"
    );

    let mut planner = Planner::new(&exec);
    assert!(planner.absent(p("/locked")).expect("no read"));
    assert!(
        planner
            .dir(p("/locked"), Perms::new(0o755), None)
            .expect("no read")
    );
    assert!(
        !planner
            .dir(p("/d"), Perms::new(mode_of(&dir.join("d"))), None)
            .expect("same")
    );
    assert!(!planner.absent(p("/none")).expect("nothing there"));
    fs::set_permissions(dir.join("locked"), fs::Permissions::from_mode(0o600)).expect("chmod");
}

/// Catches the input write swallowing an error other than EPIPE (a real failure to
/// hand the tool its input), or failing on EPIPE or success. No real pipe write fails
/// any other way on demand, so the rule is checked on its own.
#[test]
fn only_a_closed_pipe_ends_the_input_write_quietly() {
    assert!(fed(Ok(())).is_ok());
    assert!(fed(Err(io::ErrorKind::BrokenPipe.into())).is_ok());
    let err = fed(Err(io::ErrorKind::PermissionDenied.into())).expect_err("kept");
    assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
}

fn link_step(path: &str, target: &str) -> Step {
    Step::Symlink {
        path: p(path),
        target: PathBuf::from(target),
    }
}

fn names_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<_> = fs::read_dir(dir)
        .expect("list")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// Catches a link step that does not land under the root, that follows an old link to
/// a directory (and makes the new link inside it), that leaves its temporary link
/// behind, or a plan made again after the apply that is not empty (a real `inspect`
/// that loses the target would plan the flip on every run).
///
/// Mutant seen red: `Planner::symlink` comparing nothing (`done = false`).
#[test]
fn a_link_flips_to_its_new_target_and_a_rerun_plans_nothing() {
    let mut exec = ScratchHost::new("exec-link-flip");
    let root = exec.root.clone();
    fs::create_dir_all(root.join("usr/local/kbf/releases/a")).expect("seed");
    fs::create_dir_all(root.join("usr/local/kbf/releases/b")).expect("seed");
    let plan_once = |exec: &ScratchHost, target: &str| {
        let mut planner = Planner::new(exec);
        planner
            .symlink(p("/usr/local/kbf/current"), PathBuf::from(target))
            .expect("plan");
        planner.finish()
    };
    for target in ["releases/a", "releases/b"] {
        let plan = plan_once(&exec, target);
        assert_eq!(plan.steps(), [link_step("/usr/local/kbf/current", target)]);
        execute(&plan, RunAs::Root, Mode::Apply, &mut exec, &mut Vec::new()).expect("apply");
        let link = root.join("usr/local/kbf/current");
        assert_eq!(fs::read_link(&link).expect("a link"), PathBuf::from(target));
        assert!(
            plan_once(&exec, target).is_empty(),
            "the re-run planned steps"
        );
    }
    assert_eq!(
        names_in(&root.join("usr/local/kbf")),
        ["current", "releases"],
        "a temporary link was left"
    );
    assert!(
        names_in(&root.join("usr/local/kbf/releases/a")).is_empty(),
        "the flip followed the old link into its directory"
    );
}

/// Catches a flip that removes the old link before the new one exists (a reader, or a
/// restart, would find no link): here making the new link fails (its target is longer
/// than the system takes), and the old link must still be there, unchanged.
///
/// Mutant seen red: remove `path`, then make the link at `path` (no temporary link and
/// rename).
#[test]
fn a_flip_that_fails_leaves_the_old_link() {
    let (mut exec, dir) = host("link-fails");
    std::os::unix::fs::symlink("releases/a", dir.join("current")).expect("seed");
    let long = "x".repeat(5000);
    let err = exec
        .apply(&link_step("/current", &long))
        .expect_err("a target longer than PATH_MAX is refused");
    assert_eq!(err.kind(), io::ErrorKind::InvalidFilename, "{err}");
    assert_eq!(
        fs::read_link(dir.join("current")).expect("the old link is still there"),
        PathBuf::from("releases/a")
    );
    assert_eq!(names_in(&dir), ["current"]);
}

/// Catches a flip with a window in which the path names nothing: a reader polls the link
/// while it flips between two targets, and must see one of them every time.
///
/// Mutant seen red: remove `path`, then make the link at `path`.
#[test]
fn a_reader_always_finds_the_link_while_it_flips() {
    let (mut exec, dir) = host("link-reader");
    std::os::unix::fs::symlink("a", dir.join("current")).expect("seed");
    let link = dir.join("current");
    let stop = Arc::new(AtomicBool::new(false));
    let reader = {
        let stop = Arc::clone(&stop);
        thread::spawn(move || {
            let mut missing = 0;
            while !stop.load(Ordering::Relaxed) {
                if fs::read_link(&link).is_err() {
                    missing += 1;
                }
            }
            missing
        })
    };
    for i in 0..300 {
        let target = if i % 2 == 0 { "b" } else { "a" };
        exec.apply(&link_step("/current", target)).expect("flip");
    }
    stop.store(true, Ordering::Relaxed);
    assert_eq!(reader.join().expect("reader"), 0, "the link was missing");
}

/// Catches a link step that replaces a file or a directory (rename(2) would put a link
/// over a file, losing it) instead of refusing, a planner that plans that step instead of
/// refusing first, and errors other than "not found" at the path read as absent.
///
/// Mutant seen red: the step's not-a-link check removed (the file is replaced).
#[test]
fn a_link_never_replaces_a_file_or_a_directory() {
    let (mut exec, dir) = host("link-refuses");
    fs::write(dir.join("file"), "keep").expect("seed");
    fs::create_dir(dir.join("dir")).expect("seed");
    for name in ["/file", "/dir"] {
        let err = exec.apply(&link_step(name, "x")).expect_err(name);
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists, "{name}: {err}");
        assert!(err.to_string().contains("not a symbolic link"), "{err}");
        let refused = Planner::new(&exec).symlink(p(name), PathBuf::from("x"));
        assert!(
            matches!(&refused, Err(HostError::NotALink { path }) if *path == p(name)),
            "{name}: {refused:?}"
        );
    }
    assert_eq!(fs::read(dir.join("file")).expect("kept"), b"keep");
    assert!(dir.join("dir").is_dir());
    assert_eq!(names_in(&dir), ["dir", "file"]);
    let err = exec
        .apply(&link_step("/file/below", "x"))
        .expect_err("a file has no children");
    assert_eq!(err.kind(), io::ErrorKind::NotADirectory);
}

/// Catches a stale temporary name the step cannot clear (a directory someone left
/// there) being ignored: the step must fail and leave the link as it was.
#[test]
fn a_stale_temporary_that_cannot_be_removed_fails_the_flip() {
    let (mut exec, dir) = host("link-stale");
    std::os::unix::fs::symlink("a", dir.join("current")).expect("seed");
    fs::create_dir_all(dir.join(".current.kbf-host-tmp/x")).expect("seed");
    exec.apply(&link_step("/current", "b"))
        .expect_err("the temporary name is taken by a directory");
    assert_eq!(
        fs::read_link(dir.join("current")).expect("kept"),
        PathBuf::from("a")
    );
}

/// Catches an error reading a link read as "not a link" (a link that vanished between
/// the stat and the read would plan as a socket), and EINVAL read as an error.
#[test]
fn only_einval_reads_as_not_a_link() {
    let einval = Err(io::Error::from(io::ErrorKind::InvalidInput));
    assert_eq!(link_or_other(einval).expect("not a link"), Entry::Other);
    let gone = Err(io::Error::from(io::ErrorKind::NotFound));
    assert_eq!(
        link_or_other(gone).expect_err("an error").kind(),
        io::ErrorKind::NotFound
    );
}
