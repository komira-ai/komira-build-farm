//! Workspace layering: the pure crates stay pure.
//!
//! The pure crates hold the farm's decision logic and must replay a simulation seed
//! exactly, so they may not reach an async runtime, the network or a random source.
//! This test reads the resolved dependency graph from `cargo metadata` and walks each
//! pure crate's normal (non-dev, non-build) dependencies transitively.
//!
//! Catches:
//! - a pure crate that depends, directly or through another crate, on a forbidden
//!   crate (tokio, tonic, hyper, reqwest, the rand family, getrandom, mio, socket2);
//! - a pure crate that depends on a kbf crate outside the pure set;
//! - a pure crate whose `lib.rs` no longer denies the `clippy.toml` lists, which would
//!   silently switch off the clock, sleep, network and `HashMap` checks for it;
//! - a pure crate renamed or removed, which would otherwise make every check above
//!   pass vacuously;
//! - a pure module of an impure crate (`PURE_MODULES`) whose file no longer denies
//!   the `clippy.toml` lists, or that was moved or removed;
//! - the server and the worker daemon linked together outside the one crate meant to
//!   (`JOINERS`): `kbf-server` and `kbf-node` are libraries so a combined binary can
//!   run both in one process, and any other crate that links both, or either one into
//!   the other's side of the farm, makes the split meaningless;
//! - a `main.rs` of those two that grows past `THIN_MAIN_LINES` code lines, which
//!   means assembly that a combined binary could not reuse is back in a binary;
//! - the storage crate (`kbf-storage`) depending, directly or through another crate, on
//!   a frontend crate (`STORAGE_MUST_NOT_REACH`: the scheduler and authorization), which
//!   would make a storage process link the frontend; and the storage crate renamed or
//!   removed, which would make that check pass vacuously;
//! - the routing library (`kbf-route`) depending, directly or through another crate, on a
//!   frontend crate, the server, or the daemon's side (`ROUTE_MUST_NOT_REACH`): the
//!   frontend and the daemon both link it, and neither may pull in the other's code
//!   through it;
//! - a daemon crate (`DAEMON_ONLY`: `kbf-node`, the `kbf-daemon` binary's library, and
//!   `kbf-daemon` beneath it) depending, directly or through another crate, on the object
//!   store client, a crate that holds one, or a standalone HTTP client one could be
//!   written over (`COLD_STORE_CLIENT`); and a walk that cannot see the client (the
//!   client crate renamed, or the server no longer reaching it), which would make that
//!   check pass vacuously. A daemon-only node holds no cold-store credential and reaches
//!   bytes only through storage (docs/design/architecture-v2.md, section 9). What this
//!   pins is the daemon's code: the daemon crates cannot name an object-store client, so
//!   the separate `kbf-daemon` binary links none. It does not pin the binary a
//!   daemon-only node runs. The deployed `kbf daemon` is the `kbf` binary (`kbf-cli`),
//!   which links `kbf-server` and `kbf-objstore` for its other modes (`JOINERS`). For
//!   that binary the protection is that it is given no credential: its unit loads no
//!   environment file or systemd credential (kbf-deploy's `linux/modes_tests.rs`), and no
//!   daemon flag names a store or reads an environment variable (kbf-cli's tests).

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::process::Command;

use serde_json::Value;

const PURE: &[&str] = &[
    "kbf-types",
    "kbf-caps",
    "kbf-segments",
    "kbf-meta",
    "kbf-sched",
    "kbf-estimator",
    "kbf-raft",
    "kbf-invocation",
    "kbf-map",
];

const FORBIDDEN: &[&str] = &[
    "tokio",
    "tonic",
    "hyper",
    "reqwest",
    "rand",
    "rand_core",
    "rand_chacha",
    "getrandom",
    "mio",
    "socket2",
];

/// Modules of impure crates that hold state every server of a replicated farm must
/// hold alike, so they are kept pure as the pure crates are. Paths are from the
/// workspace's `crates` directory.
const PURE_MODULES: &[&str] = &["kbf-server/src/machine.rs"];

