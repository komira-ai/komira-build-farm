//! `/etc/kbf/kbf.toml`: the settings of a mode, in a file instead of on the command line.
//!
//! The file is the mode's flags and nothing else. Its keys are the flags' long names
//! (`--storage-listen` is `storage-listen`), grouped in one TOML table per setting group
//! ([`group_of`]), and a value is what the flag takes: a string or a number for a flag
//! with a value, `true` for a switch, an array for a flag that may be given more than
//! once. The file is turned into the flags it stands for ([`file_args`]) and clap parses
//! those with the command line, so a setting has the same name, the same validation and
//! the same default in the file as on the command line, and nothing here knows a mode's
//! flags: the mode's own command says which keys exist.
//!
//! Rules:
//!
//! - a flag given on the command line overrides the file's key for it;
//! - a key that is not a flag of the mode, or sits in another table than the flag's
//!   group, is refused naming the key and the file;
//! - the file must be a regular file that neither group nor other can write, since it
//!   decides what the process serves and where it keeps its keys;
//! - a secret is never a value in the file: the flags that take a secret take the path of
//!   a file that holds it, and so does the key.
//!
//! The path is `--config FILE`, else [`DEFAULT_PATH`] when that file exists.
//! [`template`] writes the whole file for a mode from its flag definitions, which
//! `tests/golden/` pins.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;

use clap::{Arg, ArgAction, Command};

/// The file `kbf <mode>` reads when `--config` does not name one and this exists.
pub const DEFAULT_PATH: &str = "/etc/kbf/kbf.toml";

/// The group bits and other bits that may not be set on the file: write.
const WRITABLE_BY_OTHERS: u32 = 0o022;

/// The name of the node-label setting, removed with `--label`: a file that still sets it
/// is refused with [`LABEL_KEY_REMOVED`], not with the general unknown-key line.
const REMOVED_LABEL: &str = "label";

/// What a file that still has a `label` key is told: it names the key and the way out,
/// and the node does not start. The flag's twin is `--label was removed` in
/// [`crate::parse_with`].
const LABEL_KEY_REMOVED: &str =
    "`label` was removed: placement uses what the node reports (OSFamily, ISA); delete the key";

/// The table of the file a flag's key is in, by the flag's long name; `None` for a flag
/// that is in no group (which `config::tests` refuses for every flag of every mode, so a
/// flag cannot be added without its key).
#[must_use]
pub fn group_of(flag: &str) -> Option<&'static str> {
    GROUPS
        .iter()
        .find(|(_, flags)| flags.contains(&flag))
        .map(|(group, _)| *group)
}

