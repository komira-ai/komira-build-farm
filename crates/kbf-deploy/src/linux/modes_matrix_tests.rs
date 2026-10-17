//! Every rule against every mode: one defect planted in each mode's rendered unit per
//! rule, and the rule must fail exactly the modes it is for and pass the others. The
//! rows say which modes each rule is for; a rule run on a mode it is not for, or not
//! run on one it is for, turns its row red.

use std::cell::RefCell;

use kbf_host::exec::Entry;
use kbf_host::step::{Owner, Perms};
use kbf_host::testkit::{FakeExec, reply};

use super::*;
use crate::linux::render::{Templates, render as render_converged};

const EXAMPLE: &str = include_str!("../../templates/linux/config.example");

fn config() -> LinuxConfig {
    LinuxConfig::parse(EXAMPLE).expect("valid settings")
}

/// Insert `line` as the first line of `[Service]`.
fn first_in_service(text: &str, line: &str) -> String {
    assert_eq!(text.matches("[Service]\n").count(), 1);
    text.replace("[Service]\n", &format!("[Service]\n{line}\n"))
}

/// Insert `line` as the last line of `[Service]` (before `[Install]`), where a
/// single-valued key wins.
fn last_in_service(text: &str, line: &str) -> String {
    assert_eq!(text.matches("\n[Install]\n").count(), 1);
    text.replace("\n[Install]\n", &format!("\n{line}\n\n[Install]\n"))
}

/// One planted defect: its rule, what it does to a mode's rendered unit, and the modes
/// the rule is for.
struct Row {
    rule: &'static str,
    plant: fn(Mode, &str) -> String,
    fails: &'static [Mode],
}

const ALL: &[Mode] = &Mode::ALL;
const BUILDS: &[Mode] = &[Mode::Daemon, Mode::StorageAndDaemon, Mode::AllInOne];
const DRAINS: &[Mode] = &[
    Mode::Frontend,
    Mode::Storage,
    Mode::StorageAndDaemon,
    Mode::AllInOne,
];

fn rows() -> Vec<Row> {
    vec![
        Row {
            rule: "render",
            plant: |_, t| format!("{t}# left @KBF_PLANTED@\n"),
            fails: ALL,
        },
        Row {
            rule: "render",
            plant: |_, t| format!("{t}# left <kbf uid>\n"),
            fails: ALL,
        },
        Row {
            rule: "continuation",
            plant: |_, t| {
                let d = t
                    .lines()
                    .find(|l| l.starts_with("Description="))
                    .expect("one");
                t.replacen(d, &format!("{d} \\\n# planted\n    more"), 1)
            },
            fails: ALL,
        },
        Row {
            rule: "exec",
            plant: |m, t| t.replacen(&format!("/kbf {m}"), &format!("/kbf-{m}"), 1),
            fails: ALL,
        },
        Row {
            rule: "slice",
            plant: |_, t| t.replace("Slice=kbf.slice", "Slice=system.slice"),
            fails: ALL,
        },
        Row {
            rule: "delegate",
            plant: |_, t| first_in_service(t, "DisableControllers=cpuset"),
            fails: BUILDS,
        },
        Row {
            rule: "delegate",
            plant: |_, t| last_in_service(t, "Delegate="),
            fails: BUILDS,
        },
        // A build node given the key; the storage unit already reads it, and so does
        // storage-and-daemon, by the one file it is allowed.
        Row {
            rule: "credential",
            plant: |_, t| first_in_service(t, "EnvironmentFile=-/etc/kbf/s3.env"),
            fails: BUILDS,
        },
        Row {
            rule: "credential",
            plant: |_, t| first_in_service(t, "EnvironmentFile=/etc/kbf/*.env"),
            fails: BUILDS,
        },
        Row {
            rule: "credential",
            plant: |_, t| first_in_service(t, "Environment=AWS_ACCESS_KEY_ID=planted"),
            fails: BUILDS,
        },
        // The key's holder run as the user builds run as, or as root.
        Row {
            rule: "credential",
            plant: |_, t| last_in_service(t, "User=kbf"),
            fails: &[Mode::Storage],
        },
        Row {
            rule: "credential",
            plant: |_, t| last_in_service(t, "User="),
            fails: &[Mode::Storage],
        },
        Row {
            rule: "storage",
            plant: |_, t| match t.lines().find(|l| l.contains("--cold-store=")) {
                Some(l) => t.replace(&format!("{l}\n"), ""),
                None => t.to_owned(),
            },
            fails: &[Mode::Storage, Mode::StorageAndDaemon],
        },
        Row {
            rule: "storage",
            plant: |_, t| t.replace("EnvironmentFile=/etc/kbf/s3.env\n", ""),
            fails: &[Mode::Storage, Mode::StorageAndDaemon],
        },
        Row {
            rule: "state-store",
            // The frontend's unit gives a state store already (the last one wins): put the
            // Postgres URI in its place; the other modes get it as a new flag.
            plant: |m, t| {
                if t.contains("--state-store=sqlite://") {
                    t.replace(
                        "--state-store=sqlite://",
                        "--state-store=postgres://db/kbf#",
                    )
                } else {
                    t.replacen(
                        &format!("/kbf {m}"),
                        &format!("/kbf {m} --state-store=postgres://db/kbf"),
                        1,
                    )
                }
            },
            fails: ALL,
        },
        // A drain cut short: a finite stop timeout, or a signal kbf does not drain on.
        Row {
            rule: "stop",
            plant: |_, t| last_in_service(t, "TimeoutStopSec=90"),
            fails: DRAINS,
        },
        Row {
            rule: "stop",
            plant: |_, t| last_in_service(t, "KillSignal=SIGKILL"),
            fails: DRAINS,
        },
    ]
}

