//! Rule `credential` of [`super::modes`]: the units of a build node do not give it the
//! cold store's key. The rule reads the unit and its drop-ins as systemd applies them
//! (systemd.exec(5)): every assignment in order, an empty one resetting its list.
//!
//! What it refuses, in `[Service]`:
//!
//! - `EnvironmentFile=` naming the key file: its path, after `-` and quotes are taken
//!   off (and systemd's whitespace, not any Unicode space), is `<KBF_ETC>/s3.env`
//!   through `.`, `..` or repeated `/`, or is absolute and ends in `/s3.env` wherever
//!   it is, or it is a wildcard that matches `<KBF_ETC>/s3.env`. systemd expands every
//!   path with glob(3) ([`super::glob`] reads it as glibc does: `*`, `?`, `[...]` with
//!   ranges, `!` or `^`, `[:class:]`, `[=c=]` and `[.c.]`, and `\` escapes), so a `\`
//!   in a path with no wildcard is taken off too, and a trailing `/` after the name
//!   still lists the file (`s3.env/`), while a trailing `/.` (ENOTDIR) or a wildcard
//!   before a `/` (directories only) lists nothing the rule refuses.
//! - `Environment=` setting any `AWS_*` variable, with systemd's quoting (several
//!   assignments on a line, single or double quotes around an assignment or inside
//!   one). A build node talks to no AWS-style store, so no `AWS_*` variable has a use
//!   there: the key pair, `AWS_SESSION_TOKEN`, and `AWS_PROFILE`,
//!   `AWS_SHARED_CREDENTIALS_FILE` or `AWS_CONFIG_FILE`, which point an SDK at a key
//!   anywhere.
//! - `PassEnvironment=` naming an `AWS_*` variable (it passes the service manager's).
//! - Any `LoadCredential=`, `LoadCredentialEncrypted=`, `SetCredential=`,
//!   `SetCredentialEncrypted=` or `ImportCredential=`: a systemd credential is opaque to
//!   the rule, and a build node's units need none.
//!
//! On the node ([`on_host`]) it also reads each `EnvironmentFile=` that names one file,
//! and refuses one that sets an `AWS_*` variable (a copy or a hard link of the key under
//! another name) or that is a symbolic link (not followed, so its target is unknown).
//!
//! What it cannot catch:
//!
//! - Offline, a file under another name that holds the key, or links to it, and a
//!   wildcard elsewhere that matches a copy named `s3.env`.
//! - On the node, the files a wildcard `EnvironmentFile=` matches (not listed: a
//!   `SKIP credential:` line says so), and a file the check cannot read (another
//!   `SKIP credential:` line).
//! - `%` specifiers (`%E/kbf/s3.env`): not expanded, so only the last part matches.
//! - Drop-ins outside the unit's own directory (`/run`, `/usr/lib`, the `service.d/`
//!   and prefix drop-ins), and `systemctl set-property` or `edit --runtime` changes.
//! - `UnsetEnvironment=` is not read: a variable it unsets still fails the rule.
//! - The key reaching the node any other way: a file the daemon reads by a flag, the
//!   service manager's own environment without `PassEnvironment=`, or a world-readable
//!   key file.

use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};

use kbf_host::exec::{Entry, Exec, Kind};
use kbf_host::path::HostPath;
use kbf_host::user;

use super::glob;
use super::modes::COLD_STORE_KEY_FILE;
use crate::unit::{UnitFile, WHITESPACE};

/// The prefix of the variables an AWS SDK or CLI reads (rule `credential`).
pub const AWS_PREFIX: &str = "AWS_";

/// The keys that pass a systemd credential, in the groups an empty value resets
/// together (systemd's `config_parse_load_credential`, `_set_credential`,
/// `_import_credential`).
const CREDENTIAL_KEYS: [&[&str]; 3] = [
    &["LoadCredential", "LoadCredentialEncrypted"],
    &["SetCredential", "SetCredentialEncrypted"],
    &["ImportCredential"],
];

/// What a unit's `[Service]` gives its processes, every assignment applied in order.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Given {
    /// The variables `Environment=` sets.
    pub set: BTreeSet<String>,
    /// The `EnvironmentFile=` values, as written.
    pub files: Vec<String>,
    /// The variables `PassEnvironment=` passes.
    pub passed: BTreeSet<String>,
    /// Each credential directive, as `Key=value`.
    pub credentials: Vec<String>,
}