/// The flags of each group. A flag is in exactly one.
const GROUPS: &[(&str, &[&str])] = &[
    (
        "listener",
        &[
            "advertise-addr",
            "api-listen",
            "drain-deadline",
            "drain-lb-wait",
            "http2-keepalive-interval-secs",
            "http2-keepalive-timeout-secs",
            "listen",
            "metrics-listen",
            "reapi-max-streams",
            "release-poll-every-secs",
            "release-poll-refs",
            "release-poll-repo",
            "releases-dir",
            "shutdown-timeout-secs",
            "storage-listen",
            "tls-reload-interval-secs",
            "worker-listen",
            "worker-max-streams",
        ],
    ),
    (
        "tls",
        &[
            "api-token-file",
            "proxy-auth-file",
            "reapi-auth-policy",
            "reapi-tls-cert",
            "reapi-tls-key",
            "storage-client-ca",
            "storage-tls-cert",
            "storage-tls-key",
            "worker-client-ca",
            "worker-deny-list",
            "worker-tls-cert",
            "worker-tls-key",
        ],
    ),
    (
        "scheduler",
        &[
            "default-platform",
            "finished-retention-secs",
            "heartbeat-interval-ms",
            "input-tree-cache-bytes",
            "input-tree-cache-entries",
            "input-tree-reads-in-flight",
            "progress-interval-secs",
            "readyz-store-timeout-ms",
            "state-store",
            "unservable-wait-secs",
            "vm-host-floor-cpus",
            "vm-host-floor-mem-gib",
        ],
    ),
    (
        "history",
        &[
            "history-index-root",
            "history-objects",
            "invocation-memory",
            "invocations-open",
            "ui",
        ],
    ),
    (
        // The flags of the removed handover and admin port, accepted and ignored for one
        // release (`kbf_server::retired::RetiredHandoverFlags`): the keys stay so that a
        // `kbf.toml` written before the removal is still read.
        "handover",
        &[
            "admin-listen",
            "await-takeover",
            "envoy-check-interval-ms",
            "envoy-check-timeout-ms",
            "envoy-unhealthy-threshold",
            "handover-follow-interval-ms",
            "handover-peer-name",
            "handover-quiesce-secs",
            "handover-socket",
            "handover-watchdog-secs",
            "instance",
        ],
    ),
    (
        "storage-client",
        &[
            "storage",
            "storage-ca",
            "storage-client-cert",
            "storage-client-key",
            "storage-dial-timeout-ms",
            "storage-ping-interval-ms",
            "storage-ping-timeout-ms",
        ],
    ),
    (
        "stores",
        &[
            "cold-store",
            "cold-store-evict-grace-ms",
            "cold-store-floor-bytes",
            "cold-store-size-bytes",
            "durable-wait",
            "hot-store",
            "hot-store-abandon",
            "hot-store-max-bytes",
            "meta-dir",
            "object-store-secrets-dir",
            "ram-ack-bytes",
            "ram-drain-deadline",
            "ram-peer-grace",
            "store-put-attempts",
            "write-bytes-in-flight",
            "write-linger",
            "write-own-segment-bytes",
            "write-pool-queue",
            "write-pool-stream-idle",
            "write-pool-writers",
            "write-puts-in-flight",
        ],
    ),
    (
        "storage-node",
        &[
            "auto-promote",
            "handoff-allow-degraded",
            "handoff-settle-secs",
            "member-dwell-secs",
            "member-ttl-secs",
            "storage-weight",
            "storage-zone",
        ],
    ),
    ("log", &["log-dir", "log-files", "log-max-bytes"]),
    (
        // Not `enroll` or `frontend`: those tables are the enrollment client's own.
        "enroll-service",
        &[
            "ca-dev-file-signer",
            "ca-dir",
            "ca-signer",
            "enroll-allow-unenforced-deny",
            "enroll-cas-endpoint",
            "enroll-cert-lifetime-secs",
            "enroll-listen",
            "enroll-max-in-flight",
            "enroll-rate-burst",
            "enroll-rate-per-minute",
            "enroll-storage-port",
            "enroll-worker-endpoint",
            "frontend-name",
        ],
    ),
    (
        "identity",
        &[
            "ca-cert",
            "cas",
            "cert",
            "frontend",
            "key",
            "node-id",
            "server",
            "tls-server-name",
        ],
    ),
    (
        "daemon",
        &[
            "async-threads",
            "blocking-threads",
            "cpu-threads",
            "daemon-user",
            "data-dir",
            "data-max-bytes",
            "drain-deadline-secs",
            "driver",
            "image",
            "native-term-grace-ms",
            "orders-dir",
            "reconnect-max-ms",
            "reconnect-ms",
            "reserve-cpus",
            "reserve-share",
            "role",
            "update-dir",
        ],
    ),
    (
        "containers",
        &[
            "actions-memory-max-gib",
            "cgroup-parent",
            "cgroup-root",
            "container-nofile",
            "container-nproc",
            "container-pids-limit",
            "container-shm-mib",
            "id-files",
            "lease-placement",
            "lease-swap-kill-mib",
            "lease-swap-kill-percent",
            "lease-swap-poll-ms",
            "numa-dir",
            "oci-runtime",
            "psi-memory-file",
            "self-cgroup",
            "supervisor-memory-min-mib",
        ],
    ),
    (
        "memory",
        &[
            "learned-sizes",
            "memory-headroom-mib",
            "memory-ladder-gib",
            "memory-limit-percent",
            "memory-poll-ms",
            "memory-pressure-pause-percent",
        ],
    ),
    (
        "macos",
        &[
            "mac-session-socket",
            "pool-first-uid",
            "pool-users",
            "require-metal-toolchain",
            "xcode-apps",
            "xcode-recheck-secs",
        ],
    ),
    (
        "outputs",
        &[
            "output-max-bytes",
            "output-max-depth",
            "output-max-entries",
            "output-max-stdio-bytes",
            "output-upload-deadline-secs",
            "segment-upload-idle-secs",
            "segment-upload-max-bytes",
            "segment-upload-streams",
            "segment-uploads-in-flight",
        ],
    ),
    (
        "transfer",
        &[
            "transfer-bytes-in-flight",
            "transfer-chunk-bytes",
            "transfer-find-missing-chunk",
            "transfer-in-flight",
            "transfer-memory-blob-bytes",
            "transfer-retries",
            "transfer-stream-window-bytes",
        ],
    ),
];

