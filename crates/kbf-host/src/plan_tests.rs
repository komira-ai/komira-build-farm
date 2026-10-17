use std::time::Duration;

use super::*;
use crate::step::Perms;
use crate::testkit::{FakeExec, RUNNER};

const ROOT: Option<Owner> = Some(Owner { uid: 0, gid: 0 });
const P644: Perms = Perms::new(0o644);
const P755: Perms = Perms::new(0o755);

fn p(s: &str) -> HostPath {
    HostPath::new(s).expect("valid test path")
}

/// The plan every test below applies: a directory, a file in it, a removal, a command
/// and a wait, so each kind of step is covered.
fn kit(exec: &FakeExec) -> Plan {
    let mut planner = Planner::new(exec);
    planner.dir(p("/etc/kbf"), P755, ROOT).expect("plan");
    planner
        .file(p("/etc/kbf/a.env"), P644, ROOT, b"A=1\n".to_vec())
        .expect("plan");
    planner.absent(p("/etc/kbf/old.env")).expect("plan");
    planner.run(Cmd::new(p("/usr/bin/systemctl")).arg("daemon-reload"));
    planner.wait_http(
        HttpUrl::new("http://127.0.0.1:8982/readyz").expect("url"),
        Duration::from_secs(5),
    );
    planner.finish()
}

fn text(out: &[u8]) -> &str {
    std::str::from_utf8(out).expect("utf-8")
}

/// Catches a dry run that changes the host (the default must never act) and one that
/// prints anything but `DRY-RUN:` lines and the closing note.
#[test]
fn a_dry_run_prints_every_step_and_changes_nothing() {
    let mut exec = FakeExec::new(501);
    exec.entries.insert(p("/etc/kbf/old.env"), Entry::Other);
    let plan = kit(&exec);
    let mut out = Vec::new();
    execute(&plan, RunAs::Root, Mode::DryRun, &mut exec, &mut out).expect("dry run");
    assert_eq!(exec.log, Vec::<Step>::new(), "a dry run applied a step");
    assert_eq!(
        text(&out),
        "DRY-RUN: mkdir /etc/kbf (0755, owner 0:0)\n\
         DRY-RUN: write /etc/kbf/a.env (0644, owner 0:0, 4 bytes)\n\
         DRY-RUN: remove /etc/kbf/old.env\n\
         DRY-RUN: run /usr/bin/systemctl daemon-reload\n\
         DRY-RUN: wait up to 5s for http://127.0.0.1:8982/readyz\n\
         dry run: nothing changed. Re-run with --apply.\n"
    );
}

/// Catches an apply that skips, reorders or repeats a step, or does not print the line
/// before it acts.
#[test]
fn apply_does_each_step_in_order_and_prints_it_first() {
    let mut exec = FakeExec::new(0);
    exec.entries.insert(p("/etc/kbf/old.env"), Entry::Other);
    let plan = kit(&exec);
    let mut out = Vec::new();
    execute(&plan, RunAs::Root, Mode::Apply, &mut exec, &mut out).expect("apply");
    assert_eq!(exec.log, plan.steps());
    let lines: Vec<String> = plan.steps().iter().map(|s| format!("+ {s}\n")).collect();
    assert_eq!(text(&out), lines.concat());
}

