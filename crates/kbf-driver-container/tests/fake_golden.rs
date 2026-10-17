//! The stand-in `podman` (`support/fake.rs`) against its golden file,
//! `fixtures/fake-podman.golden`: every verb and knob, called in order in one
//! directory, must give the exit codes, the output and the files written there, byte
//! for byte, so the driver suites keep the meaning they were written against.

mod support;

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The `action` knob both `start`s run, one argument per line: its environment (with
/// [`HOST_VARIABLE`], which only `create` runs with), an argument with each path name the fake expands (and two it must not: `$UPPERX` and
/// `x$CG`), whether it leads its own process group and session, both streams and an
/// exit code of its own.
const ACTION: &str = concat!(
    "env\nCG\n;\n",
    "env\nROOT\n;\n",
    "env\nUPPER\n;\n",
    "env\nFAKE_LEASE\n;\n",
    "env\nKBF_GOLDEN_HOST\nunset\n;\n",
    "echo\n$CG\n$ROOT/r\n$UPPER/u\n$UPPERX\nx$CG\n;\n",
    "leads\n;\n",
    "echo\n--stderr\nerr\n;\n",
    "exit\n3\n",
);

/// A variable of the `env-host` knob's prefix that each `create` runs with and no
/// other call does: `create` records it, and the action of a `start` printing it shows
/// it was handed on from the record. Set here, not taken from the test's own
/// environment, so the record is the same on every machine.
const HOST_VARIABLE: (&str, &str) = ("KBF_GOLDEN_HOST", "from-create");

/// The `--format` the driver reads a container's record with (`podman::Podman::ended`).
const RECORD: &str = "--format={{.State.Status}} {{.State.ExitCode}} {{.State.FinishedAt.IsZero}}";

/// One call: files below the fake's directory to write (or, with `None`, remove) first, then the arguments after
/// `--cgroup-manager=cgroupfs` (`None`: the flag is left out too), and whether the
/// files are recorded right after it as well as after the last call.
struct Call {
    knobs: &'static [(&'static str, Option<&'static str>)],
    args: Option<Vec<String>>,
    files_after: bool,
}

fn call(knobs: &'static [(&'static str, Option<&'static str>)], args: &[&str]) -> Call {
    Call {
        knobs,
        args: Some(args.iter().map(|a| (*a).to_owned()).collect()),
        files_after: false,
    }
}

/// `call`, with the files recorded right after it: for a state a later call undoes
/// (the upper directory unreadable while the overlay is the container's, which the
/// `chown 0:0` after it restores), which the files after the last call cannot show.
fn call_then_files(knobs: &'static [(&'static str, Option<&'static str>)], args: &[&str]) -> Call {
    Call {
        files_after: true,
        ..call(knobs, args)
    }
}

/// What one call gave.
#[derive(Debug, PartialEq)]
struct Answer {
    args: Option<Vec<String>>,
    code: Option<i32>,
    stdout: String,
    stderr: String,
    files: Option<BTreeMap<PathBuf, Entry>>,
}

/// A file or directory the calls left: its mode and, for a file, its bytes.
#[derive(Debug, PartialEq)]
enum Entry {
    Dir(u32),
    File(u32, String),
    Link(PathBuf),
}

/// Each verb, and each knob that changes one but `image-hangs` (a 30-second sleep).
/// None of them makes a program other than the fake fail (its message would be that
/// program's, not the fake's).
fn calls(dir: &Path) -> Vec<Call> {
    let root = dir.join("lease/root");
    let upper = dir.join("lease/upper");
    let volume = format!(
        "--volume={}:/w:O,upperdir={},workdir={}",
        root.display(),
        upper.display(),
        dir.join("lease/work").display()
    );
    let upper = upper.to_str().expect("utf-8 path").to_owned();
    let gone = dir.join("gone");
    let gone = gone.to_str().expect("utf-8 path").to_owned();
    let create = [
        "create",
        "--pull=never",
        "--name=kbf-lease-1-1",
        "--label=kbf.owner=node-1",
        "--cgroup-parent=/actions/kbf-lease-1-1",
        &volume,
        "docker://registry.test/busybox",
    ];
    let ps = [
        "ps",
        "--all",
        "--filter=label=kbf.owner=node-1",
        "--format=x",
    ];
    vec![
        Call {
            knobs: &[],
            args: None,
            files_after: false,
        },
        call(&[], &["frobnicate"]),
        call(&[], &["info", "--format=x"]),
        call(&[("state/info-fails", Some(""))], &["info"]),
        call(&[], &["image", "inspect", "--format={{.Id}}", "ref"]),
        call(&[("state/image-id", None)], &["image", "inspect", "ref"]),
        call(&[("state/create-fails", Some(""))], &create),
        call(&[("state/create-fails", None)], &["create"]),
        // `env-host`: `create` records its variables of the prefix ([`HOST_VARIABLE`]
        // only), which every later `start` hands to the action.
        call(
            &[
                ("state/block-log", Some("stdout")),
                ("state/env-host", Some("KBF_GOLDEN_")),
            ],
            &create,
        ),
        call(&[("state/block-log", None)], &ps),
        // Another node's containers: none of them is this one.
        call(
            &[],
            &[
                "ps",
                "--all",
                "--filter=label=kbf.owner=node-2",
                "--format=x",
            ],
        ),
        call(&[("state/ps-extra", Some("node-1 other\n"))], &ps),
        call(&[("state/ps-fails", Some(""))], &ps),
        call(&[], &["inspect", "--type=container", "kbf-lease-1-1"]),
        call(&[], &["kill", "--signal=TERM", "kbf-lease-1-1"]),
        call(
            &[("state/action", Some(ACTION))],
            &["start", "--attach", "kbf-lease-1-1"],
        ),
        call(&[], &["inspect", "kbf-lease-1-1"]),
        // The driver's format names `FinishedAt`: the record says whether an exit was
        // recorded. `unrecorded-inspects` stands in for a record read before it was.
        call(
            &[],
            &["inspect", "--type=container", RECORD, "kbf-lease-1-1"],
        ),
        call(
            &[("state/unrecorded-inspects", Some("2\n"))],
            &["inspect", "--type=container", RECORD, "kbf-lease-1-1"],
        ),
        call(&[], &["inspect", "kbf-lease-1-1"]),
        call(
            &[],
            &["inspect", "--type=container", RECORD, "kbf-lease-1-1"],
        ),
        call(&[("state/inspect-fails", Some(""))], &["inspect", "x"]),
        call(
            &[("state/kill-fails", Some(""))],
            &["kill", "--signal=TERM", "x"],
        ),
        call(&[], &["unshare", "chown", "-hR", "1:1", "--", &upper, "/b"]),
        call_then_files(
            &[("state/status-override", Some("stopped 0\n"))],
            &["start", "--attach", "kbf-lease-1-1"],
        ),
        call(&[], &["unshare", "chown", "-hR", "0:0", "--", &upper]),
        call(
            &[("state/chown-fails", Some("1:1"))],
            &["unshare", "chown", "-hR", "1:1", "--"],
        ),
        call(&[], &["unshare", "chown", "-R", "1:1", "--", "/a"]),
        call(
            &[("state/rm-fails", Some(""))],
            &["rm", "--force", "kbf-lease-1-1"],
        ),
        call(
            &[
                ("state/rm-fails", None),
                // No action to kill: the pid of one that has ended may be another's now.
                ("state/pid", None),
                ("cgroup/actions/kbf-lease-1-1/cgroup.kill", Some("1\n")),
            ],
            &["rm", "--force", "kbf-lease-1-1"],
        ),
        call(
            &[("state/unshare-fails", Some(""))],
            &["unshare", "rm", "-rf", "--", &gone],
        ),
        call_then_files(
            &[
                ("state/unshare-fails", None),
                ("state/unshare-noop", Some("")),
            ],
            &["unshare", "rm", "-rf", "--", &gone],
        ),
        call(
            &[("state/unshare-noop", None)],
            &["unshare", "rm", "-rf", "--", &gone],
        ),
        // The cache's start-up question, answered by default and by the knob.
        call(
            &[("state/info-fails", None)],
            &[
                "info",
                "--format={{.Store.GraphDriverName}} {{json .Store.GraphOptions}}",
            ],
        ),
        call(
            &[("state/graph", Some("vfs {}\n"))],
            &["info", "--format={{json .Store.GraphOptions}}"],
        ),
        call(
            &[("state/chown-fails", None)],
            &["unshare", "/helper", "__chown-dirs", "1:1", "/lease/root"],
        ),
        call(
            &[("state/chown-fails", Some("0:0"))],
            &["unshare", "/helper", "__chown-dirs", "0:0", "/lease/root"],
        ),
        call_then_files(
            &[("state/chown-fails", None), ("state/busy-once", Some(""))],
            &["start", "--attach", "kbf-lease-1-1"],
        ),
        call(
            &[("state/chown-sleeps", Some("0:0"))],
            &["unshare", "chown", "-hR", "0:0", "--", &upper],
        ),
        call(&[("state/vanish-after-create", Some(""))], &create),
    ]
}

/// Lays out a fake's directory at `dir` with `program` as its `podman`, makes the
/// calls, and returns their answers and what they left.
fn run(dir: &Path, program: &Path) -> (Vec<Answer>, BTreeMap<PathBuf, Entry>) {
    let state = dir.join("state");
    for d in [
        &state,
        &dir.join("cgroup/actions/kbf-lease-1-1"),
        &dir.join("lease/root"),
    ] {
        std::fs::create_dir_all(d).expect("mkdir");
    }
    std::fs::create_dir_all(dir.join("lease/upper/sub")).expect("mkdir upper");
    std::fs::set_permissions(
        dir.join("lease/upper"),
        std::fs::Permissions::from_mode(0o751),
    )
    .expect("chmod upper");
    std::fs::write(dir.join("cgroup/actions/kbf-lease-1-1/memory.max"), "max\n").expect("max");
    // A tree `unshare rm` can delete only once it has given itself the modes back.
    std::fs::create_dir_all(dir.join("gone/locked")).expect("mkdir gone");
    std::fs::write(dir.join("gone/locked/f"), "x").expect("write gone");
    std::fs::set_permissions(
        dir.join("gone/locked"),
        std::fs::Permissions::from_mode(0o000),
    )
    .expect("chmod gone");
    std::fs::write(state.join("image-id"), "img1\n").expect("image id");
    std::fs::write(state.join("nonce"), "n:").expect("nonce");
    let link = dir.join("podman");
    std::os::unix::fs::symlink(program, &link).expect("link podman");
    support::fake::link_act(dir);
    let mut answers = Vec::new();
    for call in calls(dir) {
        for (name, contents) in call.knobs {
            match contents {
                Some(contents) => std::fs::write(dir.join(name), contents).expect("knob"),
                None => {
                    let _ = std::fs::remove_file(dir.join(name));
                }
            }
        }
        let mut command = Command::new(&link);
        let creates = call
            .args
            .as_ref()
            .and_then(|a| a.first())
            .map(String::as_str);
        if creates == Some("create") {
            command.env(HOST_VARIABLE.0, HOST_VARIABLE.1);
        }
        if let Some(args) = &call.args {
            command.arg("--cgroup-manager=cgroupfs").args(args);
        }
        let output = command.output().expect("run the fake");
        answers.push(Answer {
            args: call.args,
            code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            files: call.files_after.then(|| {
                let mut files = tree(dir);
                // The links to the fake and to the act program: their targets are
                // outside the directory.
                files.remove(Path::new("podman"));
                files.remove(Path::new(kbf_testkit::act::NAME));
                files
            }),
        });
    }
    let mut files = tree(dir);
    // The link to the act program, this test binary: its target is outside the directory.
    let act = files.remove(Path::new(kbf_testkit::act::NAME));
    assert!(
        matches!(act, Some(Entry::Link(_))),
        "no kbf-act link: {act:?}"
    );
    (answers, files)
}

/// Every entry below `dir` by relative path. The pids `start` records differ from run
/// to run, so only their presence is kept.
fn tree(dir: &Path) -> BTreeMap<PathBuf, Entry> {
    let mut entries = BTreeMap::new();
    let mut pending = vec![dir.to_owned()];
    while let Some(d) = pending.pop() {
        for entry in std::fs::read_dir(&d).expect("list").flatten() {
            let path = entry.path();
            let meta = std::fs::symlink_metadata(&path).expect("stat");
            let mode = meta.permissions().mode() & 0o7777;
            let name = path.strip_prefix(dir).expect("below dir").to_owned();
            let entry = if meta.is_symlink() {
                Entry::Link(std::fs::read_link(&path).expect("readlink"))
            } else if meta.is_dir() {
                if mode & 0o500 == 0o500 {
                    pending.push(path);
                }
                Entry::Dir(mode)
            } else {
                let bytes = std::fs::read(&path).unwrap_or_default();
                let text = match name.file_name().and_then(|n| n.to_str()) {
                    Some("pid" | "start-pid") => "<a pid>".to_owned(),
                    _ => String::from_utf8_lossy(&bytes).into_owned(),
                };
                Entry::File(mode, text)
            };
            entries.insert(name, entry);
        }
    }
    entries
}

/// The answers and files as the golden file has them: the fake's directory written
/// `<dir>`, one call per block (with the files after it, for a call that records
/// them), then one line per file below the directory.
fn render(dir: &Path, answers: &[Answer], files: &BTreeMap<PathBuf, Entry>) -> String {
    let dir = dir.to_str().expect("utf-8 path");
    let mut text = String::new();
    for answer in answers {
        let args = match &answer.args {
            Some(args) => format!("--cgroup-manager=cgroupfs {}", args.join(" ")),
            None => "(no arguments)".to_owned(),
        };
        let code = match answer.code {
            Some(code) => code.to_string(),
            None => "killed".to_owned(),
        };
        text += &format!(
            "$ podman {args}\nexit {code}\nstdout {:?}\nstderr {:?}\n",
            answer.stdout, answer.stderr
        );
        if let Some(files) = &answer.files {
            text += "files after this call\n";
            render_files(&mut text, files);
        }
    }
    text += "files\n";
    render_files(&mut text, files);
    text.replace(dir, "<dir>")
}

fn render_files(text: &mut String, files: &BTreeMap<PathBuf, Entry>) {
    for (name, entry) in files {
        let name = name.display();
        *text += &match entry {
            Entry::Dir(mode) => format!("d {mode:04o} {name}\n"),
            Entry::File(mode, bytes) => format!("f {mode:04o} {name} {bytes:?}\n"),
            Entry::Link(to) => format!("l {name} -> {}\n", to.display()),
        };
    }
}

/// Catches the fake answering a call, or writing a record, otherwise than the driver
/// suites were written against: an exit code, a line of output, a record's format
/// (`calls`, `events`, `status`, `create.args`, ...), a knob it ignores or a mode it
/// leaves differently. A change to the fake that is meant updates the golden file in
/// the same commit: on a difference the test writes what the fake gave next to its
/// scratch directory and names that file.
///
/// The controls first: a golden file updated from a run whose calls no longer reach
/// the knobs and records they are there for would pin nothing.
#[test]
fn the_fake_answers_and_records_as_its_golden_file_says() {
    // Modes are part of the records; the runners' umask is 022, a dev box's may not be.
    rustix::process::umask(rustix::fs::Mode::from_raw_mode(0o022));
    let base = support::scratch("fake-golden");
    let dir = base.join("fake");
    let (answers, mut files) = run(&dir, &support::fake::program());
    // The fake unlinks itself in the last call (`vanish-after-create`).
    let link = files.remove(Path::new("podman"));
    assert!(link.is_none(), "vanish-after-create left the podman link");
    assert!(
        answers
            .iter()
            .any(|a| a.code == Some(3) && a.stdout.contains("group 1 session 1\n")),
        "no start ran the action in a session of its own: {answers:#?}"
    );
    assert!(
        answers
            .iter()
            .any(|a| a.code == Some(3) && a.stdout.contains(HOST_VARIABLE.1)),
        "no start handed the action what create ran with (env-host): {answers:#?}"
    );
    for record in [
        "calls",
        "events",
        "status",
        "create.args",
        "create-env",
        "ps.args",
        "owner",
        "removed",
        "killed-before-rm",
    ] {
        assert!(
            files.contains_key(&Path::new("state").join(record)),
            "the fake never wrote {record}"
        );
    }
    assert!(
        !files.contains_key(Path::new("gone")),
        "unshare rm never ran"
    );
    // The upper directory is the container's after `chown 1:1`, and the `start` after
    // that must leave it unreadable: the oracle tests/userns.rs relies on to catch the
    // driver reading the outputs before it hands the overlay back.
    let after_start = answers
        .iter()
        .find_map(|a| a.files.as_ref())
        .expect("files recorded after the start the container owns");
    assert_eq!(
        after_start.get(Path::new("lease/upper")),
        Some(&Entry::Dir(0)),
        "the start after chown 1:1 left the upper directory readable"
    );
    let got = render(&dir, &answers, &files);
    let golden_path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-podman.golden");
    let golden = std::fs::read_to_string(&golden_path).unwrap_or_default();
    if got != golden {
        let actual = base.join("got.golden");
        std::fs::write(&actual, &got).expect("write what the fake gave");
        let first = got
            .lines()
            .zip(golden.lines())
            .position(|(g, w)| g != w)
            .unwrap_or_else(|| got.lines().count().min(golden.lines().count()));
        panic!(
            "the fake differs from {} from line {} on; it gave {}",
            golden_path.display(),
            first + 1,
            actual.display()
        );
    }
    support::force_remove(&base);
}