/// Catches a rule run on a mode it is not for (a storage unit's own key refused, a
/// frontend asked to delegate) and one not run on a mode it is for (a build node given
/// the key, a storage unit without its cold store or run as the builds' user, a
/// Postgres state store in any mode's unit).
#[test]
fn each_rule_fails_exactly_the_modes_it_is_for() {
    let c = config();
    let mut wrong = Vec::new();
    for (i, row) in rows().iter().enumerate() {
        for mode in Mode::ALL {
            let text = render(&c, Some(1500), mode).expect("renders");
            let planted = (row.plant)(mode, &text);
            if planted == text {
                // A defect with nothing to plant on (no line to drop) must not be one
                // of the modes the rule is for.
                assert!(
                    !row.fails.contains(&mode),
                    "row {i} ({}) planted nothing in {mode}",
                    row.rule
                );
                continue;
            }
            let r = check(&c, mode, &planted, true).expect("a unit");
            let got: Vec<&str> = r.failures.iter().map(|f| f.rule).collect();
            let want: Vec<&str> = if !row.fails.contains(&mode) {
                Vec::new()
            } else {
                vec![row.rule]
            };
            if got != want {
                wrong.push(format!("row {i} ({}) on {mode}: {:?}", row.rule, lines(&r)));
            }
        }
    }
    assert_eq!(wrong, Vec::<String>::new());
}

fn lines(r: &Report) -> Vec<String> {
    r.failures.iter().map(ToString::to_string).collect()
}

fn file(bits: u32, uid: u32, text: &str) -> Entry {
    Entry::File {
        perms: Perms::new(bits),
        owner: Owner { uid, gid: uid },
        bytes: text.as_bytes().to_vec(),
    }
}

/// A node with kbf installed, and a key file `key`; kbf refuses every command line
/// (`check-args` exits 2) and `getent passwd kbf-server` is uid 990.
fn node(key: Entry) -> FakeExec {
    let mut exec = FakeExec::new(0);
    let at = |p: &str| HostPath::new(p).expect("absolute");
    exec.entries
        .insert(at("/usr/local/bin/kbf"), file(0o755, 0, ""));
    exec.entries.insert(at("/etc/kbf/s3.env"), key);
    let asked = RefCell::new(Vec::new());
    exec.answer = Box::new(move |cmd: &Cmd| {
        asked.borrow_mut().push(cmd.to_string());
        Ok(match cmd.program.as_str() {
            "/usr/bin/getent" => reply(
                0,
                "kbf-server:x:990:990::/var/lib/kbf-server:/sbin/nologin\n",
                "",
            ),
            "/usr/bin/id" => reply(0, "990\n", ""),
            _ => reply(2, "", "error: planted refusal\n"),
        })
    });
    exec
}

/// The rules that read the node, per mode: `parse` runs kbf for every mode;
/// `credential` reads the key file's owner and mode only for the units that hold it
/// (a 0640 key or one another user owns fails storage and storage-and-daemon, and no
/// other; a key the server user owns passes storage and fails storage-and-daemon, whose
/// key is root's alone).
/// Catches each of these run on the wrong modes or on none.
#[test]
fn the_node_rules_read_the_node_for_exactly_the_modes_they_are_for() {
    let c = config();
    let slice = UnitText::new(
        SLICE,
        render_converged(&c, Some(1500), &Templates::BUILT_IN)
            .expect("renders")
            .slice,
    );
    let cases: [(&str, Entry, &[Mode]); 4] = [
        ("parse", file(0o600, 0, ""), &Mode::ALL),
        (
            "credential",
            file(0o640, 0, ""),
            &[Mode::Storage, Mode::StorageAndDaemon],
        ),
        (
            "credential",
            file(0o600, 1000, ""),
            &[Mode::Storage, Mode::StorageAndDaemon],
        ),
        (
            "credential",
            file(0o600, 990, ""),
            &[Mode::StorageAndDaemon],
        ),
    ];
    let mut wrong = Vec::new();
    for (rule, key, fails) in cases {
        let exec = node(key);
        for mode in Mode::ALL {
            let text = render(&c, Some(1500), mode).expect("renders");
            let unit = UnitText::new(mode.unit_name(), text);
            let r = check_on_host(&exec, &c, mode, &unit, &slice, true, None).expect("runs");
            let hits = r.failures.iter().filter(|f| f.rule == rule).count();
            let want = usize::from(fails.contains(&mode));
            if hits != want {
                wrong.push(format!("{rule} on {mode}: {:?}", lines(&r)));
            }
            // No other node rule fires on the clean node (`parse` aside, which this kbf
            // refuses).
            let others: Vec<String> = r
                .failures
                .iter()
                .filter(|f| f.rule != rule && f.rule != "parse")
                .map(ToString::to_string)
                .collect();
            if !others.is_empty() {
                wrong.push(format!("{rule} on {mode}, other rules: {others:?}"));
            }
        }
    }
    assert_eq!(wrong, Vec::<String>::new());
}