/// Catches a planner that adds a step for something the host already has, so a second
/// run would rewrite files and restart services for nothing. Every kind of check
/// (file bytes, permissions, owner; directory; absence) is exercised.
#[test]
fn a_rerun_after_an_apply_plans_nothing() {
    let mut exec = FakeExec::new(0);
    exec.entries.insert(p("/etc/kbf/old.env"), Entry::Other);
    let mut planner = Planner::new(&exec);
    planner.dir(p("/etc/kbf"), P755, ROOT).expect("plan");
    planner
        .file(p("/etc/kbf/a.env"), P644, ROOT, b"A=1\n".to_vec())
        .expect("plan");
    planner
        .file(p("/etc/kbf/b.env"), P644, None, b"B=1\n".to_vec())
        .expect("plan");
    planner.absent(p("/etc/kbf/old.env")).expect("plan");
    let plan = planner.finish();
    assert_eq!(plan.steps().len(), 4);
    execute(&plan, RunAs::Root, Mode::Apply, &mut exec, &mut Vec::new()).expect("apply");

    let mut again = Planner::new(&exec);
    again.dir(p("/etc/kbf"), P755, ROOT).expect("plan");
    again
        .file(p("/etc/kbf/a.env"), P644, ROOT, b"A=1\n".to_vec())
        .expect("plan");
    again
        .file(p("/etc/kbf/b.env"), P644, None, b"B=1\n".to_vec())
        .expect("plan");
    again.absent(p("/etc/kbf/old.env")).expect("plan");
    let again = again.finish();
    assert_eq!(again, Plan::default(), "the re-run planned steps");
    let mut out = Vec::new();
    execute(&again, RunAs::Root, Mode::DryRun, &mut exec, &mut out).expect("dry run");
    assert_eq!(text(&out), "nothing to do\n");
}

/// Catches a planner that compares only part of what it wants: each case changes one
/// property of a file or directory already on the host, and that alone must plan the
/// step again.
#[test]
fn any_difference_plans_the_step_again() {
    let file = |perms, owner, bytes: &[u8]| Entry::File {
        perms,
        owner,
        bytes: bytes.to_vec(),
    };
    let dir = |perms, owner| Entry::Dir { perms, owner };
    let other_owner = Owner { uid: 0, gid: 20 };
    let files = [
        file(P644, Owner { uid: 0, gid: 0 }, b"A=2\n"),
        file(Perms::new(0o600), Owner { uid: 0, gid: 0 }, b"A=1\n"),
        file(P644, other_owner, b"A=1\n"),
        dir(P644, Owner { uid: 0, gid: 0 }),
        Entry::Other,
        Entry::Link {
            target: "/a.real".into(),
        },
    ];
    for have in files {
        let mut exec = FakeExec::new(0);
        exec.entries.insert(p("/a"), have.clone());
        let mut planner = Planner::new(&exec);
        planner
            .file(p("/a"), P644, ROOT, b"A=1\n".to_vec())
            .expect("plan");
        assert_eq!(planner.finish().steps().len(), 1, "{have:?}");
    }
    let dirs = [
        dir(Perms::new(0o700), Owner { uid: 0, gid: 0 }),
        dir(P755, other_owner),
        file(P755, Owner { uid: 0, gid: 0 }, b""),
    ];
    for have in dirs {
        let mut exec = FakeExec::new(0);
        exec.entries.insert(p("/d"), have.clone());
        let mut planner = Planner::new(&exec);
        planner.dir(p("/d"), P755, ROOT).expect("plan");
        assert_eq!(planner.finish().steps().len(), 1, "{have:?}");
    }
    // `owner: None` takes whatever owner the host has.
    let mut exec = FakeExec::new(501);
    exec.entries.insert(p("/d"), dir(P755, RUNNER));
    let mut planner = Planner::new(&exec);
    planner.dir(p("/d"), P755, None).expect("plan");
    planner.absent(p("/gone")).expect("plan");
    assert!(planner.finish().is_empty());
}

/// Catches a root plan applied by a user (it would half-apply and fail on the first
/// root-owned file) and a user plan applied as root (it would leave root-owned files
/// in a user's home). Neither may apply a single step.
#[test]
fn apply_refuses_the_wrong_user_before_any_step() {
    let mut user = FakeExec::new(501);
    let plan = kit(&user);
    let err = execute(&plan, RunAs::Root, Mode::Apply, &mut user, &mut Vec::new());
    assert!(
        matches!(err, Err(HostError::NotRoot { euid: 501 })),
        "{err:?}"
    );
    assert_eq!(user.log, Vec::<Step>::new());

    let mut root = FakeExec::new(0);
    let err = execute(&plan, RunAs::User, Mode::Apply, &mut root, &mut Vec::new());
    assert!(matches!(err, Err(HostError::IsRoot)), "{err:?}");
    assert_eq!(root.log, Vec::<Step>::new());

    // A dry run is allowed as anyone: it changes nothing.
    execute(&plan, RunAs::Root, Mode::DryRun, &mut user, &mut Vec::new()).expect("dry run");
    execute(&plan, RunAs::User, Mode::Apply, &mut user, &mut Vec::new()).expect("user apply");
    assert_eq!(user.log.len(), plan.steps().len());
}

