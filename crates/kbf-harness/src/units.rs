//! The processes of a farm, started as the packaged units start `kbf`: `kbf --mode=<mode>`,
//! with the settings in a `kbf.toml` the harness writes and `--config` names.
//!
//! The command line is the shape of the packaged unit (`packaging/linux/units/kbf-<mode>.service`,
//! `kbf-frontend.service` for the frontend): its `--mode=<name>` word, read with
//! `kbf_unitfile`, and the test's `kbf` in place of `/usr/bin/kbf`. Every other
//! setting is a key of the process's `kbf.toml`, and a [`Unit`] is that file while a farm
//! builds it.
//!
//! A [`Unit`] starts from the settings the farm gives a mode for its directories, ports and
//! addresses ([`Unit::new`]; kbf-cli's `tests/golden/farm-units.txt` records, per process,
//! the flags they came to, and its tests hold the file to them), then the farm changes
//! it with [`Unit::replace`], [`Unit::remove`], [`Unit::set`] and [`Unit::append`], by
//! flag name (`--cold-store=file:///...`). The edits are strict, so a setting that stops
//! existing is not hidden: a flag that is not one of the mode's is refused (the keys of a
//! mode are those of `kbf-cli`'s `tests/golden/kbf-<mode>.toml`, which that crate's tests
//! hold equal to the flags the mode takes), and so is a `replace` or `remove` of a setting
//! the unit does not have and an `append` of one it has.

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;

use kbf_unitfile::UnitFile;

use crate::node_support::{cgroup_tree, id_files, oci_runtime};
use crate::topology::{Driver, NodeMode, Role, Topology};

/// The packaged unit of each mode the farm starts, and the `kbf.toml` of each with every
/// setting of the mode (`kbf-cli`'s golden files, generated from the flag definitions).
const FRONTEND_UNIT: &str = include_str!("../../../packaging/linux/units/kbf-frontend.service");
const STORAGE_UNIT: &str = include_str!("../../../packaging/linux/units/kbf-storage.service");
const STORAGE_AND_DAEMON_UNIT: &str =
    include_str!("../../../packaging/linux/units/kbf-storage-and-daemon.service");
const DAEMON_UNIT: &str = include_str!("../../../packaging/linux/units/kbf-daemon.service");
const FRONTEND_KEYS: &str = include_str!("../../kbf-cli/tests/golden/kbf-frontend.toml");
const STORAGE_KEYS: &str = include_str!("../../kbf-cli/tests/golden/kbf-storage.toml");
const STORAGE_AND_DAEMON_KEYS: &str =
    include_str!("../../kbf-cli/tests/golden/kbf-storage-and-daemon.toml");
const DAEMON_KEYS: &str = include_str!("../../kbf-cli/tests/golden/kbf-daemon.toml");

/// The modes a farm starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Frontend,
    Storage,
    StorageAndDaemon,
    Daemon,
}

impl Mode {
    /// Every mode a farm starts.
    pub const ALL: [Mode; 4] = [
        Mode::Frontend,
        Mode::Storage,
        Mode::StorageAndDaemon,
        Mode::Daemon,
    ];