const DENY_ATTR: &str = "#![deny(clippy::disallowed_methods, clippy::disallowed_types)]";

struct Graph {
    /// Package id to package name.
    names: BTreeMap<String, String>,
    /// Package id to the ids of its normal dependencies.
    normal: BTreeMap<String, Vec<String>>,
    /// Workspace member name to (package id, path of its library root).
    members: BTreeMap<String, (String, Option<String>)>,
}

fn load_graph() -> Graph {
    let cargo = std::env::var("CARGO").expect("CARGO is set when cargo runs a test");
    let out = Command::new(cargo)
        .args(["metadata", "--format-version", "1", "--locked"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("run cargo metadata");
    assert!(
        out.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let meta: Value = serde_json::from_slice(&out.stdout).expect("cargo metadata emits JSON");

    let mut names = BTreeMap::new();
    for pkg in meta["packages"].as_array().expect("packages") {
        let id = pkg["id"].as_str().expect("package id").to_owned();
        names.insert(id, pkg["name"].as_str().expect("package name").to_owned());
    }

    let mut members = BTreeMap::new();
    let member_ids: BTreeSet<&str> = meta["workspace_members"]
        .as_array()
        .expect("workspace_members")
        .iter()
        .map(|v| v.as_str().expect("member id"))
        .collect();
    for pkg in meta["packages"].as_array().expect("packages") {
        let id = pkg["id"].as_str().expect("package id");
        if !member_ids.contains(id) {
            continue;
        }
        let lib = pkg["targets"]
            .as_array()
            .expect("targets")
            .iter()
            .find(|t| {
                t["kind"]
                    .as_array()
                    .is_some_and(|k| k.iter().any(|k| k == "lib"))
            })
            .map(|t| t["src_path"].as_str().expect("src_path").to_owned());
        let name = pkg["name"].as_str().expect("package name").to_owned();
        members.insert(name, (id.to_owned(), lib));
    }

    let mut normal = BTreeMap::new();
    for node in meta["resolve"]["nodes"].as_array().expect("resolve.nodes") {
        let deps = node["deps"]
            .as_array()
            .expect("deps")
            .iter()
            .filter(|d| {
                // A null kind is a normal dependency; "dev" and "build" are not linked
                // into the crate's library.
                d["dep_kinds"]
                    .as_array()
                    .expect("dep_kinds")
                    .iter()
                    .any(|k| k["kind"].is_null())
            })
            .map(|d| d["pkg"].as_str().expect("dep pkg").to_owned())
            .collect();
        normal.insert(node["id"].as_str().expect("node id").to_owned(), deps);
    }

    Graph {
        names,
        normal,
        members,
    }
}

/// Every package reachable from `root` through normal edges, with the chain of names
/// that reaches it (for the failure message).
fn normal_closure(g: &Graph, root: &str) -> BTreeMap<String, String> {
    let mut seen = BTreeMap::new();
    let mut queue = VecDeque::from([(root.to_owned(), g.names[root].clone())]);
    while let Some((id, chain)) = queue.pop_front() {
        for dep in g.normal.get(&id).into_iter().flatten() {
            if seen.contains_key(dep) {
                continue;
            }
            let dep_chain = format!("{chain} -> {}", g.names[dep]);
            seen.insert(dep.clone(), dep_chain.clone());
            queue.push_back((dep.clone(), dep_chain));
        }
    }
    seen
}

#[test]
fn pure_crates_have_no_impure_dependencies() {
    let g = load_graph();
    let mut violations = Vec::new();
    for &pure in PURE {
        let (id, _) = g
            .members
            .get(pure)
            .unwrap_or_else(|| panic!("{pure} is not a workspace member"));
        for (dep, chain) in normal_closure(&g, id) {
            let name = g.names[&dep].as_str();
            let impure_kbf = g.members.contains_key(name) && !PURE.contains(&name);
            if FORBIDDEN.contains(&name) || impure_kbf {
                violations.push(chain);
            }
        }
    }
    assert!(
        violations.is_empty(),
        "pure crates reach impure crates:\n{}",
        violations.join("\n")
    );
}

#[test]
fn pure_crates_deny_the_clippy_lists() {
    let g = load_graph();
    for &pure in PURE {
        let (_, lib) = g
            .members
            .get(pure)
            .unwrap_or_else(|| panic!("{pure} is not a workspace member"));
        let lib = lib
            .as_ref()
            .unwrap_or_else(|| panic!("{pure} has no library target"));
        let src = std::fs::read_to_string(lib).unwrap_or_else(|e| panic!("read {lib}: {e}"));
        assert!(
            src.lines().any(|l| l.trim() == DENY_ATTR),
            "{pure} ({lib}) must carry `{DENY_ATTR}`"
        );
    }
}

#[test]
fn pure_modules_deny_the_clippy_lists() {
    let crates = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    for &module in PURE_MODULES {
        let path = crates.join(module);
        let src = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        assert!(
            src.lines().any(|l| l.trim() == DENY_ATTR),
            "{module} must carry `{DENY_ATTR}`"
        );
    }
}

/// The crates allowed to depend on both the server and the node library: the combined
/// binary and the one `kbf` binary.
const JOINERS: &[&str] = &["kbf-all-in-one", "kbf-cli"];

/// What the server library's normal dependencies must not reach: the daemon side. The
/// server stores and schedules; it never moves blobs as a client.
const DAEMON_SIDE: &[&str] = &[
    "kbf-node",
    "kbf-daemon",
    "kbf-transfer",
    "kbf-driver-container",
    "kbf-driver-native",
];

/// What the node library's normal dependencies must not reach: the server side.
const SERVER_SIDE: &[&str] = &["kbf-server", "kbf-front"];

/// Names in `names` that `root`'s normal closure contains, with the chain reaching each.
fn reaches(g: &Graph, root: &str, names: &[&str]) -> Vec<String> {
    let (id, _) = g
        .members
        .get(root)
        .unwrap_or_else(|| panic!("{root} is not a workspace member"));
    normal_closure(g, id)
        .into_iter()
        .filter(|(dep, _)| names.contains(&g.names[dep].as_str()))
        .map(|(_, chain)| chain)
        .collect()
}

#[test]
fn server_and_node_libraries_stay_apart() {
    let g = load_graph();
    let mut violations = Vec::new();
    for lib in ["kbf-server", "kbf-node"] {
        let (_, root) = &g.members[lib];
        assert!(root.is_some(), "{lib} must have a library target");
    }
    violations.extend(reaches(&g, "kbf-server", DAEMON_SIDE));
    violations.extend(reaches(&g, "kbf-node", SERVER_SIDE));
    for (member, (id, _)) in &g.members {
        if JOINERS.contains(&member.as_str()) {
            continue;
        }
        let linked: BTreeSet<&str> = normal_closure(&g, id)
            .keys()
            .map(|dep| g.names[dep].as_str())
            .collect();
        if linked.contains("kbf-server") && linked.contains("kbf-node") {
            violations.push(format!("{member} links both kbf-server and kbf-node"));
        }
    }
    assert!(
        violations.is_empty(),
        "the server and the node library are linked together outside {JOINERS:?}:\n{}",
        violations.join("\n")
    );
}

/// The most lines of code (not blank, not comment) a binary's `main.rs` may have.
const THIN_MAIN_LINES: usize = 40;

#[test]
fn the_server_and_daemon_binaries_are_thin() {
    let crates = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    for main in [
        "kbf-server/src/main.rs",
        "kbf-node/src/main.rs",
        "kbf-cli/src/main.rs",
    ] {
        let path = crates.join(main);
        let src = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        let code = src
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with("//"))
            .count();
        assert!(
            code <= THIN_MAIN_LINES,
            "{main} has {code} code lines (at most {THIN_MAIN_LINES}): the assembly belongs \
             in the library, where a combined binary can call it"
        );
    }
}

/// What the storage crate's normal dependencies must not reach: the frontend's scheduler
/// and authorization (docs/design/architecture-v2.md, section 3). A storage process
/// stores; it neither schedules nor decides who may call.
const STORAGE_MUST_NOT_REACH: &[&str] = &["kbf-sched", "kbf-auth"];

#[test]
fn storage_reaches_no_frontend_crate() {
    let g = load_graph();
    let (_, root) = g
        .members
        .get("kbf-storage")
        .expect("kbf-storage is a workspace member");
    assert!(root.is_some(), "kbf-storage must have a library target");
    let violations = reaches(&g, "kbf-storage", STORAGE_MUST_NOT_REACH);
    assert!(
        violations.is_empty(),
        "kbf-storage reaches a frontend crate:\n{}",
        violations.join("\n")
    );
}

/// The daemon crates: the daemon's code, which the `kbf-daemon` binary is and which
/// `kbf daemon` runs inside the `kbf` binary, with the drivers, which `kbf-node` names
/// and so are in its closure.
const DAEMON_ONLY: &[&str] = &["kbf-node", "kbf-daemon"];

/// What a daemon crate's normal dependencies must not reach: the object store client
/// (`kbf-objstore`, which signs S3 requests), the crates that hold the cold store behind
/// their cache and build it from a cold-store URI (`kbf-front`, `kbf-server`), and the
/// standalone HTTP clients an S3 client could be written over (`reqwest`, which
/// `kbf-objstore` is written over, and `ureq`). `hyper-util` is not listed: tonic's
/// channel, which the daemon dials the server with, is built on its client.
const COLD_STORE_CLIENT: &[&str] = &["kbf-objstore", "kbf-front", "kbf-server", "reqwest", "ureq"];

#[test]
fn a_daemon_cannot_build_a_cold_store_client() {
    let g = load_graph();
    // Every name the check looks for is a package of the graph, and the walk sees the
    // client where it is linked: the server builds its cold store from it. Without
    // these, a renamed or misspelled crate would pass the check below.
    for name in COLD_STORE_CLIENT {
        assert!(
            g.names.values().any(|n| n == *name),
            "{name} is not a package of the workspace's graph"
        );
    }
    assert!(
        !reaches(&g, "kbf-server", &["kbf-objstore"]).is_empty(),
        "the server's closure must reach kbf-objstore, or this walk proves nothing"
    );
    let violations: Vec<String> = DAEMON_ONLY
        .iter()
        .flat_map(|daemon| reaches(&g, daemon, COLD_STORE_CLIENT))
        .collect();
    assert!(
        violations.is_empty(),
        "a daemon crate reaches an object-store client (a daemon-only node holds no \
         cold-store credential, docs/design/architecture-v2.md section 9):\n{}",
        violations.join("\n")
    );
}

/// What the routing library's normal dependencies must not reach: the frontend's scheduler
/// and authorization, the server, and the daemon's side (the transfer engine and the node).
/// The frontend and the daemon both use it (docs/design/architecture-v2.md, section 7); a
/// path to either side's code would make the other link it.
const ROUTE_MUST_NOT_REACH: &[&str] = &[
    "kbf-sched",
    "kbf-auth",
    "kbf-front",
    "kbf-server",
    "kbf-transfer",
    "kbf-node",
    "kbf-daemon",
];

#[test]
fn route_reaches_neither_the_frontend_nor_the_daemon() {
    let g = load_graph();
    let (_, root) = g
        .members
        .get("kbf-route")
        .expect("kbf-route is a workspace member");
    assert!(root.is_some(), "kbf-route must have a library target");
    let violations = reaches(&g, "kbf-route", ROUTE_MUST_NOT_REACH);
    assert!(
        violations.is_empty(),
        "kbf-route reaches a frontend or daemon crate:\n{}",
        violations.join("\n")
    );
}