/// Catches applying as root from a binary a user could have replaced.
#[test]
fn apply_as_root_refuses_an_untrusted_binary() {
    let mut exec = FakeExec::new(0);
    exec.untrusted = true;
    let plan = kit(&exec);
    let err = execute(&plan, RunAs::Root, Mode::Apply, &mut exec, &mut Vec::new());
    let msg = err.err().map(|e| e.to_string()).unwrap_or_default();
    assert_eq!(
        msg,
        "refusing to run as root from a binary root does not own alone: \
         /opt/downloads/kbf-deploy is writable by group or others (mode 0775)"
    );
    assert_eq!(exec.log, Vec::<Step>::new());
}

/// Catches an apply that carries on after a failed step (later steps would run on a
/// half-changed host) and an error that does not say which step failed.
#[test]
fn apply_stops_at_the_first_failed_step() {
    let mut exec = FakeExec::new(0);
    exec.fail_at = Some(1);
    let plan = kit(&exec);
    let err = execute(&plan, RunAs::Root, Mode::Apply, &mut exec, &mut Vec::new());
    let msg = err.err().map(|e| e.to_string()).unwrap_or_default();
    assert_eq!(
        msg,
        "step 1 (write /etc/kbf/a.env (0644, owner 0:0, 4 bytes)) failed: planted failure"
    );
    assert_eq!(exec.log.len(), 2, "a step ran after the failed one");
}

/// Catches an unreadable path being planned as absent (a file the kit cannot see would
/// be written or kept blindly).
#[test]
fn an_unreadable_path_stops_the_plan() {
    let exec = FakeExec::new(501);
    let mut planner = Planner::new(&exec);
    let errs = [
        planner.file(p("/unreadable"), P644, None, Vec::new()),
        planner.dir(p("/unreadable"), P755, None),
        planner.absent(p("/unreadable")),
    ];
    for err in errs {
        let msg = err.err().map(|e| e.to_string()).unwrap_or_default();
        assert_eq!(msg, "cannot read /unreadable: permission denied");
    }
    assert!(planner.finish().is_empty());
}

/// A writer that takes `left` bytes, then fails.
struct FailAfter {
    left: usize,
}