    /// The mode's name, as `kbf --mode=` takes it.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Mode::Frontend => "frontend",
            Mode::Storage => "storage",
            Mode::StorageAndDaemon => "storage-and-daemon",
            Mode::Daemon => "daemon",
        }
    }

    /// The text of the packaged unit that starts the mode.
    fn unit(self) -> &'static str {
        match self {
            Mode::Frontend => FRONTEND_UNIT,
            Mode::Storage => STORAGE_UNIT,
            Mode::StorageAndDaemon => STORAGE_AND_DAEMON_UNIT,
            Mode::Daemon => DAEMON_UNIT,
        }
    }

    /// The `kbf.toml` with every setting of the mode.
    fn keys(self) -> &'static str {
        match self {
            Mode::Frontend => FRONTEND_KEYS,
            Mode::Storage => STORAGE_KEYS,
            Mode::StorageAndDaemon => STORAGE_AND_DAEMON_KEYS,
            Mode::Daemon => DAEMON_KEYS,
        }
    }
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Whether `argv` (a unit's `ExecStart=` without the binary) names `mode` as its one
/// `--mode=<name>` word, the form every packaged unit has. The old spelling (`kbf storage`
/// as the first word), `--mode <name>` as two words, and a second `--mode=` are not
/// accepted: the day a unit starts `kbf` another way, this check fails the farm loudly at
/// the line that reads it, where the one edit is.
#[must_use]
pub fn names_mode(argv: &[&str], mode: Mode) -> bool {
    let modes: Vec<&&str> = argv.iter().filter(|w| w.starts_with("--mode")).collect();
    modes.len() == 1 && *modes[0] == format!("--mode={}", mode.name())
}

/// The `--mode=<name>` word of the packaged unit of `mode`.
///
/// # Panics
/// When the unit does not start `kbf` with exactly that word (a defect of the unit).
fn mode_word(mode: Mode) -> String {
    mode_word_in(mode.unit(), mode)
}

/// [`mode_word`] over the text of a unit.
fn mode_word_in(text: &str, mode: Mode) -> String {
    let unit = UnitFile::parse(text).expect("a packaged unit");
    let exec = unit.exec_start();
    assert!(
        exec.first().is_some_and(|kbf| kbf.ends_with("/kbf")) && names_mode(&exec[1..], mode),
        "{mode}: the unit does not start `kbf --mode={mode}`: {exec:?}"
    );
    format!("--mode={}", mode.name())
}

/// The table of `kbf.toml` that holds each setting of `mode`, by key: read from the file
/// with every setting (a live line `key = ...` or a commented one `# key = ...`, under
/// its `[table]`), and whether the key is a switch (`key = false`, the file's shape of a
/// flag that takes no value).
pub fn keys(mode: Mode) -> BTreeMap<&'static str, (&'static str, bool)> {
    parse_keys(mode.keys())
}

/// [`keys`] over the text of a file with every setting.
fn parse_keys(text: &'static str) -> BTreeMap<&'static str, (&'static str, bool)> {
    let mut found = BTreeMap::new();
    let mut table = "";
    for line in text.lines() {
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            table = name;
            continue;
        }
        let setting = line.strip_prefix("# ").unwrap_or(line);
        let key = setting.split_once(" = ").map(|(key, _)| key);
        // A line of a setting's help may hold ` = ` too: a key is a flag's long name.
        if let Some(key) = key.filter(|k| k.bytes().all(is_flag_byte)) {
            found.insert(key, (table, line == format!("{key} = false")));
        }
    }
    found
}

/// Whether `b` may be in a flag's long name.
fn is_flag_byte(b: u8) -> bool {
    b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'
}

/// The value of one setting.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Setting {
    /// `--key=value`: any scalar `kbf` reads as the flag's text.
    Text(String),
    /// `--key`: a switch that is on.
    On,
}

/// The ports and addresses of a farm that every process is told.
pub struct Wiring {
    pub reapi: u16,
    /// The frontend's worker listener.
    pub worker: u16,
    pub api: u16,
    /// The storage URL the frontend dials: node 1's.
    pub storage_url: String,
}

/// Where one process lives and what it dials: its own address, node id, directories under
/// `root` and storage port, and the farm's [`Wiring`].
pub struct Settings<'a> {
    pub root: &'a Path,
    pub host: &'a str,
    pub node_id: &'a str,
    pub storage_port: u16,
    pub wiring: &'a Wiring,
    /// The image the daemon half is pinned to (`repo@sha256:...`), or empty for none.
    pub image: &'a str,
}

/// The name every dialler of the frontend uses for it (a certificate names it).
const FRONTEND_NAME: &str = "localhost";

/// The memory a daemon leaves to actions: the RAM given (16 GiB) less the reserved shares
/// (1 and 1) and the OS headroom (4).
const ACTIONS_MEMORY_MAX_GIB: u32 = 10;

/// The daemon's own memory floor, in MiB.
const SUPERVISOR_MEMORY_MIN_MIB: u32 = 256;

/// The `kbf.toml` of one process: its mode's settings, by key, and the environment its
/// unit gave it.
#[derive(Clone, Debug)]
pub struct Unit {
    pub mode: Mode,
    settings: BTreeMap<String, Setting>,
    pub env: Vec<(String, String)>,
}

/// A flag as a test gives it, `--name=value` or `--name`, taken apart.
fn flag_parts(flag: &str) -> (&str, Option<&str>) {
    let flag = flag.strip_prefix("--").unwrap_or(flag);
    match flag.split_once('=') {
        Some((name, value)) => (name, Some(value)),
        None => (flag, None),
    }
}

