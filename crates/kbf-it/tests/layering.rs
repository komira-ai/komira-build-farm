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
//!   pass vacuously.

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