/// What `unit` gives, by systemd's rules for each key.
pub(super) fn given(unit: &UnitFile) -> Given {
    let mut g = Given::default();
    let mut creds: [Vec<String>; 3] = Default::default();
    for d in unit.directives().iter().filter(|d| d.section == "Service") {
        let v = d.value.as_str();
        match d.key.as_str() {
            "Environment" if v.is_empty() => g.set.clear(),
            "Environment" => g.set.extend(
                words(v)
                    .into_iter()
                    .filter_map(|w| w.split_once('=').map(|(name, _)| name.to_owned())),
            ),
            "EnvironmentFile" if v.is_empty() => g.files.clear(),
            "EnvironmentFile" => g.files.push(v.to_owned()),
            "PassEnvironment" if v.is_empty() => g.passed.clear(),
            "PassEnvironment" => g.passed.extend(words(v)),
            key => {
                if let Some(i) = CREDENTIAL_KEYS.iter().position(|g| g.contains(&key)) {
                    if v.is_empty() {
                        creds[i].clear();
                    } else {
                        creds[i].push(format!("{key}={v}"));
                    }
                }
            }
        }
    }
    g.credentials = creds.concat();
    g
}

/// How `unit` gives a build node the key whose file is `key`, one line per way. With
/// `allow_file`, the unit that holds the key beside its builds, an `EnvironmentFile=`
/// that is exactly `key` (its path as written: no `-`, quotes, wildcard or `..`) is not
/// one; every other way is.
pub(super) fn key_given(unit: &UnitFile, key: &Path, allow_file: bool) -> Vec<String> {
    let g = given(unit);
    let mut out: Vec<String> = g
        .files
        .iter()
        .filter(|f| names_key(f, key) && !(allow_file && is_exactly(f, key)))
        .map(|f| format!("EnvironmentFile={f}"))
        .collect();
    out.extend(
        g.set
            .iter()
            .filter(|n| n.starts_with(AWS_PREFIX))
            .map(|n| format!("Environment= sets {n}")),
    );
    out.extend(
        g.passed
            .iter()
            .filter(|n| n.starts_with(AWS_PREFIX))
            .map(|n| format!("PassEnvironment= passes {n}")),
    );
    out.extend(
        g.credentials
            .iter()
            .map(|c| format!("{c}, a systemd credential")),
    );
    out
}

/// What reading the node's environment files found: failures and `SKIP` notes.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct OnHost {
    pub failures: Vec<String>,
    pub notes: Vec<String>,
}

/// Rule `credential`'s reading of the files `unit`'s `EnvironmentFile=` lines name, on
/// the host `exec` reads; `key` is the key file, which [`key_given`] reports itself.
pub(super) fn on_host<E: Exec>(exec: &E, unit: &UnitFile, key: &Path) -> OnHost {
    let mut out = OnHost::default();
    for f in given(unit).files.iter().filter(|f| !names_key(f, key)) {
        let path = bare(f);
        if glob::is_wildcard(path) {
            out.notes.push(format!(
                "SKIP credential: EnvironmentFile={f} is a wildcard; the files it matches \
                 were not read"
            ));
            continue;
        }
        let Some(host) = env_file(path) else {
            continue;
        };
        match exec.inspect(&host) {
            Ok(Some(Entry::File { bytes, .. })) => {
                if let Some(name) = env_file_names(&bytes).find(|n| n.starts_with(AWS_PREFIX)) {
                    out.failures
                        .push(format!("EnvironmentFile={f}, which sets {name}"));
                }
            }
            Ok(Some(Entry::Link { .. } | Entry::Other)) => out.failures.push(format!(
                "EnvironmentFile={f}, a link or special file the check does not follow \
                 (name the file itself)"
            )),
            Ok(Some(Entry::Dir { .. }) | None) => {}
            Err(e) => out.notes.push(format!(
                "SKIP credential: cannot read EnvironmentFile={f} ({e}), so its variables \
                 were not checked"
            )),
        }
    }
    out
}

