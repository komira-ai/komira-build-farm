//! The host inventory: `/etc/kbf-updater/inventory`, root-owned, one host per line.
//!
//! ```text
//! # alias   address               role      host key (type and base64, as in known_hosts)  [node id]
//! fe1       fe1.farm.example:22   frontend  ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIExample
//! st1       st1.farm.example      storage   ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIExample
//! dm1       dm1.farm.example      daemon    ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIExample  dm1
//! mac1      mac1.farm.example     mac-daemon ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIExample  mac1
//! ```
//!
//! The roles are `frontend` (refused when a roll reaches it), `storage`, `daemon` (a Linux host that runs only a daemon) and
//! `mac-daemon` (a Mac that runs a daemon). A `mac-daemon` host is rolled like a `daemon`
//! host (it needs a node id, and its node is drained and given back) except for the
//! install: its `kbf-host-install` starts the apply and answers at once, and the roll then
//! polls the host until the apply is done ([`Role::MacDaemon`]).
//!
//! Blank lines and `#` lines are skipped. A line is five words, or six: the alias, the
//! address, the role, the key as its type and its base64, and optionally the node id the
//! host's kbf configuration names (a DNS label: what `kbf-host-install query` reports as
//! `node_id`, and what the frontend knows the node by). The roll checks it against the host
//! before it drains the node, and a daemon host without one is refused. The updater never
//! learns a host key by asking the host: the pinned key here is the one ssh must see.
//!
//! # Frontends
//!
//! A `frontend` line parses, and the real health check refuses it before the installer is
//! called (see [`crate::node_health`]): a frontend is a container that `kbf-host-install`
//! drains and recreates, and the updater does not roll one. The words `instance=` and
//! `base=`, which listed a frontend as a pool member in earlier builds, are
//! refused with the line number.
//!
//! The file decides which machines a roll may touch and as whom ssh logs in, so it is read
//! only if it is owned by the expected uid (root) and writable by no one else, and any
//! malformed line refuses the whole file: a half-read inventory would roll a different set
//! of hosts than the operator wrote.

use std::fs;
use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

use crate::api::{ApiError, ErrorCode, Hosts};

/// What a host runs, which decides its turn and its health bound.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// A frontend: ordered before the other hosts, no safe-to-stop, and refused by the real
    /// health check, which a roll asks before it installs.
    Frontend,
    /// A storage node, possibly with a daemon.
    Storage,
    /// A daemon-only Linux host.
    Daemon,
    /// A Mac that runs a daemon. Like [`Role::Daemon`] in everything but the install: its
    /// `install` answers `apply_started` and the apply goes on by itself, so the roll polls
    /// the host's `query` until the apply is done (or has failed, or the bound is out).
    MacDaemon,
}

impl Role {
    fn parse(s: &str) -> Option<Role> {
        match s {
            "frontend" => Some(Role::Frontend),
            "storage" => Some(Role::Storage),
            "daemon" => Some(Role::Daemon),
            "mac-daemon" => Some(Role::MacDaemon),
            _ => None,
        }
    }

    /// A host whose node the frontend knows and the roll drains: a daemon of either kind.
    pub fn is_daemon(self) -> bool {
        matches!(self, Role::Daemon | Role::MacDaemon)
    }
}

/// One host of the inventory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Host {
    pub alias: String,
    /// `host` or `host:port`; never starts with `-`, so ssh cannot read it as an option.
    pub address: String,
    pub role: Role,
    /// The pinned host key, `<type> <base64>`.
    pub host_key: String,
    /// The id the frontend knows the node by: what the host's `query` must report before the
    /// node is drained. Absent on a line that lists none.
    pub node_id: Option<String>,
}