/// The arguments of `own` that are flags with a long name a file may set: all but help,
/// version, the two flags of this module and the `refused` ones (the flags of
/// `kbf-server` that a mode hides and refuses by name have no key).
fn settings<'a>(own: &'a Command, refused: &BTreeSet<String>) -> BTreeMap<String, &'a Arg> {
    own.get_arguments()
        .filter_map(|a| a.get_long().map(|long| (long.to_owned(), a)))
        .filter(|(long, _)| !["help", "version", "config", "check"].contains(&long.as_str()))
        .filter(|(long, _)| !refused.contains(long))
        .collect()
}

/// Whether `arg` is a switch: it takes no value.
fn is_switch(arg: &Arg) -> bool {
    matches!(
        arg.get_action(),
        ArgAction::SetTrue | ArgAction::SetFalse | ArgAction::Count
    )
}

/// Why the file at `path` may not be read, if it may not: not there, not a regular file,
/// or writable by group or other.
///
/// # Errors
/// The reason, naming the file.
pub fn check_file(path: &Path) -> Result<(), String> {
    let meta = std::fs::metadata(path)
        .map_err(|e| format!("cannot read the config file {}: {e}", path.display()))?;
    if !meta.is_file() {
        return Err(format!(
            "the config file {} is not a regular file",
            path.display()
        ));
    }
    let mode = meta.permissions().mode() & 0o7777;
    if mode & WRITABLE_BY_OTHERS != 0 {
        return Err(format!(
            "the config file {} is writable by group or other (mode {mode:04o}): it decides what \
             the process serves, so run `chmod go-w {}`",
            path.display(),
            path.display(),
        ));
    }
    Ok(())
}

/// The top-level `mode` of the file at `path`: `None` when it sets none.
///
/// # Errors
/// The file may not be read ([`check_file`]), is not TOML, or `mode` is not a string.
pub fn file_mode(path: &Path) -> Result<Option<String>, String> {
    check_file(path)?;
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read the config file {}: {e}", path.display()))?;
    let table: toml::Table = toml::from_str(&text)
        .map_err(|e| format!("the config file {} is not valid TOML: {e}", path.display()))?;
    match table.get("mode") {
        None => Ok(None),
        Some(toml::Value::String(mode)) => Ok(Some(mode.clone())),
        Some(_) => Err(format!(
            "the config file {}: `mode` is a string naming one mode",
            path.display()
        )),
    }
}

/// Whether the file at `path` holds enrollment's tables (`[enroll]`, `[frontend]` or
/// `[node]`: a node that gets its certificates and its settings from a frontend), as
/// against a file of the mode's flags alone. A file that cannot be read or parsed does
/// not: the flags' parser reports it.
#[must_use]
pub fn enrolls(path: &Path) -> bool {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| toml::from_str::<toml::Table>(&text).ok())
        .is_some_and(|table| {
            ["enroll", "frontend", "node"]
                .iter()
                .any(|t| table.get(*t).is_some_and(toml::Value::is_table))
        })
}

/// The refusal of `group = <value>` at the top of the file at `path` when `group` is a
/// table of enrollment ([`kbf_enroll::config::KEYS`]): a string, number or bool there is
/// no setting, and without this it would be dropped (`storage = "http://..."` meant as
/// the frontend's `--storage`) or reach enrollment's parser, which names no file.
fn value_for_table(path: &Path, group: &str, body: &toml::Value) -> Option<String> {
    let own = !group.is_empty() && kbf_enroll::config::KEYS.iter().any(|(t, _)| *t == group);
    (own && !body.is_table()).then(|| {
        format!(
            "key `{group}` in {} is a value, but [{group}] is the enrollment's table: it holds \
             keys, not a value",
            path.display()
        )
    })
}