impl Unit {
    /// The unit of `mode` for the process `s` describes, run by the user `uid`: the
    /// settings the farm gives it for the directories, ports and addresses of `s`.
    ///
    /// # Panics
    /// When a setting it names is not one of the mode's keys.
    #[must_use]
    pub fn new(mode: Mode, s: &Settings, uid: u32) -> Self {
        let root = s.root.display();
        let etc = format!("{root}/etc");
        let frontend = format!("https://{FRONTEND_NAME}:{}", s.wiring.worker);
        let storage = vec![
            "--cold-store=s3://kbf-cas?endpoint=http://127.0.0.1:9000&conditional-put=true"
                .to_owned(),
            format!("--hot-store=file://{root}/hot"),
            format!("--meta-dir={root}/meta"),
            format!("--storage-listen={}:{}", s.host, s.storage_port),
            format!("--storage-tls-cert={etc}/storage.pem"),
            format!("--storage-tls-key={etc}/storage.key"),
            format!("--storage-client-ca={etc}/ca.pem"),
        ];
        let mut daemon = vec![
            "--driver=container".to_owned(),
            "--oci-runtime=/usr/bin/crun".to_owned(),
            format!("--server={frontend}"),
            format!("--ca-cert={etc}/ca.pem"),
            format!("--cert={etc}/node.pem"),
            format!("--key={etc}/node.key"),
            format!("--node-id={}", s.node_id),
            format!("--data-dir={root}/data"),
            format!("--actions-memory-max-gib={ACTIONS_MEMORY_MAX_GIB}"),
            format!("--supervisor-memory-min-mib={SUPERVISOR_MEMORY_MIN_MIB}"),
        ];
        if !s.image.is_empty() {
            daemon.push(format!("--image=docker://{}", s.image));
        }
        let flags = match mode {
            Mode::Frontend => vec![
                format!("--listen={}:{}", s.host, s.wiring.reapi),
                format!("--reapi-tls-cert={etc}/reapi.pem"),
                format!("--reapi-tls-key={etc}/reapi.key"),
                format!("--worker-listen={}:{}", s.host, s.wiring.worker),
                format!("--worker-tls-cert={etc}/worker.pem"),
                format!("--worker-tls-key={etc}/worker.key"),
                format!("--worker-client-ca={etc}/ca.pem"),
                format!("--api-listen=127.0.0.1:{}", s.wiring.api),
                format!("--api-token-file={etc}/api-token"),
                format!("--storage={}", s.wiring.storage_url),
                format!("--storage-client-cert={etc}/worker.pem"),
                format!("--storage-client-key={etc}/worker.key"),
                format!("--storage-ca={etc}/ca.pem"),
                "--state-store=postgres:///kbf?host=/run/postgresql".to_owned(),
            ],
            Mode::Storage => storage,
            Mode::StorageAndDaemon => {
                let mut flags = storage;
                flags.push(format!("--frontend={frontend}"));
                flags.push(format!("--advertise-addr={}:{}", s.host, s.storage_port));
                flags.extend(daemon);
                flags
            }
            Mode::Daemon => {
                daemon.push(format!("--cas={frontend}"));
                daemon
            }
        };
        // The daemon half runs rootless containers: its unit's session directory.
        let env = match mode {
            Mode::Frontend | Mode::Storage => Vec::new(),
            Mode::StorageAndDaemon | Mode::Daemon => {
                vec![("XDG_RUNTIME_DIR".to_owned(), format!("/run/user/{uid}"))]
            }
        };
        Unit {
            mode,
            settings: BTreeMap::new(),
            env,
        }
        .append(&flags)
    }

    /// The key and setting `flag` stands for.
    ///
    /// # Panics
    /// When the flag is not one of the mode's, or is given in the wrong form (a switch with
    /// a value, a flag that takes one without).
    fn parse(&self, flag: &str) -> (String, Setting) {
        let (name, value) = flag_parts(flag);
        let Some((_, switch)) = keys(self.mode).get(name).copied() else {
            panic!("kbf {} takes no --{name}", self.mode);
        };
        match (switch, value) {
            (true, None) => (name.to_owned(), Setting::On),
            (false, Some(value)) => (name.to_owned(), Setting::Text(value.to_owned())),
            (true, Some(_)) => panic!("--{name} is a switch: give it as --{name}"),
            (false, None) => panic!("--{name} takes a value: give --{name}=<value>"),
        }
    }