/// Why an inventory was refused.
#[derive(Debug, thiserror::Error)]
pub enum InventoryError {
    #[error("cannot read the inventory: {0}")]
    Io(#[from] std::io::Error),
    #[error("the inventory is not a regular file (a symlink, a directory or a device)")]
    NotRegular,
    #[error("the inventory {what} is owned by uid {found}, not {expected}")]
    Owner {
        what: &'static str,
        found: u32,
        expected: u32,
    },
    #[error("the inventory {what} is writable by group or others (mode {mode:o})")]
    Mode { what: &'static str, mode: u32 },
    #[error("line {line}: {reason}")]
    Line { line: usize, reason: String },
    #[error("the inventory has no hosts")]
    Empty,
}

const KEY_TYPES: &[&str] = &[
    "ssh-ed25519",
    "ecdsa-sha2-nistp256",
    "ecdsa-sha2-nistp384",
    "ecdsa-sha2-nistp521",
    "ssh-rsa",
];

/// The parsed inventory: hosts in file order, aliases unique, and at most one frontend.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Inventory {
    hosts: Vec<Host>,
}

impl Inventory {
    /// Read the file at `path`, which must be a regular file (a symlink is refused) owned by
    /// `owner_uid` and not writable by group or others, in a directory owned by `owner_uid`
    /// and not writable by group or others (else anyone who may write there may swap the
    /// file). The file is opened once and every check is on the open handle, so the bytes
    /// read are the bytes checked; the directory is checked on its own open handle.
    pub fn load(path: &Path, owner_uid: u32) -> Result<Inventory, InventoryError> {
        let mut file = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)
            .map_err(|e| match e.raw_os_error() {
                Some(libc::ELOOP) => InventoryError::NotRegular,
                _ => InventoryError::Io(e),
            })?;
        let meta = file.metadata()?;
        if !meta.is_file() {
            return Err(InventoryError::NotRegular);
        }
        check_owner_and_mode(&meta, owner_uid, "file")?;
        let parent = match path.parent() {
            Some(p) if !p.as_os_str().is_empty() => p,
            _ => Path::new("."),
        };
        let dir = fs::File::open(parent)?.metadata()?;
        check_owner_and_mode(&dir, owner_uid, "directory")?;
        let mut text = String::new();
        file.read_to_string(&mut text)?;
        Inventory::parse(&text)
    }

    pub fn parse(text: &str) -> Result<Inventory, InventoryError> {
        let mut hosts: Vec<Host> = Vec::new();
        for (n, raw) in text.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let at = |reason: String| InventoryError::Line {
                line: n + 1,
                reason,
            };
            let host = parse_line(line).map_err(at)?;
            if hosts.iter().any(|h| h.alias == host.alias) {
                return Err(at(format!("alias {} is listed twice", host.alias)));
            }
            if host.role == Role::Frontend && hosts.iter().any(|h| h.role == Role::Frontend) {
                return Err(at("a second frontend".to_owned()));
            }
            hosts.push(host);
        }
        if hosts.is_empty() {
            return Err(InventoryError::Empty);
        }
        Ok(Inventory { hosts })
    }

    pub fn host(&self, alias: &str) -> Option<&Host> {
        self.hosts.iter().find(|h| h.alias == alias)
    }

    /// The hosts of a roll in the order they are touched: the frontend first, then the rest
    /// one at a time, in inventory order for "all" and in the order given for a list. An
    /// alias that is not in the inventory, or is listed twice, is refused here, before
    /// anything runs.
    pub fn plan(&self, hosts: &Hosts) -> Result<Vec<&Host>, ApiError> {
        let mut chosen: Vec<&Host> = match hosts {
            Hosts::All(_) => self.hosts.iter().collect(),
            Hosts::Aliases(aliases) => {
                let mut out: Vec<&Host> = Vec::new();
                for alias in aliases {
                    let host = self.host(alias).ok_or_else(|| {
                        ApiError::new(
                            ErrorCode::UnknownAlias,
                            format!("no host with the alias {alias:?} in the inventory"),
                        )
                    })?;
                    if out.iter().any(|h| h.alias == host.alias) {
                        return Err(ApiError::new(
                            ErrorCode::BadRequest,
                            format!("the alias {alias:?} is listed twice"),
                        ));
                    }
                    out.push(host);
                }
                out
            }
        };
        // Stable: the frontend moves to the front, everything else keeps its order.
        chosen.sort_by_key(|h| h.role != Role::Frontend);
        Ok(chosen)
    }
}