/// Checks that no table of enrollment is a value in the file `text` read from `path`
/// ([`value_for_table`]); text that is not TOML is left to the parser that reads it next.
///
/// # Errors
/// Every such key, one per line, naming the file.
pub fn check_own_tables(path: &Path, text: &str) -> Result<(), String> {
    let Ok(table) = toml::from_str::<toml::Table>(text) else {
        return Ok(());
    };
    let problems: Vec<String> = table
        .iter()
        .filter_map(|(group, body)| value_for_table(path, group, body))
        .collect();
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems.join("\n"))
    }
}

/// The flags the file at `path` stands for, for the mode whose own command is `own`:
/// `--key=value` per setting, in the file's order of tables and keys, leaving out each
/// flag in `given` (an id of `own`: it was given on the command line, which wins).
///
/// # Errors
/// Every problem of the file, one per line: it may not be read ([`check_file`]), it is
/// not TOML, a key is not a flag of the mode or is in the wrong table, or a value is not
/// what the flag takes.
pub fn file_args(
    own: &Command,
    refused: &BTreeSet<String>,
    path: &Path,
    given: &BTreeSet<String>,
) -> Result<Vec<OsString>, String> {
    check_file(path)?;
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read the config file {}: {e}", path.display()))?;
    let table: toml::Table = toml::from_str(&text)
        .map_err(|e| format!("the config file {} is not valid TOML: {e}", path.display()))?;
    let known = settings(own, refused);
    let mut args = Vec::new();
    let mut problems = Vec::new();
    for (group, body) in &table {
        // Enrollment's own keys share the file (`kbf_enroll::config::KEYS`): the top-level
        // `mode`, and the tables of `[enroll]`, `[frontend]`, `[node]`, `[dirs]` and
        // `[storage]`, which hold no flag; `[daemon]` and `[tls]` hold both.
        if kbf_enroll::config::is_key("", group) {
            continue;
        }
        if kbf_enroll::config::is_own_table(group) {
            match value_for_table(path, group, body) {
                Some(problem) => problems.push(problem),
                None => {
                    if let Some(key) = body.as_table().and_then(|keys| {
                        keys.keys()
                            .find(|key| !kbf_enroll::config::is_key(group, key))
                    }) {
                        problems.push(format!(
                            "unknown key `{group}.{key}` in {}: [{group}] is the enrollment's table and has no such key",
                            path.display()
                        ));
                    }
                }
            }
            continue;
        }
        let toml::Value::Table(keys) = body else {
            problems.push(match group_of(group) {
                Some(g) if known.contains_key(group.as_str()) => format!(
                    "key `{group}` in {} is outside a table: it belongs under [{g}]",
                    path.display()
                ),
                _ if group == REMOVED_LABEL => format!(
                    "unknown key `{group}` in {}: {LABEL_KEY_REMOVED}",
                    path.display()
                ),
                _ => format!(
                    "unknown key `{group}` in {}: settings are keys of tables, one per group",
                    path.display()
                ),
            });
            continue;
        };
        for (key, value) in keys {
            if kbf_enroll::config::is_key(group, key) {
                continue;
            }
            let Some(arg) = known.get(key.as_str()) else {
                problems.push(format!(
                    "unknown key `{group}.{key}` in {}: {}",
                    path.display(),
                    if key == REMOVED_LABEL {
                        LABEL_KEY_REMOVED.to_owned()
                    } else {
                        format!("kbf has no flag --{key} in this mode")
                    }
                ));
                continue;
            };
            match group_of(key) {
                Some(g) if g == group => {}
                Some(g) => {
                    problems.push(format!(
                        "unknown key `{group}.{key}` in {}: --{key} is a setting of [{g}]",
                        path.display()
                    ));
                    continue;
                }
                None => {
                    problems.push(format!(
                        "unknown key `{group}.{key}` in {}: --{key} is in no group",
                        path.display()
                    ));
                    continue;
                }
            }
            if given.contains(arg.get_id().as_str()) {
                continue;
            }
            match values(arg, key, value) {
                Ok(v) => args.extend(v),
                Err(why) => {
                    problems.push(format!("key `{group}.{key}` in {}: {why}", path.display()))
                }
            }
        }
    }
    if problems.is_empty() {
        Ok(args)
    } else {
        Err(problems.join("\n"))
    }
}