    /// Gives each flag in `flags` the value it names, in place of the unit's.
    ///
    /// # Panics
    /// When a flag is not one of the mode's, or the unit sets none of that name.
    pub fn replace(mut self, flags: &[String]) -> Self {
        for flag in flags {
            let (key, setting) = self.parse(flag);
            let Some(at) = self.settings.get_mut(&key) else {
                panic!("{} sets no --{key}", self.mode);
            };
            *at = setting;
        }
        self
    }

    /// Takes out the flags named `names` (`--image`, which only the container driver
    /// takes): each must be set, so a setting the unit stops having is not hidden.
    ///
    /// # Panics
    /// When a name is not one of the mode's flags, or the unit sets none of it.
    #[must_use]
    pub fn remove(mut self, names: &[&str]) -> Self {
        for flag in names {
            let (key, _) = flag_parts(flag);
            assert!(
                keys(self.mode).contains_key(key),
                "kbf {} takes no --{key}",
                self.mode
            );
            assert!(
                self.settings.remove(key).is_some(),
                "{} sets no --{key}",
                self.mode
            );
        }
        self
    }

    /// Gives each flag in `flags` the value it names: in place of the unit's if it sets
    /// the flag, else added. For limits a unit does not set but may take.
    ///
    /// # Panics
    /// When a flag is not one of the mode's.
    #[must_use]
    pub fn set(mut self, flags: &[String]) -> Self {
        for flag in flags {
            let (key, setting) = self.parse(flag);
            self.settings.insert(key, setting);
        }
        self
    }

    /// Adds flags the unit does not set (the test box's own).
    ///
    /// # Panics
    /// When a flag is not one of the mode's, or the unit sets it already.
    pub fn append(mut self, flags: &[String]) -> Self {
        for flag in flags {
            let (key, setting) = self.parse(flag);
            let old = self.settings.insert(key.clone(), setting);
            assert!(old.is_none(), "{} already sets --{key}", self.mode);
        }
        self
    }

    /// The settings as flags, `--key=value` or `--key`, by key.
    #[must_use]
    pub fn flags(&self) -> Vec<String> {
        self.settings
            .iter()
            .map(|(key, setting)| match setting {
                Setting::Text(value) => format!("--{key}={value}"),
                Setting::On => format!("--{key}"),
            })
            .collect()
    }

    /// The `kbf.toml`: each setting under the table of its group, as `kbf` reads it. A
    /// value is the flag's text, so `kbf` takes it as it takes `--key=value`.
    #[must_use]
    pub fn toml(&self) -> String {
        let keys = keys(self.mode);
        let mut tables: BTreeMap<&str, toml::Table> = BTreeMap::new();
        for (key, setting) in &self.settings {
            let value = match setting {
                Setting::Text(text) => toml::Value::String(text.clone()),
                Setting::On => toml::Value::Boolean(true),
            };
            let (table, _) = keys[key.as_str()];
            tables.entry(table).or_default().insert(key.clone(), value);
        }
        let mut root = toml::Table::new();
        for (table, body) in tables {
            root.insert(table.to_owned(), toml::Value::Table(body));
        }
        root.to_string()
    }

    /// The command line of the process, without the program: the packaged unit's
    /// `--mode=<name>` and `--config` with the file `config` names (a flag, as
    /// `kbf_testkit::config::flag_with` returns it).
    #[must_use]
    pub fn argv(&self, config: String) -> Vec<String> {
        vec![mode_word(self.mode), config]
    }
}

/// The flag that makes a farm's frontend take the node as its own (the nodes are this
/// host, whatever its architecture): the unit's default (`linux/x86_64`) leaves an action
/// that names no architecture with no worker on an aarch64 host.
const ONE_HOST_PLATFORM: &str = "--default-platform=none";

/// The flag that ends a farm's frontend without the wait a load balancer needs: a stop
/// (SIGTERM) keeps `/readyz` at 503 and serves on for `--drain-lb-wait` (10 s) so that a
/// load balancer's health check sees it, and the farm has none, while its stop is bounded
/// ([`crate::procs::HANG`]). Only a frontend of this release is given it: an older release
/// does not know the flag.
const NO_LB_WAIT: &str = "--drain-lb-wait=0";

/// What the container driver's nodes get from the box: the stand-in `crun`
/// (`--oci-runtime=`), the planted cgroup tree and the id files.
pub struct Container {
    pub oci_runtime: String,
    pub flags: Vec<String>,
}

