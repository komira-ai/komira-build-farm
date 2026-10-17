//! `kbf --version --json`: what this build is, for a host to check what it runs.
//!
//! One JSON object on one line of stdout, exit 0. The fields are the contract (a host's
//! check and the artifacts workflow, `cargo xtask build-commit check --json`, read
//! them), sorted by name:
//!
//! - `commit`: the first 12 hex digits of the commit built, or `unknown` for a build
//!   with no git checkout ([`kbf_server::BUILD_COMMIT`]);
//! - `min_protocol` and `protocol`: the oldest and the newest `kbf.worker.v1` protocol
//!   version this binary speaks ([`kbf_server::worker`]); the daemon half speaks exactly
//!   `protocol` (a test pins the two halves equal);
//! - `profile`: the Cargo profile, `release` or `debug` ([`PROFILE`]);
//! - `state_schema`: the schema version of the SQLite state file this binary creates and
//!   reads ([`kbf_state::sqlite::SCHEMA_VERSION`]);
//! - `version`: the package version.
//!
//! Only the two words `--version --json`, in either order and nothing else, ask for it;
//! `--json` anywhere else is the usage error it always was.

use std::ffi::OsString;

use serde_json::{Map, Value};

/// The Cargo profile this binary was built with: `release` or `debug`.
pub const PROFILE: &str = env!("KBF_BUILD_PROFILE");

/// Whether `args` (the program's name first) are exactly `--version --json` or
/// `--json --version`.
#[must_use]
pub fn asked(args: &[OsString]) -> bool {
    matches!(args, [_, a, b] if (a == "--version" && b == "--json") || (a == "--json" && b == "--version"))
}

/// The identity of this build.
#[must_use]
pub fn identity() -> Value {
    // Inserted in the order they print: sorted, whatever map `serde_json` is built with.
    let mut fields = Map::new();
    fields.insert("commit".to_owned(), kbf_server::BUILD_COMMIT.into());
    fields.insert(
        "min_protocol".to_owned(),
        kbf_server::worker::OLDEST_PROTOCOL_VERSION.into(),
    );
    fields.insert("profile".to_owned(), PROFILE.into());
    fields.insert(
        "protocol".to_owned(),
        kbf_server::worker::PROTOCOL_VERSION.into(),
    );
    fields.insert(
        "state_schema".to_owned(),
        kbf_state::sqlite::SCHEMA_VERSION.into(),
    );
    fields.insert("version".to_owned(), env!("CARGO_PKG_VERSION").into());
    Value::Object(fields)
}

/// The line `kbf --version --json` prints, with its line end.
#[must_use]
pub fn line() -> String {
    format!("{}\n", identity())
}