impl io::Write for FailAfter {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        if b.len() > self.left {
            return Err(io::Error::from(io::ErrorKind::BrokenPipe));
        }
        self.left -= b.len();
        Ok(b.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Catches an apply that acts on a step it could not print (the operator would not see
/// what changed), and a printing error being dropped, for a step line, the closing
/// note and the empty plan's line.
#[test]
fn a_line_that_cannot_be_printed_stops_the_run() {
    let mut exec = FakeExec::new(0);
    let plan = kit(&exec);
    for mode in [Mode::DryRun, Mode::Apply] {
        let err = execute(
            &plan,
            RunAs::Root,
            mode,
            &mut exec,
            &mut FailAfter { left: 0 },
        );
        assert!(matches!(err, Err(HostError::Output(_))), "{mode:?}");
    }
    assert_eq!(exec.log, Vec::<Step>::new());

    let empty = Plan::default();
    let err = execute(
        &empty,
        RunAs::Root,
        Mode::DryRun,
        &mut exec,
        &mut FailAfter { left: 0 },
    );
    assert!(matches!(err, Err(HostError::Output(_))));

    let mut planner = Planner::new(&exec);
    planner.run(Cmd::new(p("/usr/bin/true")));
    let one = planner.finish();
    let mut w = FailAfter {
        left: "DRY-RUN: run /usr/bin/true\n".len(),
    };
    let err = execute(&one, RunAs::Root, Mode::DryRun, &mut exec, &mut w);
    assert!(
        matches!(err, Err(HostError::Output(_))),
        "the note's error was dropped"
    );
    assert_eq!(w.left, 0);
    io::Write::flush(&mut w).expect("flush");
}

/// Catches a planner that reports a step it did not plan, or hides one it did; kits
/// plan `daemon-reload` and restarts from these answers.
#[test]
fn the_planner_says_whether_it_planned_a_step() {
    let mut exec = FakeExec::new(0);
    exec.entries.insert(
        p("/d"),
        Entry::Dir {
            perms: P755,
            owner: RUNNER,
        },
    );
    exec.entries.insert(
        p("/f"),
        Entry::File {
            perms: P644,
            owner: RUNNER,
            bytes: b"x".to_vec(),
        },
    );
    let mut planner = Planner::new(&exec);
    assert!(!planner.dir(p("/d"), P755, None).expect("plan"));
    assert!(planner.dir(p("/e"), P755, None).expect("plan"));
    assert!(
        !planner
            .file(p("/f"), P644, None, b"x".to_vec())
            .expect("plan")
    );
    assert!(
        planner
            .file(p("/f"), P644, None, b"y".to_vec())
            .expect("plan")
    );
    assert!(planner.absent(p("/f")).expect("plan"));
    assert!(!planner.absent(p("/g")).expect("plan"));
    assert_eq!(planner.steps().len(), 3);
    assert_eq!(planner.finish().steps().len(), 3);
}

/// Catches a link planner that plans a flip when the link already points at the target
/// (every deploy would rewrite `current` for nothing, and a re-plan after an apply would
/// not be empty), one that keeps a link pointing elsewhere, and one that plans a step
/// over a file, directory or socket (which the step would refuse half-way through an
/// apply) instead of refusing before any step.
///
/// Mutants seen red: `done` always false; a non-link planned as a flip.
#[test]
fn a_link_is_planned_only_when_it_points_elsewhere_or_is_missing() {
    let mut exec = FakeExec::new(0);
    let want = std::path::PathBuf::from("releases/b");
    let plan = |exec: &FakeExec| {
        let mut planner = Planner::new(exec);
        let planned = planner
            .symlink(p("/kbf/current"), want.clone())
            .expect("plan");
        (planned, planner.finish())
    };
    let (planned, missing) = plan(&exec);
    let flip = Step::Symlink {
        path: p("/kbf/current"),
        target: want.clone(),
    };
    assert!(planned);
    assert_eq!(missing.steps(), std::slice::from_ref(&flip));
    exec.entries.insert(
        p("/kbf/current"),
        Entry::Link {
            target: "releases/a".into(),
        },
    );
    assert_eq!(plan(&exec).1.steps(), [flip]);
    execute(
        &missing,
        RunAs::Root,
        Mode::Apply,
        &mut exec,
        &mut Vec::new(),
    )
    .expect("apply");
    let (planned, again) = plan(&exec);
    assert!(!planned && again.is_empty(), "the re-plan was {again:?}");

    let others = [
        Entry::File {
            perms: P644,
            owner: RUNNER,
            bytes: Vec::new(),
        },
        Entry::Dir {
            perms: P755,
            owner: RUNNER,
        },
        Entry::Other,
    ];
    for have in others {
        let mut exec = FakeExec::new(0);
        exec.entries.insert(p("/kbf/current"), have.clone());
        let err = Planner::new(&exec)
            .symlink(p("/kbf/current"), want.clone())
            .expect_err("refused");
        assert!(
            matches!(&err, HostError::NotALink { path } if *path == p("/kbf/current")),
            "{have:?}: {err:?}"
        );
        assert_eq!(
            err.to_string(),
            "refusing to replace /kbf/current with a symbolic link: it is not one"
        );
    }
}
