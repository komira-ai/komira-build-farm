use std::process::ExitCode;

use clap::Parser;
use kbf_host::testkit::scratch;

use super::*;
use crate::rollout::fleet_tests::{Bad, Call, Fleet, commit};

#[derive(Parser)]
struct Top {
    #[command(flatten)]
    args: RolloutArgs,
}

const INVENTORY: &str = "front frontend\ns1 storage-and-daemon\nm1 daemon-mac\n";
const NEW: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

fn parse(args: &[&str]) -> Result<RolloutArgs, clap::Error> {
    Top::try_parse_from(std::iter::once("rollout").chain(args.iter().copied())).map(|t| t.args)
}

/// An inventory file in a scratch directory, and the flags that name it.
fn flags(name: &str, inventory: &str, extra: &[&str]) -> Vec<String> {
    let dir = scratch(name);
    let path = dir.join("inventory");
    std::fs::write(&path, inventory).expect("write the inventory");
    let mut flags: Vec<String> = [
        "--inventory",
        path.to_str().expect("utf-8 path"),
        "--commit",
        NEW,
        "--run-id",
        "run-1",
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect();
    flags.extend(extra.iter().map(|s| (*s).to_owned()));
    flags
}

fn fleet() -> Fleet {
    Fleet::new(
        &Inventory::parse(INVENTORY).expect("inventory"),
        &commit('a'),
    )
}

fn go(flags: &[String], fleet: Option<&Fleet>) -> (u8, String, String) {
    let refs: Vec<&str> = flags.iter().map(String::as_str).collect();
    let args = parse(&refs).expect("valid flags");
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let mut transport = fleet.cloned();
    let dynamic = transport.as_mut().map(|f| f as &mut dyn Transport);
    let code = run(args, dynamic, &mut out, &mut err);
    let code = [0, 1, 2, 3]
        .into_iter()
        .find(|c| ExitCode::from(*c) == code)
        .expect("an exit code of the command");
    (
        code,
        String::from_utf8(out).expect("utf-8"),
        String::from_utf8(err).expect("utf-8"),
    )
}

/// Catches a dry run that touches the fleet: it prints the plan, ends with the dry-run
/// note, and makes no call at all (not the lock, not a poll), even with a transport.
#[test]
fn a_dry_run_prints_the_plan_and_touches_nothing() {
    let f = fleet();
    let flags = flags("rollout-dry", INVENTORY, &[]);
    let (code, out, err) = go(&flags, Some(&f));
    assert_eq!((code, err.as_str()), (0, ""));
    assert_eq!(f.events(), vec![]);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines[0], "DRY-RUN: lock the fleet as run run-1");
    assert_eq!(lines[1], "DRY-RUN: front: back up the state store");
    assert_eq!(lines[2], "DRY-RUN: s1: require no under-replicated blob");
    assert_eq!(lines[lines.len() - 2], "DRY-RUN: unlock the fleet");
    assert_eq!(
        lines[lines.len() - 1],
        "dry run: nothing changed. Re-run with --apply."
    );
    // The same without any transport: a dry run needs none.
    let (code, again, _) = go(&flags, None);
    assert_eq!((code, again), (0, out));
}

/// Catches an `--apply` with nothing to carry it out being taken as done.
#[test]
fn apply_without_a_transport_is_refused() {
    let flags = flags("rollout-none", INVENTORY, &["--apply"]);
    let (code, out, err) = go(&flags, None);
    assert_eq!(code, 2);
    assert_eq!(out, "");
    assert_eq!(
        err,
        "REFUSED: this build has no transport to the hosts, so --apply cannot run\n"
    );
}

