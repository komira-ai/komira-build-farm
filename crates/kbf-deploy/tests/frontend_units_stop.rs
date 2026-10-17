//! A unit that runs a frontend must not be killed by systemd while it drains.
//!
//! SIGTERM (SIGINT for the pilot unit) begins a drain: new Execute calls are refused and
//! the open streams run until their action is done, which Buck2 needs because it does not
//! recover a stream that breaks. A stop timeout (systemd's default is 90 s, the pilot
//! template had 20) SIGKILLs the process in the middle and fails every action in flight.
//! So every unit template that starts a frontend carries `TimeoutStopSec=infinity`, and
//! the example in `docs/deploy/storage-node.md` shows it.

use std::path::{Path, PathBuf};

/// The program (its file name) and first argument an `ExecStart=` starts when the unit
/// runs a frontend: the server binary, the `kbf` frontend and all-in-one modes, and the
/// pilot unit's placeholder for the server command it renders. Matched word by word, not
/// as a substring: the storage unit's `--meta-dir=/var/lib/kbf-server/raft`
/// names `kbf-server` too.
const FRONTENDS: [(&str, Option<&str>); 4] = [
    ("kbf-server", None),
    ("kbf", Some("frontend")),
    ("kbf", Some("all-in-one")),
    ("@EXEC_START@", None),
];

const STOP: &str = "TimeoutStopSec=infinity";

fn manifest() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Every `.service` file under `dir`, recursively.
fn services(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("a directory") {
        let path = entry.expect("an entry").path();
        if path.is_dir() {
            services(&path, out);
        } else if path.extension().is_some_and(|e| e == "service") {
            out.push(path);
        }
    }
}

/// Whether the `ExecStart=` of `unit` starts a frontend ([`FRONTENDS`]).
fn starts_frontend(unit: &str) -> bool {
    let Some(start) = unit.lines().find_map(|l| l.strip_prefix("ExecStart=")) else {
        return false;
    };
    let mut words = start.split_whitespace();
    let program = words.next().unwrap_or_default();
    let name = program.rsplit('/').next().unwrap_or(program);
    let first = words.next();
    FRONTENDS
        .iter()
        .any(|&(p, arg)| p == name && arg.is_none_or(|a| first == Some(a)))
}

/// Catches a frontend unit template (or its golden) that lets systemd kill a drain: the
/// pilot template's `TimeoutStopSec=20`, a template with no stop timeout set (the
/// default is 90 s), and a new frontend unit added without it. Mutants: the line removed
/// from any of the four templates, or set to a number. The expected four also keep this
/// from passing with nothing to check.
#[test]
fn every_frontend_unit_template_waits_for_its_drain() {
    let mut files = Vec::new();
    services(&manifest().join("templates"), &mut files);
    services(&manifest().join("tests/golden/linux"), &mut files);
    let mut frontends = Vec::new();
    for file in files {
        let unit = std::fs::read_to_string(&file).expect("a unit");
        if !starts_frontend(&unit) {
            continue;
        }
        let stops: Vec<&str> = unit
            .lines()
            .filter(|l| l.starts_with("TimeoutStopSec="))
            .collect();
        assert_eq!(stops, [STOP], "{}", file.display());
        frontends.push(
            file.file_name()
                .expect("a name")
                .to_string_lossy()
                .into_owned(),
        );
    }
    frontends.sort();
    // Each template and, for the linux ones, its golden.
    assert_eq!(
        frontends,
        [
            "kbf-all-in-one.service",
            "kbf-all-in-one.service",
            "kbf-frontend@.service",
            "kbf-frontend@.service",
            "kbf-pilot-server.service",
            "kbf-server.service",
            "kbf-server.service",
        ]
    );
}

/// Catches the deploy doc's example server unit showing a stop timeout (or none) that
/// would kill a drain.
#[test]
fn the_documented_server_unit_waits_for_its_drain() {
    let doc = std::fs::read_to_string(manifest().join("../../docs/deploy/storage-node.md"))
        .expect("the doc");
    let from = doc
        .find("ExecStart=/usr/local/bin/kbf-server")
        .expect("the server unit");
    let unit = &doc[from..];
    let unit = &unit[..unit.find("```").expect("the end of the block")];
    let stops: Vec<&str> = unit
        .lines()
        .filter(|l| l.starts_with("TimeoutStopSec="))
        .collect();
    assert_eq!(stops, [STOP]);
}
