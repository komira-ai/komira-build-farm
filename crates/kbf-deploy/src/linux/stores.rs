//! Rules `storage`, `state-store` and `ports` of [`super::modes`]: where a unit's stores
//! are, that the state store is a SQLite file named by an absolute path, and that no
//! two listeners share a port.
//!
//! `storage` (the `storage` mode): the unit gives every flag a storage node needs to
//! serve the farm ([`STORAGE_FLAGS`]: its cold store, its metadata directory, its
//! listen address and the three certificate flags of mutual TLS), and an `s3://` cold
//! store comes with the key file (`EnvironmentFile=<KBF_ETC>/s3.env`): `kbf storage`
//! parses without any of them and then refuses to start, or serves plain text on
//! loopback only. `--hot-store` is not required: without it uploads go straight to the
//! cold store, slower but correct.
//!
//! `state-store` (every mode; the flag is the frontend's). The only backend kbf opens
//! today is SQLite (Postgres is a later backend), so a `--state-store` the unit gives
//! must be `sqlite:///<absolute path>`. The rule refuses:
//!
//! - A `postgres://` or `postgresql://` value: postgres is not wired yet.
//! - Any other scheme, or a value with no scheme.
//! - A `sqlite://` value whose path is empty, relative or not normal (`//`, `.` or
//!   `..` in it).
//! - A value that names a variable (`${VAR}` or `$VAR`): systemd expands it into the
//!   command line it starts kbf with, so the rule cannot read the store it names, and
//!   whatever the variable holds is in kbf's argv, which every user can read.
//!
//! No message echoes the value: a mistyped one may carry a password. A unit with no
//! `--state-store` passes (the modes that hold no state store give none).
//!
//! `ports` (every mode): the listeners `--listen`, `--worker-listen`, `--api-listen` and
//! `--storage-listen` ([`LISTEN_FLAGS`]) of one unit are on four ports. Two on one port
//! fail even on two addresses (`127.0.0.1` and the LAN address): one port per listener
//! on a host is what the settings give (`KBF_*_PORT`), and a second listener that binds
//! is an operator's surprise.

use kbf_host::path::HostPath;

use super::credential;
use crate::unit::UnitFile;

/// The listener flags of kbf's modes, without `--` (rule `ports`).
pub const LISTEN_FLAGS: [&str; 4] = ["listen", "worker-listen", "api-listen", "storage-listen"];

/// The flags rule `storage` requires of a storage node's unit, without `--`.
pub const STORAGE_FLAGS: [&str; 6] = [
    "cold-store",
    "meta-dir",
    "storage-listen",
    "storage-tls-cert",
    "storage-tls-key",
    "storage-client-ca",
];

/// The words of the last `ExecStart=`, unquoted as systemd unquotes them.
fn exec_words(unit: &UnitFile) -> Vec<String> {
    unit.get("ExecStart")
        .map(credential::words)
        .unwrap_or_default()
}

/// The last value `words` give `--<name>`, as `--<name>=V` or `--<name> V`.
fn flag<'a>(words: &'a [String], name: &str) -> Option<&'a str> {
    let long = format!("--{name}");
    let mut found = None;
    let mut it = words.iter();
    while let Some(w) = it.next() {
        if w == &long {
            found = it.next().map(String::as_str);
        } else if let Some(v) = w
            .strip_prefix(long.as_str())
            .and_then(|r| r.strip_prefix('='))
        {
            found = Some(v);
        }
    }
    found
}

/// Rule `storage`: how the unit of a storage node falls short, one line per way. With
/// `exact`, the unit that holds the key beside its builds (`storage-and-daemon`), the key
/// file must be named exactly (`EnvironmentFile=<key>`, as written), not by a wildcard or
/// with a `-`.
pub(super) fn storage(unit: &UnitFile, key: &std::path::Path, exact: bool) -> Vec<String> {
    let words = exec_words(unit);
    let mut out = Vec::new();
    let missing: Vec<String> = STORAGE_FLAGS
        .iter()
        .filter(|f| flag(&words, f).is_none_or(str::is_empty))
        .map(|f| format!("--{f}"))
        .collect();
    if !missing.is_empty() {
        out.push(format!(
            "gives no {}: a storage node keeps its blobs in a cold store and its index in a \
             metadata directory, and serves mutual TLS on its listen address",
            missing.join(" ")
        ));
    }
    let s3 = flag(&words, "cold-store").is_some_and(|v| v.starts_with("s3://"));
    let keyed = credential::given(unit).files.iter().any(|f| {
        if exact {
            credential::is_exactly(f, key)
        } else {
            credential::names_key(f, key)
        }
    });
    if s3 && !keyed {
        out.push(format!(
            "has an s3:// cold store and no EnvironmentFile={}: kbf storage starts without \
             the bucket's key pair and refuses",
            key.display()
        ));
    }
    out
}

/// The variable a `--state-store` value names: `${VAR}` or `$VAR` anywhere in it.
fn variable(value: &str) -> Option<&str> {
    let after = &value[value.find('$')? + 1..];
    let name = match after.strip_prefix('{') {
        Some(braced) => &braced[..braced.find('}')?],
        None => {
            let end = after
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .unwrap_or(after.len());
            &after[..end]
        }
    };
    (!name.is_empty()).then_some(name)
}

/// Why a `--state-store` value is refused, or `None` when it is `sqlite:///<absolute
/// path>`.
fn refusal(value: &str) -> Option<String> {
    if let Some(var) = variable(value) {
        return Some(format!(
            "--state-store names ${{{var}}}: systemd expands it into kbf's command line, \
             which every user on the node can read in /proc, and the rule cannot read the \
             store it names; write sqlite:///<absolute path>"
        ));
    }
    if value.starts_with("postgres://") || value.starts_with("postgresql://") {
        return Some(
            "--state-store is a postgres URI: postgres is not wired yet; the state store \
             is sqlite:///<absolute path>"
                .to_owned(),
        );
    }
    let Some(path) = value.strip_prefix("sqlite://") else {
        return Some(
            "--state-store is not sqlite:///<absolute path>: SQLite is the only state \
             store kbf opens"
                .to_owned(),
        );
    };
    HostPath::new(path).is_err().then(|| {
        "--state-store is sqlite:// with an empty, relative or not normal path: give \
         sqlite:///<absolute path>"
            .to_owned()
    })
}

/// Rule `state-store`: its failures (at most one, see the module docs).
pub(super) fn state_store(unit: &UnitFile) -> Vec<String> {
    let words = exec_words(unit);
    flag(&words, "state-store")
        .and_then(refusal)
        .into_iter()
        .collect()
}

/// Rule `ports`: the listener flags of `unit` that share a port with an earlier one.
pub(super) fn ports(unit: &UnitFile) -> Vec<String> {
    let words = exec_words(unit);
    let mut seen: Vec<(&str, &str)> = Vec::new();
    let mut out = Vec::new();
    for f in LISTEN_FLAGS {
        let Some((_, port)) = flag(&words, f).and_then(|v| v.rsplit_once(':')) else {
            continue;
        };
        match seen.iter().find(|(p, _)| *p == port) {
            Some((_, first)) => out.push(format!(
                "--{first} and --{f} are both on port {port}: give each listener its own \
                 (KBF_REAPI_PORT, KBF_WORKER_PORT, KBF_API_PORT, KBF_STORAGE_PORT)"
            )),
            None => seen.push((port, f)),
        }
    }
    out
}

#[cfg(test)]
#[path = "stores_tests.rs"]
mod tests;
