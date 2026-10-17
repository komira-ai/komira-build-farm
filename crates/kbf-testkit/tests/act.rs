//! `kbf-act`, run as a test payload runs it: what each step prints or writes, the exit
//! status it leaves, and the process-level effects (signals, children, sessions,
//! memory) that only a separate process shows.

use std::os::unix::fs::MetadataExt as _;
use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

const ACT: &str = env!("CARGO_BIN_EXE_kbf-act");

fn act(args: &[&str]) -> Command {
    let mut command = Command::new(ACT);
    command.args(args);
    command
}

fn output(args: &[&str]) -> Output {
    act(args).output().expect("kbf-act runs")
}

/// A fresh directory for one test, under Cargo's per-target temporary directory.
fn dir(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    if dir.exists() {
        std::fs::remove_dir_all(&dir).expect("old dir");
    }
    std::fs::create_dir_all(&dir).expect("dir");
    dir
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Waits up to 10 s for `done`.
fn eventually(what: &str, mut done: impl FnMut() -> bool) {
    let until = Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(Instant::now() < until, "never: {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Catches: `exit` leaving another status (a test that waits for exit 3 would pass
/// on a crash); `echo` on the wrong stream or with its words not joined by single
/// spaces; `env` printing a default over a set value, or nothing for an unset one;
/// `pwd` other than the working directory; `id` other than the real user id; a step
/// after `exit` run anyway.
#[test]
fn steps_print_what_they_say_and_exit_sets_the_status() {
    let dir = dir("act-print");
    let after = dir.join("after");
    let uid = std::fs::metadata(&dir).expect("dir").uid();
    let out = act(&[
        "echo",
        "out",
        "two",
        ";",
        "echo",
        "--stderr",
        "err",
        ";",
        "env",
        "KBF_UNSET",
        "unset",
        ";",
        "env",
        "KBF_SET",
        ";",
        "env",
        "KBF_SET",
        "default",
        ";",
        "pwd",
        ";",
        "id",
        ";",
        "exit",
        "3",
        ";",
        "append",
        &after.display().to_string(),
        "x",
    ])
    .current_dir(&dir)
    .env("KBF_SET", "value")
    .env_remove("KBF_UNSET")
    .output()
    .expect("runs");
    assert_eq!(out.status.code(), Some(3), "{out:?}");
    let real = std::fs::canonicalize(&dir).expect("real");
    assert_eq!(
        text(&out.stdout),
        format!("out two\nunset\nvalue\nvalue\n{}\n{uid}\n", real.display())
    );
    assert_eq!(text(&out.stderr), "err\n");
    assert!(!after.exists(), "a step after exit ran");
}

/// Catches: a run without `exit` leaving other than 0; `append` truncating instead of
/// appending, or dropping the newline; `upper` not applied or written elsewhere.
#[test]
fn files_are_appended_and_upper_cased() {
    let dir = dir("act-files");
    let (log, input, upper) = (dir.join("log"), dir.join("in"), dir.join("upper"));
    let log = log.display().to_string();
    std::fs::write(&input, "two\nThree\n").expect("input");
    let upper_path = upper.display().to_string();
    let input = input.display().to_string();
    for word in ["a b", "c"] {
        let mut args = vec!["append", &log];
        args.extend(word.split(' '));
        args.extend([";", "upper", &upper_path, &input]);
        let out = output(&args);
        assert_eq!(out.status.code(), Some(0), "{out:?}");
    }
    assert_eq!(std::fs::read_to_string(&log).expect("log"), "a b\nc\n");
    assert_eq!(
        std::fs::read_to_string(&upper).expect("upper"),
        "TWO\nTHREE\n"
    );
}

/// Catches: `append` writing a line in more than one write (the words, then the
/// newline): processes appending to one log at once then run their lines together, as
/// a fake `xcrun` and `xcodebuild` the daemon runs at once do (seen as a red run of
/// kbf-node's sandboxed survey test, whose log read `...-version` and the next line
/// as one).
#[test]
fn appends_from_processes_at_once_keep_their_lines_whole() {
    let dir = dir("act-append-at-once");
    let log = dir.join("log").display().to_string();
    let mut steps: Vec<&str> = Vec::new();
    for _ in 0..200 {
        steps.extend(["append", &log, "one", "whole", "line", ";"]);
    }
    steps.pop();
    let writers: Vec<_> = (0..8).map(|_| act(&steps).spawn().expect("runs")).collect();
    for mut writer in writers {
        assert!(writer.wait().expect("exits").success());
    }
    let text = std::fs::read_to_string(&log).expect("log");
    let broken: Vec<&str> = text.lines().filter(|l| *l != "one whole line").collect();
    assert!(
        broken.is_empty(),
        "{} lines run together, as {:?}",
        broken.len(),
        broken.first()
    );
    assert_eq!(text.lines().count(), 8 * 200);
}

/// Catches: `rename` copying (the old name left) or not replacing; `symlink` with its
/// target and link swapped, or a copy instead of a link; `chmod` setting other bits
/// than asked (a test of a directory nobody may write would pass on a writable one).
#[test]
fn paths_are_renamed_linked_and_chmodded() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = dir("act-paths");
    let (from, to, link) = (dir.join("from"), dir.join("to"), dir.join("link"));
    std::fs::write(&from, "moved").expect("from");
    std::fs::write(&to, "replaced").expect("to");
    let [from_arg, to_arg, link_arg] = [&from, &to, &link].map(|p| p.display().to_string());
    let out = output(&[
        "rename", &from_arg, &to_arg, ";", "symlink", "to", &link_arg, ";", "chmod", "640", &to_arg,
    ]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert!(!from.exists(), "the old name is left");
    assert_eq!(std::fs::read_to_string(&to).expect("to"), "moved");
    assert_eq!(
        std::fs::read_link(&link).expect("a link"),
        Path::new("to"),
        "the link's target"
    );
    let mode = std::fs::metadata(&to).expect("to").permissions().mode();
    assert_eq!(mode & 0o7777, 0o640, "{mode:o}");
}

/// Catches: `sleep-once` sleeping on every run (a fake that must stall one call and
/// then answer would stall them all), not sleeping on the first, or leaving no flag
/// behind; and a flag it cannot make passing as a sleep that happened.
#[test]
fn sleep_once_sleeps_only_in_the_first_run() {
    let dir = dir("act-sleep-once");
    let flag = dir.join("flag").display().to_string();
    let started = Instant::now();
    let out = output(&["sleep-once", &flag, "0.4", ";", "echo", "first"]);
    assert_eq!(text(&out.stdout), "first\n", "{out:?}");
    assert!(
        started.elapsed() >= Duration::from_millis(400),
        "{:?}",
        started.elapsed()
    );
    assert!(Path::new(&flag).exists(), "the flag is made");

    let started = Instant::now();
    let out = output(&["sleep-once", &flag, "30", ";", "echo", "second"]);
    assert_eq!(text(&out.stdout), "second\n", "{out:?}");
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "{:?}",
        started.elapsed()
    );

    let nowhere = dir.join("no/dir/flag").display().to_string();
    let out = output(&["sleep-once", &nowhere, "30"]);
    assert_ne!(out.status.code(), Some(0), "{out:?}");
}

/// Catches: `wait-for` returning before the path exists (a test gated on it would go
/// on at once), waiting on although it is there, or never giving up (a red run of a
/// test gated on it would hang instead of failing).
#[test]
fn wait_for_returns_when_the_path_appears_and_gives_up_in_time() {
    let dir = dir("act-wait-for");
    let gate = dir.join("gate");
    let gate_arg = gate.display().to_string();
    let started = Instant::now();
    let mut waiting = act(&["wait-for", &gate_arg, "10", ";", "echo", "through"])
        .stdout(Stdio::piped())
        .spawn()
        .expect("runs");
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        waiting.try_wait().expect("status").is_none(),
        "went on before the path existed"
    );
    std::fs::write(&gate, "").expect("gate");
    let out = waiting.wait_with_output().expect("exits");
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(text(&out.stdout), "through\n");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );

    let out = output(&["wait-for", &gate_arg, "0", ";", "echo", "there"]);
    assert_eq!(text(&out.stdout), "there\n", "{out:?}");

    let started = Instant::now();
    let missing = dir.join("missing").display().to_string();
    let out = output(&["wait-for", &missing, "0.3"]);
    let took = started.elapsed();
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(took >= Duration::from_millis(300), "{took:?}");
    assert!(took < Duration::from_secs(5), "{took:?}");
}

