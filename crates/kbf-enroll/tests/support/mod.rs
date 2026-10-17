//! Shared by the integration tests; each test binary uses part of it.
#![allow(dead_code)]

pub mod fake;
pub mod pki;

use std::path::PathBuf;

use kbf_enroll::config::{Config, Host};
use kbf_proto::kbf::enroll::v1::NodeFacts;

/// A fresh scratch directory.
pub fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("kbf-enroll-it-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch");
    std::fs::canonicalize(&dir).expect("canonical")
}

/// A `kbf.toml` for a node that dials `127.0.0.1:port` and keeps its state under `root`.
pub fn config_for(root: &std::path::Path, port: u16, extra: &str) -> Config {
    let text = format!(
        "[frontend]\nurl = \"https://localhost:{port}\"\n[node]\nid = \"node-02\"\n{extra}\n[enroll]\ntoken_file = \"{}\"\n[dirs]\nstate = \"{}\"\n",
        root.join("enroll.token").display(),
        root.join("state").display(),
    );
    Config::parse(
        &text,
        &Host {
            hostname: "node-02".to_owned(),
            cpus: 8,
        },
    )
    .unwrap_or_else(|e| panic!("config: {e}"))
}

pub fn facts() -> NodeFacts {
    NodeFacts {
        os: "linux".to_owned(),
        arch: "x86_64".to_owned(),
        commit: "abc".to_owned(),
        protocol: 1,
        package_version: "0.0.0".to_owned(),
    }
}