fn check_owner_and_mode(
    meta: &fs::Metadata,
    owner_uid: u32,
    what: &'static str,
) -> Result<(), InventoryError> {
    if meta.uid() != owner_uid {
        return Err(InventoryError::Owner {
            what,
            found: meta.uid(),
            expected: owner_uid,
        });
    }
    let mode = meta.mode() & 0o777;
    if mode & 0o022 != 0 {
        return Err(InventoryError::Mode { what, mode });
    }
    Ok(())
}

fn parse_line(line: &str) -> Result<Host, String> {
    let words: Vec<&str> = line.split_whitespace().collect();
    // The five words and the node id come first. The key, the fifth, may end in `=` as base64
    // does, so the `name=value` words an earlier build read are looked for after it.
    let named = words.iter().skip(5).position(|w| w.contains('='));
    let (words, fields) = words.split_at(named.map_or(words.len(), |i| i + 5));
    if let Some(field) = fields.first() {
        return Err(format!(
            "{field:?}: instance= and base= listed a frontend as a pool member, and the updater \
             no longer rolls frontends; remove them (and the frontend line)"
        ));
    }
    let (head, node_id) = match words[..] {
        [a, b, c, d, e] => ([a, b, c, d, e], None),
        [a, b, c, d, e, id] => ([a, b, c, d, e], Some(id)),
        _ => {
            return Err(format!(
                "expected alias, address, role, key type, key and optionally a node id; found {} words",
                words.len()
            ));
        }
    };
    let [alias, address, role, key_type, key] = head;
    if !valid_alias(alias) {
        return Err(format!(
            "alias {alias:?} must be 1..=63 of letters, digits, _ and -, starting with a letter or digit"
        ));
    }
    if !valid_address(address) {
        return Err(format!("address {address:?} is not a host or host:port"));
    }
    let role = Role::parse(role)
        .ok_or_else(|| format!("role {role:?} is not frontend, storage, daemon or mac-daemon"))?;
    if !KEY_TYPES.contains(&key_type) {
        return Err(format!(
            "host key type {key_type:?} is not a supported type"
        ));
    }
    if !key
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'='))
    {
        return Err("the host key is not base64".to_owned());
    }
    if let Some(id) = node_id
        && !valid_node_id(id)
    {
        return Err(format!(
            "node id {id:?} must be a DNS label: 1..=63 of a-z, 0-9 and -, not starting or ending with -"
        ));
    }
    Ok(Host {
        alias: alias.to_owned(),
        address: address.to_owned(),
        role,
        host_key: format!("{key_type} {key}"),
        node_id: node_id.map(str::to_owned),
    })
}

/// A node id as the host reports one and the frontend knows one: a DNS label,
/// `^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$` (the grammar `kbf-host-install` reads it with).
pub(crate) fn valid_node_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 63
        && !s.starts_with('-')
        && !s.ends_with('-')
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn valid_alias(s: &str) -> bool {
    let mut bytes = s.bytes();
    bytes.next().is_some_and(|b| b.is_ascii_alphanumeric())
        && s.len() <= 63
        && bytes.all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
}

/// Whether `s` is a pinned host key as the inventory reads one: a supported type, one space,
/// base64. For a host not made by the parser (a line break in a key would be a second
/// `known_hosts` line).
pub(crate) fn valid_host_key(s: &str) -> bool {
    let Some((key_type, key)) = s.split_once(' ') else {
        return false;
    };
    KEY_TYPES.contains(&key_type)
        && !key.is_empty()
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'='))
}