/// Catches: `act::main` answering only under the name `kbf-act` (another crate's bin
/// has a name of its own, and would run nothing), or differently from the hook.
#[test]
fn main_answers_under_any_name() {
    use std::os::unix::process::CommandExt as _;
    let out = Command::new(ACT)
        .arg0("kbf-other-act")
        .args(["echo", "from", "main", ";", "exit", "4"])
        .output()
        .expect("runs");
    assert_eq!(out.status.code(), Some(4), "{out:?}");
    assert_eq!(text(&out.stdout), "from main\n");
}

/// Catches: a failed step exiting 0 or 2 (taken for success or for a typo), a program
/// `run` ran that failed among them; a malformed step found only when reached, after
/// earlier steps ran, a child's step included; a failure with nothing on stderr; an
/// argument that is not UTF-8 accepted.
#[test]
fn failures_exit_1_and_usage_errors_exit_2_before_any_step() {
    use std::os::unix::ffi::OsStrExt as _;
    let dir = dir("act-failures");
    let early = dir.join("early").display().to_string();
    let missing = dir.join("missing").display().to_string();
    let no_dir = dir.join("no/such/file").display().to_string();
    let taken = dir.join("taken");
    std::fs::write(&taken, "").expect("taken");
    let taken = taken.display().to_string();
    let no_dir_under_file = format!("{taken}/d");
    let cases: [(&[&str], i32, &str); 19] = [
        (&["mkdir", &no_dir_under_file], 1, "mkdir "),
        (&["cat", &missing], 1, "read "),
        (&["mtime", &missing, "0"], 1, "mtime "),
        (&["run", &missing], 1, "run "),
        (&["run", ACT, "exit", "3"], 1, "ended: exit status: 3"),
        (
            &["spawn-child", "--pid", &no_dir, "exit", "0"],
            1,
            "append to ",
        ),
        (&["spawn-child", "exit", "0", ";;", "nope"], 2, "bad step"),
        (&["rename", &missing, &early], 1, "rename "),
        (&["symlink", &missing, &taken], 1, "link "),
        (&["chmod", "644", &missing], 1, "chmod "),
        (&["wait-for", &missing, "0"], 1, "not there after"),
        (&["env", "KBF_NEVER_SET"], 1, "KBF_NEVER_SET is not set"),
        (&["upper", &early, &missing], 1, "read "),
        (&["upper", &no_dir, ACT], 1, "create "),
        (&["append", &no_dir, "x"], 1, "append to "),
        (&["echo", "x"], 1, "write: "),
        (&["append", &early, "x", ";", "nope"], 2, "bad step"),
        (&["sleep", "soon"], 2, "not a number of seconds"),
        (&[], 2, "usage"),
    ];
    for (args, code, said) in cases {
        // macOS has no /dev/full.
        if args == ["echo", "x"] && !cfg!(target_os = "linux") {
            continue;
        }
        let mut command = act(args);
        command.env_remove("KBF_NEVER_SET");
        if args == ["echo", "x"] {
            command.stdout(std::fs::File::create("/dev/full").expect("/dev/full"));
        }
        let out = command.output().expect("runs");
        assert_eq!(out.status.code(), Some(code), "{args:?}: {out:?}");
        assert!(text(&out.stderr).contains(said), "{args:?}: {out:?}");
        assert!(!Path::new(&early).exists(), "{args:?}: a step ran");
    }
    let out = Command::new(ACT)
        .arg(std::ffi::OsStr::from_bytes(b"echo\xff"))
        .output()
        .expect("runs");
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    assert!(text(&out.stderr).contains("is not UTF-8"), "{out:?}");
}