/// Rule `credential` for the mode whose unit holds the key (`storage`): how the unit
/// runs as a user that is not the key's alone, or `None`. With no `User=` it runs as
/// root; as `build_user` (`KBF_USER`) it runs as the user builds run as.
pub(super) fn holder(unit: &UnitFile, build_user: &str) -> Option<String> {
    match unit
        .get_in("Service", "User")
        .map(|u| u.trim_matches(WHITESPACE))
    {
        None | Some("") => Some("runs as root (no User=)".to_owned()),
        Some(u) if u == build_user => {
            Some(format!("runs as {u}, the user builds run as (KBF_USER)"))
        }
        Some(_) => None,
    }
}

/// [`holder`]'s reading of the node: each key file the unit's `EnvironmentFile=` lines
/// name (`key` itself for a wildcard that matches it) is a plain file, root's or the
/// unit's user's, with no group or other bit. A key file that is not there, a
/// user or file the check cannot read, is a `SKIP credential:` line.
pub(super) fn holder_on_host<E: Exec>(exec: &E, unit: &UnitFile, key: &HostPath) -> OnHost {
    let mut out = OnHost::default();
    let Some(user) = unit
        .get_in("Service", "User")
        .map(|u| u.trim_matches(WHITESPACE))
        .filter(|u| !u.is_empty())
    else {
        return out;
    };
    let named: Vec<String> = given(unit)
        .files
        .iter()
        .filter(|f| names_key(f, Path::new(key.as_str())))
        .map(|f| bare(f).to_owned())
        .collect();
    for f in named {
        let path = env_file(&f).unwrap_or_else(|| key.clone());
        let meta = match exec.stat(&path) {
            Ok(Some(m)) => m,
            Ok(None) => {
                out.notes.push(format!(
                    "SKIP credential: no key file at {path} yet, so its owner and mode were \
                     not checked"
                ));
                continue;
            }
            Err(e) => {
                out.notes.push(format!(
                    "SKIP credential: cannot read {path} ({e}), so its owner and mode were \
                     not checked"
                ));
                continue;
            }
        };
        if meta.kind != Kind::File {
            out.failures.push(format!(
                "{path} is not a plain file (a link or a directory)"
            ));
            continue;
        }
        let bits = meta.perms.bits();
        if bits & 0o077 != 0 {
            out.failures.push(format!(
                "{path} is mode {bits:04o}, so users other than {user} can read the key: make \
                 it 0600"
            ));
        }
        // systemd reads the file as root, so a root-owned key works as well; any other
        // owner can read or replace it.
        if meta.owner.uid == 0 {
            continue;
        }
        match user::lookup(exec, user) {
            Ok(Some(u)) if u.uid != meta.owner.uid => out.failures.push(format!(
                "{path} is uid {}'s, neither root's nor {user}'s (uid {}): that user can \
                 read or replace the key",
                meta.owner.uid, u.uid
            )),
            Ok(Some(_)) => {}
            Ok(None) => out
                .failures
                .push(format!("User={user} is not a user on this node")),
            Err(e) => out.notes.push(format!(
                "SKIP credential: cannot look up User={user} ({e}), so the owner of {path} \
                 was not checked"
            )),
        }
    }
    out
}

/// Rule `credential` for the mode whose unit holds the key beside its builds
/// (`storage-and-daemon`): the key file, when the unit names it exactly, is a plain file,
/// root's, with no group or other bit. systemd reads it as root, so nothing else needs
/// to; any other owner can replace the key, and any other bit lets another user read it.
/// A key file that is not there, or that cannot be read, is a `SKIP credential:` line.
pub(super) fn root_key_on_host<E: Exec>(exec: &E, unit: &UnitFile, key: &HostPath) -> OnHost {
    let mut out = OnHost::default();
    if !given(unit)
        .files
        .iter()
        .any(|f| is_exactly(f, Path::new(key.as_str())))
    {
        return out;
    }
    let meta = match exec.stat(key) {
        Ok(Some(m)) => m,
        Ok(None) => {
            out.notes.push(format!(
                "SKIP credential: no key file at {key} yet, so its owner and mode were not \
                 checked"
            ));
            return out;
        }
        Err(e) => {
            out.notes.push(format!(
                "SKIP credential: cannot read {key} ({e}), so its owner and mode were not \
                 checked"
            ));
            return out;
        }
    };
    if meta.kind != Kind::File {
        out.failures
            .push(format!("{key} is not a plain file (a link or a directory)"));
        return out;
    }
    if meta.owner.uid != 0 {
        out.failures.push(format!(
            "{key} is uid {}'s, not root's: that user can replace the key",
            meta.owner.uid
        ));
    }
    let bits = meta.perms.bits();
    if bits & 0o077 != 0 {
        out.failures.push(format!(
            "{key} is mode {bits:04o}, so other users can read the key: make it 0600"
        ));
    }
    out
}