impl Container {
    /// The box's container driver for the node whose files are under `root` and whose
    /// stand-in `crun` is in `bin` ([`crate::node_support`]); makes the cgroup tree.
    #[must_use]
    pub fn planted(bin: &Path, root: &Path) -> Self {
        let mut flags = cgroup_tree(bin);
        flags.push(format!("--id-files={}", id_files(root).display()));
        Container {
            oci_runtime: oci_runtime(bin),
            flags,
        }
    }
}

/// `unit` with the flags the topology gives `role` set on top of everything else.
fn overrides(topo: &Topology, role: Role, unit: Unit) -> Unit {
    let flags: Vec<String> = topo
        .overrides
        .iter()
        .filter(|(r, _)| *r == role)
        .flat_map(|(_, f)| f.iter().cloned())
        .collect();
    unit.set(&flags)
}

/// `unit` for `driver`: the container driver is the unit's own; the native driver replaces
/// it.
fn driven(unit: Unit, driver: Driver) -> Unit {
    match driver {
        Driver::StandInContainer => unit,
        Driver::Native => unit.replace(&["--driver=native".to_owned()]),
    }
}

/// The frontend's unit. The unit's state store is the host's Postgres; the box has none, so
/// the farm gives the frontend a SQLite file.
#[must_use]
pub fn frontend(s: &Settings, uid: u32, topo: &Topology) -> Unit {
    let state = format!("--state-store=sqlite://{}/state/state.db", s.root.display());
    // A frontend of an older release has no such flag, and its start fails on one it does not
    // know: it is given none, and its stop has no wait to skip.
    let mut older = topo.binaries.iter().map(|(role, _)| *role);
    let mut flags = vec![ONE_HOST_PLATFORM.to_owned()];
    if !older.any(|role| role == Role::Frontend) {
        flags.push(NO_LB_WAIT.to_owned());
    }
    let unit = Unit::new(Mode::Frontend, s, uid)
        .replace(&[state])
        .set(&flags)
        .set(&topo.limits.frontend);
    overrides(topo, Role::Frontend, unit)
}

/// The unit of storage node `n` (from 1), whose `/metrics` route is on loopback port
/// `metrics`: the cold store is a directory (no MinIO here), the unit is given the scrape
/// route no unit sets, and a node with the native driver (`kbf storage`, which has no
/// daemon half) is given `--frontend` and `--advertise-addr` that the storage-and-daemon
/// unit sets, so that it registers with the frontend as the other does.
///
/// # Panics
/// When the topology's driver and node mode do not go together (the farm's start refuses
/// that first).
#[must_use]
pub fn node(
    (s, uid): (&Settings, u32),
    n: usize,
    metrics: u16,
    topo: &Topology,
    container: Option<&Container>,
) -> Unit {
    let mode = match topo.node_mode {
        NodeMode::Storage => Mode::Storage,
        NodeMode::StorageAndDaemon => Mode::StorageAndDaemon,
    };
    let cold = format!("--cold-store=file://{}", s.root.join("cold").display());
    let scrape = format!("--metrics-listen=127.0.0.1:{metrics}");
    let unit = Unit::new(mode, s, uid).replace(&[cold]).append(&[scrape]);
    let unit = if topo.driver == Driver::Native {
        unit.append(&[
            format!("--frontend=https://{FRONTEND_NAME}:{}", s.wiring.worker),
            format!("--advertise-addr={}:{}", s.host, s.storage_port),
        ])
    } else {
        driven(unit, topo.driver)
    };
    let unit = overrides(topo, Role::Node(n), unit.set(&topo.limits.node));
    with_container(unit, container)
}

/// The unit of plain daemon `d` (from 1).
#[must_use]
pub fn daemon(
    (s, uid): (&Settings, u32),
    d: usize,
    topo: &Topology,
    container: Option<&Container>,
) -> Unit {
    let unit = driven(Unit::new(Mode::Daemon, s, uid), topo.driver).set(&topo.limits.daemon);
    with_container(overrides(topo, Role::Daemon(d), unit), container)
}

/// `unit` with the box's container driver: its `--oci-runtime` made the stand-in `crun`
/// (the unit names `/usr/bin/crun`, and this box's is the stand-in, which finds its state
/// beside the link it was run through), the cgroup tree and the id files.
fn with_container(unit: Unit, container: Option<&Container>) -> Unit {
    match container {
        Some(c) => unit
            .replace(std::slice::from_ref(&c.oci_runtime))
            .append(&c.flags),
        None => unit,
    }
}

#[cfg(test)]
#[path = "units_tests.rs"]
mod tests;