/// The flags one key stands for.
fn values(arg: &Arg, key: &str, value: &toml::Value) -> Result<Vec<OsString>, String> {
    use toml::Value as V;
    let one = |v: &V| -> Result<String, String> {
        match v {
            V::String(s) => Ok(s.clone()),
            V::Integer(i) => Ok(i.to_string()),
            V::Float(f) => Ok(f.to_string()),
            V::Boolean(b) => Ok(b.to_string()),
            V::Datetime(_) | V::Array(_) | V::Table(_) => {
                Err("expects a string or a number".to_owned())
            }
        }
    };
    if is_switch(arg) {
        return match value {
            V::Boolean(true) => Ok(vec![format!("--{key}").into()]),
            V::Boolean(false) => Ok(Vec::new()),
            _ => Err("is a switch: it takes true or false".to_owned()),
        };
    }
    match value {
        V::Array(items) if matches!(arg.get_action(), ArgAction::Append) => items
            .iter()
            .map(|v| one(v).map(|s| format!("--{key}={s}").into()))
            .collect(),
        V::Array(_) => Err("takes one value, not an array".to_owned()),
        v => Ok(vec![format!("--{key}={}", one(v)?).into()]),
    }
}

/// The whole file for the mode whose own command is `own`: every setting in its group's
/// table, with the flag's help; a setting with a default is a live line holding it, one
/// without is a comment showing its shape. A file of the live lines alone is the mode's
/// defaults.
///
/// # Panics
/// On a flag that [`group_of`] does not place (`config::tests` refuses that).
#[must_use]
pub fn template(mode: &str, own: &Command, refused: &BTreeSet<String>) -> String {
    let mut groups: BTreeMap<&'static str, Vec<(String, &Arg)>> = BTreeMap::new();
    for (long, arg) in settings(own, refused) {
        let group = group_of(&long).unwrap_or_else(|| panic!("--{long} is in no group"));
        groups.entry(group).or_default().push((long, arg));
    }
    let mut out = format!(
        "# kbf {mode}: every setting, one table per group; a key is a flag's long name.\n\
         # Generated from the flag definitions (tests/golden). A flag on the command line\n\
         # overrides its key. A secret is the path of the file that holds it.\n"
    );
    for (group, keys) in groups {
        out.push_str(&format!("\n[{group}]\n"));
        for (long, arg) in keys {
            out.push_str(&entry(&long, arg));
        }
    }
    out
}

fn entry(long: &str, arg: &Arg) -> String {
    let mut out = String::new();
    if let Some(help) = arg.get_help() {
        for line in help.to_string().lines() {
            out.push_str(&format!("# {line}\n"));
        }
    }
    if arg.is_hide_set() {
        out.push_str("# (not in --help)\n");
    }
    let defaults: Vec<String> = arg
        .get_default_values()
        .iter()
        .map(|v| v.to_string_lossy().into_owned())
        .collect();
    let shape = if is_switch(arg) {
        "false".to_owned()
    } else if matches!(arg.get_action(), ArgAction::Append) {
        let items: Vec<String> = defaults.iter().map(|d| literal(d)).collect();
        format!("[{}]", items.join(", "))
    } else {
        match defaults.first() {
            Some(d) => literal(d),
            None => literal(
                &arg.get_value_names()
                    .and_then(|n| n.first().map(ToString::to_string))
                    .unwrap_or_else(|| long.to_uppercase()),
            ),
        }
    };
    let live = is_switch(arg) || !defaults.is_empty();
    if live {
        out.push_str(&format!("{long} = {shape}\n"));
    } else {
        out.push_str(&format!("# {long} = {shape}\n"));
    }
    out
}

/// A TOML literal for a default: a number as itself, anything else a string.
fn literal(value: &str) -> String {
    if value.parse::<i64>().is_ok() || value.parse::<f64>().is_ok_and(f64::is_finite) {
        value.to_owned()
    } else {
        format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
    }
}

#[cfg(test)]
mod tests;