/// The one file the `EnvironmentFile=` path `path` (no wildcard, [`bare`]) names, when
/// systemd would read one: a relative path is one systemd ignores, and a name ending
/// in a lone `\`, or one that names a directory, is one its glob(3) lists no file for.
pub(super) fn env_file(path: &str) -> Option<HostPath> {
    let file = glob::literal(path).map(|p| normalise(&p).to_string_lossy().into_owned())?;
    HostPath::new(&file).ok()
}

/// The variable names an environment file assigns: each line's text before `=`, after
/// an `export ` (systemd ignores the line; the rule is stricter), skipping comments.
pub(super) fn env_file_names(bytes: &[u8]) -> impl Iterator<Item = String> + '_ {
    bytes.split(|b| *b == b'\n').filter_map(|raw| {
        let line = String::from_utf8_lossy(raw);
        let line = line.trim();
        if line.starts_with('#') || line.starts_with(';') {
            return None;
        }
        let (name, _) = line.split_once('=')?;
        let name = name.trim();
        Some(
            name.strip_prefix("export ")
                .unwrap_or(name)
                .trim()
                .to_owned(),
        )
    })
}

/// Whether the `EnvironmentFile=` value `value` is the path `key`, spelled as it is.
pub(super) fn is_exactly(value: &str, key: &Path) -> bool {
    Path::new(value) == key && value == key.to_string_lossy()
}

/// Whether the `EnvironmentFile=` value `value` names the key file `key`.
pub(super) fn names_key(value: &str, key: &Path) -> bool {
    let path = bare(value);
    glob::matches(path, &key.to_string_lossy())
        || glob::literal(path).is_some_and(|p| {
            p.starts_with('/') && p.rsplit('/').next() == Some(COLD_STORE_KEY_FILE)
        })
}

/// The path an `EnvironmentFile=` value names: without the `-` that makes it optional
/// and without quotes around it (systemd takes neither as part of the path), trimmed of
/// systemd's [`WHITESPACE`] only (U+00A0 is part of a path).
pub(super) fn bare(value: &str) -> &str {
    let v = value.trim_matches(WHITESPACE);
    let v = v.strip_prefix('-').unwrap_or(v);
    v.trim_matches(|c| c == '"' || c == '\'')
}

/// `path` with `.`, `..` and repeated `/` resolved by its words alone (no link read).
pub(super) fn normalise(path: &str) -> PathBuf {
    let mut out = PathBuf::new();
    for c in Path::new(path).components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            c => out.push(c),
        }
    }
    out
}

/// The words of a systemd value, unquoted as systemd.syntax(7) says: [`WHITESPACE`]
/// (not any Unicode space) separates words outside quotes, `'...'` and `"..."` group (anywhere in a word), and
/// `\` outside single quotes keeps the next character.
pub(super) fn words(value: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut word: Option<String> = None;
    let mut quote: Option<char> = None;
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (None, c) if WHITESPACE.contains(&c) => out.extend(word.take()),
            (None, '\'' | '"') => {
                quote = Some(c);
                word.get_or_insert_with(String::new);
            }
            (Some(q), c) if c == q => quote = None,
            (Some('\''), c) => word.get_or_insert_with(String::new).push(c),
            (_, '\\') => {
                let w = word.get_or_insert_with(String::new);
                w.extend(chars.next());
            }
            (_, c) => word.get_or_insert_with(String::new).push(c),
        }
    }
    out.extend(word);
    out
}

#[cfg(test)]
#[path = "credential_tests.rs"]
mod tests;