/// Catches: `kill-self` sending another signal, or the program exiting by itself
/// instead of being killed (a test of how kbf reports a signalled action would see an
/// exit status); a signal the process ignores taken for a kill.
#[test]
fn kill_self_dies_by_the_signal() {
    let out = output(&["kill-self", "9", ";", "exit", "0"]);
    assert_eq!(out.status.signal(), Some(9), "{out:?}");
    let out = output(&["kill-self", "15"]);
    assert_eq!(out.status.signal(), Some(15), "{out:?}");
    // SIGWINCH (28 on Linux and macOS) is ignored by default.
    let out = output(&["kill-self", "28", ";", "exit", "0"]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(text(&out.stderr).contains("still running after signal 28"));
}

/// Catches: `sleep` not sleeping, or for whole seconds only.
#[test]
fn sleep_takes_its_time() {
    let started = Instant::now();
    let out = output(&["sleep", "0.3"]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let took = started.elapsed();
    assert!(took >= Duration::from_millis(300), "{took:?}");
    assert!(took < Duration::from_secs(1), "{took:?}");
}

/// Catches: `spawn-child` waiting for its child (a test of output held open past a
/// process's exit would see none), the child not given the same stdout, and the
/// child not running its step.
#[test]
fn spawn_child_runs_on_holding_stdout() {
    let dir = dir("act-spawn");
    let mark = dir.join("mark").display().to_string();
    let mut parent = act(&[
        "spawn-child",
        "append",
        &mark,
        "child",
        ";",
        "spawn-child",
        "sleep",
        "1",
        ";",
        "echo",
        "parent",
    ])
    .stdout(Stdio::piped())
    .spawn()
    .expect("runs");
    let started = Instant::now();
    let status = parent.wait().expect("parent exits");
    assert!(status.success(), "{status}");
    assert!(
        started.elapsed() < Duration::from_millis(900),
        "the parent waited for its child: {:?}",
        started.elapsed()
    );
    let mut stdout = String::new();
    std::io::Read::read_to_string(&mut parent.stdout.take().expect("stdout"), &mut stdout)
        .expect("read");
    assert!(
        started.elapsed() >= Duration::from_secs(1),
        "the child did not hold stdout: {:?}",
        started.elapsed()
    );
    assert_eq!(stdout, "parent\n");
    eventually("the child ran its step", || {
        std::fs::read_to_string(&mark).is_ok_and(|m| m == "child\n")
    });
}

/// Catches: `setsid` not making a new session (a test of a process that escapes its
/// group would test nothing), and a second `setsid` that cannot work reported as done.
#[test]
fn setsid_leads_a_new_session() {
    let mut child = act(&["setsid", ";", "sleep", "3"]).spawn().expect("runs");
    let pid = rustix::process::Pid::from_child(&child);
    eventually("the child leads its own session", || {
        rustix::process::getsid(Some(pid)).is_ok_and(|sid| sid == pid)
    });
    // Let it exit by itself: a killed process writes no coverage data.
    let status = child.wait().expect("reaped");
    assert!(status.success(), "{status}");
    let out = output(&["setsid", ";", "setsid"]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(text(&out.stderr).contains("setsid: "), "{out:?}");
}

/// Catches: `alloc` that allocates without touching (the pages stay unmapped and a
/// memory-limit test never reaches its limit), or frees before the next step.
#[cfg(target_os = "linux")]
#[test]
fn alloc_holds_resident_memory() {
    let mut child = act(&["alloc", "64", ";", "sleep", "3"])
        .spawn()
        .expect("runs");
    let status = format!("/proc/{}/status", child.id());
    eventually("64 MiB resident", || {
        std::fs::read_to_string(&status)
            .ok()
            .and_then(|s| {
                s.lines()
                    .find_map(|l| l.strip_prefix("VmRSS:"))
                    .and_then(|kb| kb.trim().trim_end_matches(" kB").parse::<u64>().ok())
            })
            .is_some_and(|kb| kb >= 64 << 10)
    });
    // Let it exit by itself: a killed process writes no coverage data.
    let status = child.wait().expect("reaped");
    assert!(status.success(), "{status}");
}

/// Catches: `mkdir` making only the last directory (a test that writes into a nested
/// output would fail for the wrong reason); `cat` printing other than the file's bytes;
/// `mtime` setting another time, or none on a directory (a test of how old a leftover
/// is would see it new); `pgid` printing another group than its own.
#[test]
fn directories_are_made_files_printed_and_times_set() {
    use std::os::unix::process::CommandExt as _;
    let dir = dir("act-mkdir-cat-mtime");
    let nested = dir.join("a/b/c");
    let file = dir.join("file");
    std::fs::write(&file, "two\nlines\n").expect("file");
    let [nested_arg, file_arg] = [&nested, &file].map(|p| p.display().to_string());
    let child = act(&[
        "mkdir",
        &nested_arg,
        ";",
        "mtime",
        &nested_arg,
        "946684800",
        ";",
        "cat",
        &file_arg,
        ";",
        "pgid",
    ])
    .stdout(Stdio::piped())
    .process_group(0)
    .spawn()
    .expect("runs");
    let pid = child.id();
    let out = child.wait_with_output().expect("exits");
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert!(nested.is_dir(), "the nested directory is missing");
    let modified = std::fs::metadata(&nested)
        .and_then(|m| m.modified())
        .expect("mtime");
    assert_eq!(
        modified,
        std::time::UNIX_EPOCH + Duration::from_secs(946_684_800)
    );
    // Its own group: the test made it the leader of one.
    assert_eq!(text(&out.stdout), format!("two\nlines\n{pid}\n"));
}

/// Catches: `run` not waiting for its program (the next step would run first), not
/// passing its arguments, or giving it other output streams.
#[test]
fn run_waits_for_its_program_and_shares_its_output() {
    let dir = dir("act-run");
    let mark = dir.join("mark").display().to_string();
    let out = output(&[
        "run", ACT, "sleep", "0.2", ";", "append", &mark, "inner", ";", "echo", "from", "inner",
        ";", "append", &mark, "outer",
    ]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(text(&out.stdout), "from inner\n");
    assert_eq!(
        std::fs::read_to_string(&mark).expect("mark"),
        "inner\nouter\n"
    );
}

/// Catches: `spawn-child --pid` reporting another process than the child (a test that
/// waits for that pid to die would pass at once), writing it elsewhere than asked, or
/// the child's `;;` steps not all run, or not in order. The child prints its own
/// process group id once `setsid` made it a group's leader: its pid, which must be the
/// one the parent printed.
#[test]
fn spawn_child_reports_its_pid_and_runs_every_step() {
    let dir = dir("act-spawn-pid");
    let (mark, pids) = (dir.join("mark"), dir.join("pids"));
    let [mark_arg, pids_arg] = [&mark, &pids].map(|p| p.display().to_string());
    // Waits for the child too: it holds stdout until it exits.
    let out = output(&[
        "spawn-child",
        "--pid",
        "-",
        "append",
        &mark_arg,
        "one",
        ";;",
        "setsid",
        ";;",
        "pgid",
        ";;",
        "append",
        &mark_arg,
        "two",
        ";",
        "spawn-child",
        "--pid",
        &pids_arg,
        "exit",
        "0",
    ]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let printed: Vec<u32> = text(&out.stdout)
        .lines()
        .map(|line| line.parse().expect("a pid"))
        .collect();
    assert_eq!(printed.len(), 2, "{printed:?}");
    assert_eq!(printed[0], printed[1], "the pid printed is not the child's");
    assert_eq!(
        std::fs::read_to_string(&mark).expect("mark"),
        "one\ntwo\n",
        "the child's steps"
    );
    let written = std::fs::read_to_string(&pids).expect("pids");
    assert!(written.trim().parse::<u32>().is_ok(), "{written:?}");
    assert!(written.ends_with('\n'), "{written:?}");
}

/// Catches: `write-lines` appending instead of truncating, joining its lines or
/// dropping a newline, or leaving a file it was given no line for as it was; `copy`
/// writing one input only, out of order, or appending to what `OUT` held; `remove`
/// leaving a directory that holds files, or following a link (the directory it
/// points to would go); `truncate` not making the file, or not cutting a longer one;
/// `inode-links` other than each path's inode and link count, in order.
#[test]
fn files_are_written_copied_cut_and_removed() {
    use std::os::unix::fs::FileTypeExt as _;
    let dir = dir("act-write-copy-remove");
    let path = |name: &str| dir.join(name).display().to_string();
    std::fs::write(dir.join("lines"), "old\nbytes\n").expect("lines");
    std::fs::write(dir.join("empty"), "old").expect("empty");
    std::fs::write(dir.join("one"), "first ").expect("one");
    std::fs::write(dir.join("two"), "second").expect("two");
    std::fs::write(dir.join("copy"), "a longer old copy").expect("copy");
    std::fs::write(dir.join("long"), "ten bytes!").expect("long");
    std::fs::write(dir.join("file"), "x").expect("file");
    std::fs::hard_link(dir.join("file"), dir.join("file2")).expect("hard link");
    std::fs::create_dir_all(dir.join("tree/sub")).expect("tree");
    std::fs::write(dir.join("tree/sub/f"), "x").expect("tree file");
    std::fs::create_dir(dir.join("kept")).expect("kept");
    std::fs::write(dir.join("kept/f"), "x").expect("kept file");
    std::os::unix::fs::symlink(dir.join("kept"), dir.join("link")).expect("link");
    let out = output(&[
        "write-lines",
        &path("lines"),
        "a",
        "b c",
        ";",
        "write-lines",
        &path("empty"),
        ";",
        "copy",
        &path("copy"),
        &path("one"),
        &path("two"),
        ";",
        "remove",
        &path("tree"),
        ";",
        "remove",
        &path("link"),
        ";",
        "remove",
        &path("two"),
        ";",
        "truncate",
        &path("new"),
        "2048",
        ";",
        "truncate",
        &path("long"),
        "3",
        ";",
        "mkfifo",
        &path("fifo"),
        ";",
        "inode-links",
        &path("links"),
        &path("file"),
        &path("one"),
    ]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let read = |name: &str| std::fs::read(dir.join(name)).expect(name);
    assert_eq!(read("lines"), b"a\nb c\n");
    assert_eq!(read("empty"), b"");
    assert_eq!(read("copy"), b"first second");
    assert!(!dir.join("tree").exists(), "the tree is left");
    assert!(!dir.join("link").exists(), "the link is left");
    assert!(dir.join("kept/f").exists(), "remove followed the link");
    assert!(!dir.join("two").exists(), "the file is left");
    assert_eq!(read("new"), vec![0; 2048]);
    assert_eq!(read("long"), b"ten");
    let fifo = std::fs::symlink_metadata(dir.join("fifo")).expect("fifo");
    assert!(fifo.file_type().is_fifo(), "{fifo:?}");
    let [file, one] = ["file", "one"].map(|name| {
        std::fs::metadata(dir.join(name))
            .expect(name)
            .ino()
            .to_string()
    });
    assert_eq!(
        String::from_utf8(read("links")).expect("utf-8"),
        format!("{file} 2\n{one} 1\n")
    );
}

/// Catches: a check that passes whatever it is asked (a test action gated on one
/// would check nothing): `test` with each predicate, and with `!`, passing and
/// failing as `sh`'s `test` does; `points-to` and `holds` failing only on what differs.
#[test]
fn checks_pass_and_fail_on_what_they_name() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = dir("act-checks");
    let path = |name: &str| dir.join(name).display().to_string();
    std::fs::write(dir.join("plain"), "two words").expect("plain");
    std::fs::write(dir.join("tool"), "").expect("tool");
    std::fs::set_permissions(dir.join("tool"), std::fs::Permissions::from_mode(0o755))
        .expect("chmod");
    std::os::unix::fs::symlink("plain", dir.join("link")).expect("link");
    let (plain, tool, link, missing) = (path("plain"), path("tool"), path("link"), path("no"));
    let dir_arg = dir.display().to_string();
    let passes: &[&[&str]] = &[
        &["test", "-e", &plain],
        &["test", "!", "-e", &missing],
        &["test", "-d", &dir_arg],
        &["test", "!", "-d", &plain],
        &["test", "!", "-d", &missing],
        &["test", "-x", &tool],
        &["test", "!", "-x", &plain],
        &["test", "!", "-x", &missing],
        &["points-to", &link, "plain"],
        &["holds", &plain, "two", "words"],
        &["holds", &link, "two words"],
    ];
    for args in passes {
        let out = output(args);
        assert_eq!(out.status.code(), Some(0), "{args:?}: {out:?}");
    }
    let fails: &[(&[&str], &str)] = &[
        (&["test", "-e", &missing], "test of "),
        (&["test", "!", "-e", &plain], "Exists is true"),
        (&["test", "-d", &plain], "Dir is false"),
        (&["test", "-x", &plain], "Executable is false"),
        (&["test", "!", "-x", &tool], "Executable is true"),
        (&["points-to", &link, "other"], "points to plain, not other"),
        (&["points-to", &plain, "x"], "readlink "),
        (
            &["holds", &plain, "two"],
            "holds \"two words\", not \"two\"",
        ),
        (&["holds", &missing, "x"], "read "),
    ];
    for (args, said) in fails {
        let out = output(args);
        assert_eq!(out.status.code(), Some(1), "{args:?}: {out:?}");
        assert!(text(&out.stderr).contains(said), "{args:?}: {out:?}");
    }
}

/// Catches: `zeros` writing other than that many zero bytes, or on the wrong stream;
/// `leads` printing other than whether the process leads its group and its session
/// (a test that the fake podman starts the action in a session of its own would pass
/// either way); `ignore-signal` not ignoring the signal, or reporting a signal that
/// cannot be ignored as ignored.
#[test]
fn zeros_leads_and_ignored_signals() {
    use std::os::unix::process::CommandExt as _;
    let out = output(&["zeros", "5", ";", "zeros", "--stderr", "3"]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(out.stdout, [0; 5]);
    assert_eq!(out.stderr, [0; 3]);

    let out = act(&["leads"]).process_group(0).output().expect("runs");
    assert_eq!(text(&out.stdout), "group 1 session 0\n", "{out:?}");
    let out = output(&["setsid", ";", "leads"]);
    assert_eq!(text(&out.stdout), "group 1 session 1\n", "{out:?}");

    let out = output(&["ignore-signal", "15", ";", "kill-self", "15"]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(
        text(&out.stderr).contains("still running after signal 15"),
        "{out:?}"
    );
    let out = output(&["ignore-signal", "9", ";", "echo", "on"]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(text(&out.stderr).contains("ignore signal 9: "), "{out:?}");
    assert!(out.stdout.is_empty(), "{out:?}");
}

/// Catches: the new verbs' failures exiting 0 (a failed step taken for success) or
/// with nothing on stderr: a path that is not there to remove or stat, a named pipe
/// where a file is, a file in a directory that is not there.
#[test]
fn failed_file_steps_exit_1() {
    let dir = dir("act-file-failures");
    let missing = dir.join("missing").display().to_string();
    let taken = dir.join("taken");
    std::fs::write(&taken, "").expect("taken");
    let taken = taken.display().to_string();
    let no_dir = dir.join("no/such/file").display().to_string();
    let out_file = dir.join("out").display().to_string();
    let cases: &[(&[&str], &str)] = &[
        (&["remove", &missing], "remove "),
        (&["mkfifo", &taken], "mkfifo "),
        (&["truncate", &no_dir, "1"], "truncate "),
        (&["copy", &out_file, &missing], "read "),
        (&["inode-links", &out_file, &missing], "stat "),
        (&["write-lines", &no_dir, "x"], "create "),
    ];
    for (args, said) in cases {
        let out = output(args);
        assert_eq!(out.status.code(), Some(1), "{args:?}: {out:?}");
        assert!(text(&out.stderr).contains(said), "{args:?}: {out:?}");
        assert!(!Path::new(&out_file).exists(), "{args:?}: wrote OUT");
    }
}