/// Catches an apply whose result is not the exit code and the lines the command line
/// promises, for each way a rollout can end.
#[test]
fn an_apply_exits_by_how_the_rollout_ended() {
    // Done.
    let f = fleet();
    let flags_ok = flags("rollout-done", INVENTORY, &["--apply"]);
    let (code, out, err) = go(&flags_ok, Some(&f));
    assert_eq!((code, err.as_str()), (0, ""));
    assert!(out.starts_with("+ lock the fleet as run run-1\n"), "{out}");
    assert!(
        out.ends_with(&format!("rollout done: 3 hosts are on {NEW}\n")),
        "{out}"
    );
    assert_eq!(f.current("m1"), commit('b'));

    // A host failed: its rollback is in the message.
    let f = fleet();
    f.bad("s1", Bad::NeverRegisters);
    let (code, _, err) = go(&flags_ok, Some(&f));
    assert_eq!(code, 1);
    assert!(
        err.starts_with("FAIL s1: health: not done within 120 s")
            && err.ends_with("; the host was rolled back and undrained\n"),
        "{err}"
    );

    // Busy: pending, with nothing wrong.
    let f = fleet();
    f.busy_forever("front");
    let flags_idle = flags(
        "rollout-idle",
        INVENTORY,
        &["--apply", "--idle-timeout", "10"],
    );
    let (code, out, _) = go(&flags_idle, Some(&f));
    assert_eq!(code, 3);
    assert!(
        out.ends_with("rollout pending: front: work in flight: not done within 10 s (3 still in flight); the host was left as it was\n"),
        "{out}"
    );
    // The flag is the limit: the wait ended at 10 s, not at the 6 hour default.
    assert_eq!(f.elapsed(), std::time::Duration::from_secs(10));

    // Locked: refused.
    let f = fleet();
    f.hold_lock("other");
    let (code, _, err) = go(&flags_ok, Some(&f));
    assert_eq!(code, 2);
    assert_eq!(err, "REFUSED: the fleet is locked by run other\n");
}

/// Catches a pause file that is not read: while it exists the rollout stops before the
/// next host (here, before the first), and with the file gone it runs.
#[test]
fn the_pause_file_stops_the_rollout_between_hosts() {
    let dir = scratch("rollout-pause");
    let pause = dir.join("pause");
    std::fs::write(&pause, "").expect("write the pause file");
    let pause = pause.to_str().expect("utf-8").to_owned();
    let f = fleet();
    let paused = flags(
        "rollout-paused",
        INVENTORY,
        &["--apply", "--pause-file", &pause],
    );
    let (code, out, _) = go(&paused, Some(&f));
    assert_eq!(code, 3);
    assert_eq!(
        out,
        "rollout paused: 0 of 3 hosts done, the rest untouched\n"
    );
    assert_eq!(f.events(), vec![]);

    std::fs::remove_file(&pause).expect("remove the pause file");
    let (code, _, _) = go(&paused, Some(&f));
    assert_eq!(code, 0);
    assert!(f.calls_to("s1").contains(&Call::Undrain));
}

/// Catches an inventory or a flag that is wrong being run anyway.
#[test]
fn a_bad_inventory_file_or_flag_is_refused_before_anything_runs() {
    let f = fleet();
    let missing = flags("rollout-missing", INVENTORY, &["--apply"])
        .into_iter()
        .map(|a| {
            if a.ends_with("/inventory") {
                format!("{a}.nope")
            } else {
                a
            }
        })
        .collect::<Vec<_>>();
    let (code, _, err) = go(&missing, Some(&f));
    assert_eq!(code, 2);
    assert!(err.starts_with("REFUSED: cannot read "), "{err}");

    let no_frontend = flags("rollout-nofront", "s1 daemon\n", &["--apply"]);
    let (code, _, err) = go(&no_frontend, Some(&f));
    assert_eq!(code, 2);
    assert!(
        err.ends_with("the inventory needs exactly one frontend, and has 0\n"),
        "{err}"
    );
    assert_eq!(f.events(), vec![]);

    // Bad flags never reach the command.
    for bad in [
        ["--commit", "main"],
        ["--commit", "ABCDEFABCDEFABCDEFABCDEFABCDEFABCDEFABCD"],
        ["--run-id", "a b"],
    ] {
        let mut args = vec!["--inventory", "/dev/null"];
        if bad[0] != "--commit" {
            args.extend(["--commit", NEW]);
        }
        if bad[0] != "--run-id" {
            args.extend(["--run-id", "r"]);
        }
        args.extend(bad);
        assert!(parse(&args).is_err(), "{bad:?}");
    }
}