pub(crate) fn valid_address(s: &str) -> bool {
    let (host, port) = match s.rsplit_once(':') {
        Some((h, p)) => (h, Some(p)),
        None => (s, None),
    };
    let host_ok = host
        .bytes()
        .next()
        .is_some_and(|b| b.is_ascii_alphanumeric())
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'));
    let port_ok = port.is_none_or(|p| {
        p.bytes().all(|b| b.is_ascii_digit()) && p.parse::<u16>().is_ok_and(|n| n > 0)
    });
    host_ok && port_ok
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::All;
    use std::os::unix::fs::PermissionsExt;

    const KEY: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIExample";

    fn file(lines: &[&str]) -> String {
        lines.join("\n")
    }

    fn sample() -> Inventory {
        Inventory::parse(&file(&[
            "# the farm",
            "",
            &format!("st1 st1.farm.example storage {KEY}"),
            &format!("fe1 fe1.farm.example:2222 frontend {KEY}"),
            &format!("  dm1\tdm1.farm.example  daemon  {KEY}  dm-1  "),
            &format!("st2 st2.farm.example storage {KEY}"),
            &format!("mac1 mac1.farm.example mac-daemon {KEY} mac-1"),
        ]))
        .expect("inventory")
    }

    fn aliases(plan: &[&Host]) -> Vec<String> {
        plan.iter().map(|h| h.alias.clone()).collect()
    }

    /// Catches the Mac role named wrongly or taken for a daemon: only `mac-daemon` is the Mac's
    /// role (not `mac`, `macos` or `Mac-Daemon`), both daemon roles are the roll's daemons, and
    /// a frontend and a storage host are not.
    #[test]
    fn the_mac_daemon_role_is_its_own() {
        for word in ["mac", "macos", "Mac-Daemon", "mac_daemon", "mac-daemon2"] {
            let e = line_error(&format!("m m.farm.example {word} {KEY} m-1"));
            assert!(
                e.contains("is not frontend, storage, daemon or mac-daemon"),
                "{e}"
            );
        }
        let daemons =
            [Role::Frontend, Role::Storage, Role::Daemon, Role::MacDaemon].map(Role::is_daemon);
        assert_eq!(daemons, [false, false, true, true]);
        assert_ne!(Role::MacDaemon, Role::Daemon);
    }

    /// The refusal as text: `line N: reason`.
    fn line_error(text: &str) -> String {
        Inventory::parse(text).expect_err("refused").to_string()
    }

    /// Catches a reader that drops a field or reads the wrong column: every field of every
    /// host, with comments, blank lines and mixed whitespace skipped.
    #[test]
    fn a_well_formed_inventory_reads_every_field() {
        let inv = sample();
        let fe = inv.host("fe1").expect("fe1");
        assert_eq!(fe.address, "fe1.farm.example:2222");
        assert_eq!(fe.role, Role::Frontend);
        assert_eq!(fe.host_key, KEY);
        assert_eq!(inv.host("dm1").expect("dm1").role, Role::Daemon);
        let mac = inv.host("mac1").expect("mac1");
        assert_eq!(mac.role, Role::MacDaemon);
        assert_eq!(mac.node_id.as_deref(), Some("mac-1"));
        assert_eq!(
            inv.host("dm1").expect("dm1").node_id.as_deref(),
            Some("dm-1")
        );
        assert_eq!(fe.node_id, None);
        assert_eq!(inv.host("st1").expect("st1").role, Role::Storage);
        assert!(inv.host("nope").is_none());
    }

    /// Catches an unknown alias slipping past as a skipped host: it must be an error before
    /// anything runs, whether the list is all unknown, mixed, or the unknown one is last.
    #[test]
    fn an_unknown_alias_is_refused_before_anything_runs() {
        let inv = sample();
        for list in [vec!["ghost"], vec!["st1", "ghost"], vec!["ghost", "st1"]] {
            let hosts = Hosts::Aliases(list.iter().map(|s| (*s).to_owned()).collect());
            let err = inv.plan(&hosts).expect_err("refused");
            assert_eq!(err.code, ErrorCode::UnknownAlias, "{list:?}");
            assert!(err.message.contains("ghost"));
        }
    }

    /// Catches an order that is not frontend first, then one host at a time: "all" is the
    /// inventory order with the frontend moved up, a list keeps the caller's order after it.
    #[test]
    fn the_plan_puts_the_frontend_first_and_keeps_the_rest_in_order() {
        let inv = sample();
        let all = inv.plan(&Hosts::All(All::All)).expect("all");
        assert_eq!(aliases(&all), ["fe1", "st1", "dm1", "st2", "mac1"]);
        let list = Hosts::Aliases(vec!["st2".into(), "fe1".into(), "st1".into()]);
        assert_eq!(
            aliases(&inv.plan(&list).expect("list")),
            ["fe1", "st2", "st1"]
        );
        let nodes = Hosts::Aliases(vec!["dm1".into(), "st2".into()]);
        assert_eq!(aliases(&inv.plan(&nodes).expect("nodes")), ["dm1", "st2"]);
    }

    /// Catches a host rolled twice by a repeated alias.
    #[test]
    fn a_repeated_alias_in_a_list_is_refused() {
        let hosts = Hosts::Aliases(vec!["st1".into(), "st1".into()]);
        let err = sample().plan(&hosts).expect_err("refused");
        assert_eq!(err.code, ErrorCode::BadRequest);
    }

    /// Catches a lenient reader: each malformed line is a refusal of the whole file, with the
    /// line number, and none is half-read.
    #[test]
    fn a_malformed_line_refuses_the_file() {
        let ok = format!("fe1 fe1.example frontend {KEY}");
        let cases: [(&str, &str); 18] = [
            ("fe1 fe1.example frontend", "found 3 words"),
            (&format!("{ok} extra more"), "found 7 words"),
            (&format!("{ok} Bad_Id"), "node id"),
            (&format!("{ok} -x"), "node id"),
            (&format!("{ok} x-"), "node id"),
            (&format!("{ok} {}", "a".repeat(64)), "node id"),
            (&format!("-x fe1.example storage {KEY}"), "alias"),
            (&format!("a/b fe1.example storage {KEY}"), "alias"),
            (
                &format!("{} fe1.example storage {KEY}", "a".repeat(64)),
                "alias",
            ),
            (&format!("a -oProxyCommand=x storage {KEY}"), "address"),
            (&format!("a host:0 storage {KEY}"), "address"),
            (&format!("a host:99999x storage {KEY}"), "address"),
            (&format!("a host: storage {KEY}"), "address"),
            (&format!("a h;st storage {KEY}"), "address"),
            (&format!("a h.example admin {KEY}"), "role"),
            ("a h.example storage ssh-dss AAAA", "key type"),
            ("a h.example storage ssh-ed25519 AA!A", "base64"),
            ("a h.example storage ssh-ed25519 AA\u{e9}A", "base64"),
        ];
        for (text, want) in cases {
            let got = line_error(&format!("# header\n{text}"));
            assert!(got.starts_with("line 2: "), "{text}: {got}");
            assert!(got.contains(want), "{text}: {got}");
        }
    }

    /// Catches a duplicate alias (the second would shadow or double-roll a host), a second
    /// frontend (two hosts both claiming the swap), and an inventory with nothing in it.
    #[test]
    fn duplicates_a_second_frontend_and_an_empty_file_are_refused() {
        let a = format!("a a.example storage {KEY}");
        assert!(line_error(&format!("{a}\n{a}")).contains("listed twice"));
        let f1 = format!("f1 f1.example frontend {KEY}");
        let f2 = format!("f2 f2.example frontend {KEY}");
        assert!(line_error(&format!("{f1}\n{f2}")).contains("second frontend"));
        assert!(matches!(
            Inventory::parse("# nothing\n\n"),
            Err(InventoryError::Empty)
        ));
    }

    /// Asserts that the words that listed a frontend as a pool member are refused with the line
    /// number and a reason, on a frontend line or any other, and that a host key ending in `=`
    /// (base64 padding) is not taken for one.
    #[test]
    fn the_pool_member_words_are_refused() {
        for fields in [
            "instance=a base=18000",
            "base=18000 instance=a",
            "instance=a",
            "base=18000",
            "port=1",
        ] {
            for role in ["frontend", "storage"] {
                let got = line_error(&format!("f f.farm.example {role} {KEY} {fields}"));
                assert!(got.starts_with("line 1: "), "{fields}: {got}");
                assert!(got.contains("no longer rolls frontends"), "{fields}: {got}");
            }
        }
        let padded =
            Inventory::parse("padded fe.farm.example storage ssh-rsa AAAAB3NzaC1yc2E= padded-node")
                .expect("a key with padding");
        let padded = padded.host("padded").expect("padded");
        assert_eq!(padded.host_key, "ssh-rsa AAAAB3NzaC1yc2E=");
        assert_eq!(padded.node_id.as_deref(), Some("padded-node"));
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/updater-scratch")
            .join(format!("inv-{name}-{}", std::process::id()));
        fs::create_dir_all(&dir).expect("scratch");
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).expect("chmod");
        dir
    }

    /// Catches an inventory read from a file anyone could have edited: another owner, group
    /// write and world write are each refused; a root-style 0644 owned by the expected uid
    /// is read.
    #[test]
    fn load_checks_owner_and_mode() {
        let path = scratch("load").join("inventory");
        fs::write(&path, format!("a a.example storage {KEY}\n")).expect("write");
        let mine = fs::metadata(&path).expect("meta").uid();
        for (mode, ok) in [(0o644, true), (0o600, true), (0o664, false), (0o646, false)] {
            fs::set_permissions(&path, fs::Permissions::from_mode(mode)).expect("chmod");
            let got = Inventory::load(&path, mine);
            assert_eq!(got.is_ok(), ok, "mode {mode:o}: {got:?}");
            if !ok {
                let want =
                    format!("the inventory file is writable by group or others (mode {mode:o})");
                assert_eq!(got.expect_err("refused").to_string(), want);
            }
        }
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("chmod");
        let other = Inventory::load(&path, mine + 1).expect_err("owner");
        assert_eq!(
            other.to_string(),
            format!(
                "the inventory file is owned by uid {mine}, not {}",
                mine + 1
            )
        );
        let missing = Inventory::load(&path.with_extension("none"), mine).expect_err("missing");
        assert!(matches!(missing, InventoryError::Io(_)));
    }

    /// Catches the check-then-read race and a file that is not a file: a symlink (even to a
    /// good inventory), a directory and a device are each `NotRegular` and never read.
    #[test]
    fn load_refuses_a_symlink_and_anything_that_is_not_a_regular_file() {
        let dir = scratch("kinds");
        let good = dir.join("inventory");
        fs::write(&good, format!("a a.example storage {KEY}\n")).expect("write");
        fs::set_permissions(&good, fs::Permissions::from_mode(0o644)).expect("chmod");
        let mine = fs::metadata(&good).expect("meta").uid();
        assert!(Inventory::load(&good, mine).is_ok());
        let link = dir.join("link");
        let _ = fs::remove_file(&link);
        std::os::unix::fs::symlink(&good, &link).expect("symlink");
        let sub = dir.join("sub");
        fs::create_dir_all(&sub).expect("sub");
        for path in [link.as_path(), sub.as_path(), Path::new("/dev/null")] {
            let got = Inventory::load(path, mine);
            assert!(
                matches!(got, Err(InventoryError::NotRegular)),
                "{path:?}: {got:?}"
            );
        }
    }

    /// Catches an inventory whose directory anyone may write to (they could swap the file):
    /// group or world write on the parent is refused, with the directory named.
    #[test]
    fn load_refuses_a_writable_parent_directory() {
        let dir = scratch("parent");
        let path = dir.join("inventory");
        fs::write(&path, format!("a a.example storage {KEY}\n")).expect("write");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("chmod");
        let mine = fs::metadata(&path).expect("meta").uid();
        for (mode, ok) in [(0o755, true), (0o775, false), (0o757, false)] {
            fs::set_permissions(&dir, fs::Permissions::from_mode(mode)).expect("chmod");
            let got = Inventory::load(&path, mine);
            assert_eq!(got.is_ok(), ok, "mode {mode:o}: {got:?}");
            if !ok {
                let want = format!(
                    "the inventory directory is writable by group or others (mode {mode:o})"
                );
                assert_eq!(got.expect_err("refused").to_string(), want);
            }
        }
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).expect("chmod");
        // A bare file name has the current directory as its parent.
        let cwd = std::env::current_dir().expect("cwd");
        std::env::set_current_dir(&dir).expect("cd");
        let bare = Inventory::load(Path::new("inventory"), mine);
        std::env::set_current_dir(cwd).expect("back");
        assert!(bare.is_ok(), "{bare:?}");
    }
}
