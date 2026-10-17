//! `linux render-units --mode` and `check-units --mode`: the systemd unit of one mode of
//! the one `kbf` binary (docs/design/architecture-v2.md, sections 1 and 11), from the
//! templates in `templates/linux/modes/`, beside `kbf.slice`.
//!
//! Each unit runs `<KBF_BIN_DIR>/kbf <mode> ...` with the flags that mode's `kbf` takes:
//! `frontend` its three listeners, the storage URL with the frontend's client
//! certificate, and the SQLite state store; `storage` its stores, its listen address and
//! its certificate flags; `daemon` the flags `kbf-daemon`'s unit has; `storage-and-daemon`
//! the storage unit's flags, its registration with the frontend (`--frontend`,
//! `--advertise-addr`) and the daemon's; `all-in-one` the frontend's and the daemon's.
//! `TODO(registration):` in the `storage` and `daemon` templates marks the frontend's DNS
//! name there (`--frontend`, `--advertise-addr`), which those two modes do not give yet.
//!
//! [`check`] reads a rendered unit back, with its drop-ins ([`UnitText`]). Its rules,
//! and the defect each catches:
//!
//! - `render`: no `@KEY@` placeholder is left; with `installable`, no `<...>` one.
//! - `continuation`: no comment line inside a continued directive. systemd 241 and
//!   later skip one (systemd.syntax(7)); 240 skips one only at the start of the line,
//!   and 239 and older join it into the value, so its words become arguments.
//! - `runtime`: a mode that runs builds passes `--container-runtime=oci` and
//!   `--oci-runtime=`, and no file of its unit names Podman ([`super::runtime`]).
//! - `exec`: `ExecStart=` is `<KBF_BIN_DIR>/kbf <mode>`, this unit's mode (a unit
//!   that starts another mode, or an old binary, under this mode's name).
//! - `slice`: the unit is in `kbf.slice`.
//! - `delegate`: a mode that runs builds delegates `cpu`, `cpuset`, `memory` and `pids`
//!   (`Delegate=yes` delegates them all) and disables none of them
//!   (`DisableControllers=`): the daemon makes its own `supervisor/` and `actions/`
//!   cgroups, and `--lease-placement=numa-node` pins each lease to one NUMA node's CPUs
//!   and memory through `cpuset`. The key is read as systemd's
//!   `config_parse_delegate` reads it: a boolean delegates every controller or none
//!   (`yes` sets the whole set, `no` clears it), a list adds its controllers to what
//!   earlier lines gave (so `Delegate=yes` then `Delegate=memory` still delegates all),
//!   an empty value starts over with none.
//! - `credential`: the units of a mode without a storage half (`daemon`, `all-in-one`)
//!   do not give it the cold store's key; [`super::credential`] says what the rule reads
//!   and what it cannot see. The unit that holds the key (`storage`) runs as a user of
//!   its own: a `User=` that is not `KBF_USER`, the user builds run as.
//!   `storage-and-daemon` runs as `KBF_USER` and holds the key in exactly one way: the
//!   `EnvironmentFile=<KBF_ETC>/s3.env`, which on the node is a plain file, root's, with
//!   no group or other bit; every other way the rule refuses in a build node's unit it
//!   refuses there too.
//! - `storage`: the `storage` and `storage-and-daemon` units give their cold store,
//!   metadata directory, listen address and the three certificate flags, and an `s3://`
//!   cold store comes with the key file ([`super::stores`]).
//! - `state-store`: a `--state-store` the unit gives is `sqlite:///<absolute path>`
//!   and names no variable (systemd would expand it into kbf's argv); postgres is not
//!   wired yet, so a Postgres URI fails ([`super::stores`]).
//! - `ports`: no two of the unit's listeners share a port ([`super::stores`]).
//! - `handover`: the frontend's unit is the template `kbf-frontend@.service` whose two
//!   instances hand over to one another: `Type=notify`, a control socket in a preserved
//!   `RuntimeDirectory=`, no `Conflicts=` ([`super::handover`]).
//! - `stop`: a mode that drains on SIGTERM (every mode but `daemon`) has
//!   `TimeoutStopSec=infinity` and `KillSignal=SIGTERM`, so systemd never kills it
//!   mid-drain and fails the builds in flight ([`super::stop`]).
//!
//! [`check_on_host`] adds the rules that read the host, and reads `kbf.slice`:
//!
//! - `parse`: a mode's flags are ones `kbf <mode>` takes, by kbf's own parser:
//!   `<kbf> check-args <mode> <flags...>` must exit 0 (it parses as a start would and
//!   starts nothing). `<kbf>` is `--kbf-bin` when given; else the unit's own
//!   `ExecStart=` binary when it is there; else the rule is skipped with a `SKIP parse:`
//!   line (rendering on a machine with no kbf installed, such as CI), which is not a
//!   failure. A `--kbf-bin` that cannot be run is a refusal. An older kbf, without
//!   `check-args`, fails the rule with clap's
//!   `error: unrecognized subcommand 'check-args'` (docs/deploy/kbf-modes.md).
//! - `cpuset`: neither the unit nor `kbf.slice` allows fewer CPUs or NUMA nodes than
//!   the node has ([`super::cpuset`]); `delegate` also refuses a `kbf.slice` that
//!   disables a delegated controller below it (a slice cannot delegate; systemd enables
//!   on the slice every controller a unit under it delegates, unless the slice's
//!   `DisableControllers=` lists it). `credential` also reads the files the unit's
//!   `EnvironmentFile=` lines name, and for `storage` that each key file is root's or
//!   the unit's user's with no group or other bit.
//!
//! `all-in-one` passes with a `WARN trusted:` line: its builds run beside the farm's
//! state and its cold store, as one user. The container driver runs each action as a
//! subordinate id of that user (`--userns=nomap`), but no test yet proves an action
//! cannot read what the user holds (docs/design/architecture-v2.md, section 9), so the
//! unit is for a trusted single box or development only.
//!
//! kbf-cli's tests also parse each rendered unit with kbf's parser in-process
//! (`crates/kbf-cli/tests/deploy_units.rs`): kbf-deploy does not link the server and
//! the daemon to read their flags.

use std::collections::BTreeSet;
use std::fmt;
use std::io;
use std::path::Path;

use kbf_host::exec::Exec;
use kbf_host::path::HostPath;
use kbf_host::step::Cmd;

use super::check::{Failure, Report, angle_placeholder};
use super::config::{LinuxConfig, LinuxError};
use super::render::{SLICE, values};
use super::{cpuset, credential, handover, runtime, stop, stores};
use crate::template::{self, TemplateError, leftovers};
use crate::unit::{UnitError, UnitFile, comments_in_continuations};

/// The cold store's key file under `KBF_ETC`: `AWS_ACCESS_KEY_ID` and
/// `AWS_SECRET_ACCESS_KEY` of the bucket's service account (`minio buckets` writes it).
pub const COLD_STORE_KEY_FILE: &str = "s3.env";

/// The controllers a mode that runs builds must have delegated (rule `delegate`).
pub const DELEGATED: [&str; 4] = ["cpu", "cpuset", "memory", "pids"];

/// The instances of the frontend's template: one serves, and an update starts the other
/// (rule `handover`, `kbf frontend handover`). Only the first is enabled at boot
/// (`DefaultInstance=` in the template).
pub const FRONTEND_INSTANCES: [&str; 2] = ["a", "b"];

/// The word after `kbf` that parses a command line and starts nothing (kbf-cli's
/// `kbf_cli::CHECK_ARGS`; its tests hold the two equal).
pub const CHECK_ARGS: &str = "check-args";

/// A mode of `kbf`, as `kbf-deploy` renders its unit. The names are kbf-cli's
/// (`kbf_cli::names`); kbf-cli's tests hold the two lists equal.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Mode {
    Frontend,
    Storage,
    Daemon,
    StorageAndDaemon,
    AllInOne,
}

impl Mode {
    /// Every mode, in `kbf --help`'s order.
    pub const ALL: [Mode; 5] = [
        Mode::Frontend,
        Mode::Storage,
        Mode::Daemon,
        Mode::StorageAndDaemon,
        Mode::AllInOne,
    ];

    /// The word after `kbf`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Mode::Frontend => "frontend",
            Mode::Storage => "storage",
            Mode::Daemon => "daemon",
            Mode::StorageAndDaemon => "storage-and-daemon",
            Mode::AllInOne => "all-in-one",
        }
    }

    /// Whether the mode runs builds, and so manages its own cgroup (rule `delegate`).
    #[must_use]
    pub fn runs_builds(self) -> bool {
        matches!(self, Mode::Daemon | Mode::StorageAndDaemon | Mode::AllInOne)
    }

    /// Whether the mode drains on SIGTERM (rule `stop`): a frontend finishes its open
    /// streams, a storage half finishes the Writes in flight. Only `daemon` does not.
    #[must_use]
    pub fn drains(self) -> bool {
        !matches!(self, Mode::Daemon)
    }

    /// Whether the mode runs builds with no cold store of its own to read (rule
    /// `credential`): `all-in-one`'s cold store is a `file://` directory.
    #[must_use]
    pub fn holds_no_key(self) -> bool {
        matches!(self, Mode::Daemon | Mode::AllInOne)
    }

    /// Whether the mode's unit holds the cold store's key beside the builds it runs
    /// (rule `credential`): `storage-and-daemon`, by `EnvironmentFile=<KBF_ETC>/s3.env`
    /// alone, a file root owns. Its storage half's uid is not split from the actions'
    /// yet (design section 9), so the unit is for a trusted network.
    #[must_use]
    pub fn holds_key_beside_builds(self) -> bool {
        self == Mode::StorageAndDaemon
    }

    /// Whether the mode's unit holds the cold store's key (rule `credential`): the
    /// storage node, as a user of its own.
    #[must_use]
    pub fn holds_key(self) -> bool {
        self == Mode::Storage
    }

    /// The unit's file name: `kbf-<mode>.service`, and for the frontend the template
    /// `kbf-frontend@.service` (see [`FRONTEND_INSTANCES`]).
    #[must_use]
    pub fn unit_name(self) -> String {
        match self {
            Mode::Frontend => "kbf-frontend@.service".to_owned(),
            _ => format!("kbf-{}.service", self.name()),
        }
    }

    /// The units the mode runs as: its own name, or for the frontend each instance of
    /// its template (`kbf-frontend@a.service`, `kbf-frontend@b.service`).
    #[must_use]
    pub fn instances(self) -> Vec<String> {
        match self {
            Mode::Frontend => FRONTEND_INSTANCES
                .iter()
                .map(|i| format!("kbf-frontend@{i}.service"))
                .collect(),
            _ => vec![self.unit_name()],
        }
    }

    fn template(self) -> &'static str {
        match self {
            Mode::Frontend => include_str!("../../templates/linux/modes/kbf-frontend@.service"),
            Mode::Storage => include_str!("../../templates/linux/modes/kbf-storage.service"),
            Mode::Daemon => include_str!("../../templates/linux/modes/kbf-daemon.service"),
            Mode::StorageAndDaemon => {
                include_str!("../../templates/linux/modes/kbf-storage-and-daemon.service")
            }
            Mode::AllInOne => include_str!("../../templates/linux/modes/kbf-all-in-one.service"),
        }
    }
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// The unit of `mode`, rendered with `config` (the values `render-units` renders the
/// converged units with); `kbf_uid` as there.
///
/// # Errors
/// Settings `mode` needs left out ([`LinuxConfig::for_mode`]), or a placeholder with no
/// value.
pub fn render(
    config: &LinuxConfig,
    kbf_uid: Option<u32>,
    mode: Mode,
) -> Result<String, RenderError> {
    config.for_mode(mode)?;
    Ok(render_template(config, kbf_uid, mode, mode.template())?)
}

/// Why a mode's unit was not rendered.
#[derive(Debug, thiserror::Error)]
pub enum RenderError {
    #[error(transparent)]
    Settings(#[from] LinuxError),
    #[error(transparent)]
    Template(#[from] TemplateError),
}

/// [`render`] with another template, for the tests that plant a defect in one.
pub(super) fn render_template(
    config: &LinuxConfig,
    kbf_uid: Option<u32>,
    mode: Mode,
    text: &str,
) -> Result<String, TemplateError> {
    template::render(&mode.unit_name(), text, &values(config, kbf_uid))
}

/// A unit as `check-units` reads it: its file, then its drop-ins (`<name>.d/*.conf` in
/// file-name order, the order systemd applies them), each a name and a text.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UnitText {
    pub files: Vec<(String, String)>,
}

impl UnitText {
    /// The file `name` with the text `text`, and no drop-in.
    #[must_use]
    pub fn new(name: impl Into<String>, text: impl Into<String>) -> UnitText {
        UnitText {
            files: vec![(name.into(), text.into())],
        }
    }

    /// This unit with the drop-in `name` (`<unit>.d/<file>.conf`) applied after the rest.
    #[must_use]
    pub fn with_drop_in(mut self, name: impl Into<String>, text: impl Into<String>) -> UnitText {
        self.files.push((name.into(), text.into()));
        self
    }

    /// The file and its drop-ins read as one unit.
    ///
    /// # Errors
    /// The first file that is not a unit file, by name.
    pub fn parse(&self) -> Result<UnitFile, ParseError> {
        let mut unit = UnitFile::default();
        for (name, text) in &self.files {
            let file = UnitFile::parse(text).map_err(|source| ParseError::Unit {
                name: name.clone(),
                source,
            })?;
            unit.extend(file);
        }
        Ok(unit)
    }
}

/// Check the rendered unit `text` of `mode` (no drop-in) against the rules of this
/// module that read the unit alone (all but those of [`check_on_host`]).
pub fn check(
    config: &LinuxConfig,
    mode: Mode,
    text: &str,
    installable: bool,
) -> Result<Report, UnitError> {
    let files = UnitText::new(mode.unit_name(), text);
    let unit = UnitFile::parse(text)?;
    Ok(report(
        mode,
        rules(config, mode, &files, &unit, installable),
    ))
}

/// [`check`] on `unit` and its drop-ins, `kbf.slice` (`slice`, with its drop-ins), and
/// then the rules that read the host `exec` runs on: `parse` (`kbf_bin`, or the unit's
/// own binary when `exec` shows it there, run as `<kbf> check-args <mode> <flags...>`
/// for a mode whose `ExecStart=` passes rule `exec`), `cpuset`, and
/// `credential`'s reading of the environment files. A rule skipped is a note.
pub fn check_on_host<E: Exec>(
    exec: &E,
    config: &LinuxConfig,
    mode: Mode,
    unit: &UnitText,
    slice: &UnitText,
    installable: bool,
    kbf_bin: Option<&HostPath>,
) -> Result<Report, ParseError> {
    let merged = unit.parse()?;
    let slice_unit = slice.parse()?;
    let name = mode.unit_name();
    let mut failures = rules(config, mode, unit, &merged, installable);
    failures.extend(text_rules(slice, installable));
    let mut notes = Vec::new();
    let mut fail = |rule: &'static str, message: String| failures.push(Failure { rule, message });
    if mode.runs_builds() {
        if let Some(off) = disabled(&slice_unit, "Slice") {
            fail(
                "delegate",
                format!(
                    "{SLICE} disables {off} below it (DisableControllers=), so {name} \
                     cannot have it delegated: the daemon needs cpu, cpuset, memory and pids"
                ),
            );
        }
        let mut found = Vec::new();
        cpuset::check(exec, &name, &merged, "Service", &mut found, &mut notes);
        cpuset::check(exec, SLICE, &slice_unit, "Slice", &mut found, &mut notes);
        for message in found {
            fail("cpuset", message);
        }
    }
    if mode.holds_no_key() {
        let found = credential::on_host(exec, &merged, &key_file(config));
        for how in found.failures {
            fail("credential", credential_message(&name, &how));
        }
        notes.extend(found.notes);
    }
    if mode.holds_key_beside_builds() {
        let found = credential::on_host(exec, &merged, &key_file(config));
        for how in found.failures {
            fail("credential", credential_message(&name, &how));
        }
        notes.extend(found.notes);
        let key = HostPath::new(&key_file(config).to_string_lossy())
            .expect("KBF_ETC is an absolute path, and so is the key file under it");
        let found = credential::root_key_on_host(exec, &merged, &key);
        for how in found.failures {
            fail("credential", root_key_message(&name, &how));
        }
        notes.extend(found.notes);
    }
    if mode.holds_key() {
        let key = HostPath::new(&key_file(config).to_string_lossy())
            .expect("KBF_ETC is an absolute path, and so is the key file under it");
        let found = credential::holder_on_host(exec, &merged, &key);
        for how in found.failures {
            fail("credential", holder_message(&name, &how));
        }
        notes.extend(found.notes);
    }
    let argv = merged.exec_start();
    if runs_this_mode(config, mode, &argv) {
        let found = match kbf_bin {
            Some(bin) => Some(bin.clone()),
            None => on_host(exec, argv[0]),
        };
        match found {
            Some(bin) => failures.extend(parse(exec, &bin, mode, &argv[2..])?),
            None => notes.push(format!(
                "SKIP parse: no kbf binary at {}, so the flags of kbf {mode} were not \
                 parsed; give --kbf-bin PATH to parse them with another",
                argv[0]
            )),
        }
    }
    let mut r = report(mode, failures);
    r.notes.extend(notes);
    Ok(r)
}

/// Why a unit could not be checked.
#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    /// A file of the unit (or of `kbf.slice`) that is not a unit file.
    #[error("{name}: {source}")]
    Unit { name: String, source: UnitError },
    /// Rule `parse` could not run kbf.
    #[error("cannot run {bin} {CHECK_ARGS}: {source}")]
    Run { bin: HostPath, source: io::Error },
}

/// The cold store's key file: `<KBF_ETC>/s3.env`.
fn key_file(config: &LinuxConfig) -> std::path::PathBuf {
    Path::new(config.etc.as_str()).join(COLD_STORE_KEY_FILE)
}

/// The unit's binary, unless `exec` shows nothing at its path or the path is not
/// absolute.
fn on_host<E: Exec>(exec: &E, program: &str) -> Option<HostPath> {
    let bin = HostPath::new(program).ok()?;
    // A path `exec` cannot look at is tried: the run says why it fails.
    (!matches!(exec.stat(&bin), Ok(None))).then_some(bin)
}

/// Rule `parse`: `bin check-args <mode> <flags>` exits 0, or the failure with the
/// first line it printed on stderr.
fn parse<E: Exec>(
    exec: &E,
    bin: &HostPath,
    mode: Mode,
    flags: &[&str],
) -> Result<Option<Failure>, ParseError> {
    let cmd = flags.iter().fold(
        Cmd::new(bin.clone()).arg(CHECK_ARGS).arg(mode.name()),
        |c, f| c.arg(*f),
    );
    let out = exec.query(&cmd).map_err(|source| ParseError::Run {
        bin: bin.clone(),
        source,
    })?;
    if out.status.success() {
        return Ok(None);
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    let said = stderr
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("(nothing on stderr)");
    let how = out
        .status
        .code()
        .map_or_else(|| "killed by a signal".to_owned(), |c| format!("exit {c}"));
    Ok(Some(Failure {
        rule: "parse",
        message: format!(
            "kbf {mode} refuses the flags of {} ({bin} {CHECK_ARGS}: {how}): {said}",
            mode.unit_name()
        ),
    }))
}

/// Whether `argv` starts `<KBF_BIN_DIR>/kbf <mode>` (rule `exec`).
fn runs_this_mode(config: &LinuxConfig, mode: Mode, argv: &[&str]) -> bool {
    argv.first() == Some(&format!("{}/kbf", config.bin_dir).as_str())
        && argv.get(1) == Some(&mode.name())
}

/// systemd's booleans (`parse_boolean`, any case).
pub(super) fn boolean(value: &str) -> Option<bool> {
    let v = value.to_ascii_lowercase();
    if ["1", "yes", "y", "true", "t", "on"].contains(&v.as_str()) {
        Some(true)
    } else if ["0", "no", "n", "false", "f", "off"].contains(&v.as_str()) {
        Some(false)
    } else {
        None
    }
}

/// The controllers the `Delegate=` lines of `[Service]` delegate, as systemd's
/// `config_parse_delegate` reads them: `yes` sets every controller, `no` clears them,
/// a list adds its controllers to the ones already set, an empty value clears them.
/// `None` for every controller.
fn delegated(unit: &UnitFile) -> Option<BTreeSet<String>> {
    let mut all = false;
    let mut listed = BTreeSet::new();
    for d in unit
        .directives()
        .iter()
        .filter(|d| d.section == "Service" && d.key == "Delegate")
    {
        let v = d.value.trim();
        match boolean(v) {
            Some(on) => {
                all = on;
                listed.clear();
            }
            None if v.is_empty() => {
                all = false;
                listed.clear();
            }
            None => listed.extend(credential::words(v)),
        }
    }
    (!all).then_some(listed)
}

/// The controllers of [`DELEGATED`] that `DisableControllers=` in `[section]` disables
/// (a list adds, an empty value resets), joined; `None` when it disables none of them.
fn disabled(unit: &UnitFile, section: &str) -> Option<String> {
    let mut off = BTreeSet::new();
    for d in unit
        .directives()
        .iter()
        .filter(|d| d.section == section && d.key == "DisableControllers")
    {
        if d.value.trim().is_empty() {
            off.clear();
        } else {
            off.extend(credential::words(&d.value));
        }
    }
    let hit: Vec<&str> = DELEGATED.into_iter().filter(|c| off.contains(*c)).collect();
    (!hit.is_empty()).then(|| hit.join(" "))
}

fn holder_message(name: &str, how: &str) -> String {
    format!(
        "{name} holds the cold store's key and {how}: the key is the storage node's user's \
         alone, never a build's"
    )
}

fn root_key_message(name: &str, how: &str) -> String {
    format!(
        "{name} holds the cold store's key beside its builds and {how}: the key file is \
         root's alone, mode 0600, which systemd reads and the unit's user cannot replace"
    )
}

fn credential_message(name: &str, how: &str) -> String {
    format!(
        "{name} gives a build node the cold store's key ({how}): a build node holds no \
         credential, and its builds run as its user"
    )
}

/// Rules `render` and `continuation`, on every file of `files`.
fn text_rules(files: &UnitText, installable: bool) -> Vec<Failure> {
    let mut failures = Vec::new();
    let mut fail = |rule: &'static str, message: String| failures.push(Failure { rule, message });
    for (name, text) in &files.files {
        if let Some(p) = leftovers(text).first() {
            fail("render", format!("{name} keeps the placeholder {p}"));
        }
        if let Some(p) = angle_placeholder(text).filter(|_| installable) {
            fail("render", format!("{name} keeps a <...> placeholder: {p}"));
        }
        for line in comments_in_continuations(text) {
            fail(
                "continuation",
                format!(
                    "{name} line {line} is a comment inside a continued line: systemd 239 \
                     and older read it into the value (240 an indented one); put it \
                     before the directive"
                ),
            );
        }
    }
    failures
}

/// The rules of this module that read the unit alone: `unit` is `files` read as one.
fn rules(
    config: &LinuxConfig,
    mode: Mode,
    files: &UnitText,
    unit: &UnitFile,
    installable: bool,
) -> Vec<Failure> {
    let name = mode.unit_name();
    let mut failures = text_rules(files, installable);
    let mut fail = |rule: &'static str, message: String| failures.push(Failure { rule, message });

    // exec
    let argv = unit.exec_start();
    if !runs_this_mode(config, mode, &argv) {
        fail(
            "exec",
            format!(
                "{name} starts '{}', want '{}/kbf {mode} ...'",
                argv.iter().take(2).copied().collect::<Vec<_>>().join(" "),
                config.bin_dir
            ),
        );
    }

    // runtime
    if mode.runs_builds() {
        let texts: Vec<(&str, &str)> = files
            .files
            .iter()
            .map(|(name, text)| (name.as_str(), text.as_str()))
            .collect();
        for how in runtime::check(&name, unit, &texts) {
            fail("runtime", how);
        }
    }

    // slice
    if unit.get_in("Service", "Slice") != Some(SLICE) {
        fail("slice", format!("{name} is not in {SLICE}"));
    }

    // delegate
    if let Some(given) = delegated(unit).filter(|_| mode.runs_builds()) {
        let missing: Vec<&str> = DELEGATED
            .into_iter()
            .filter(|c| !given.contains(*c))
            .collect();
        if !missing.is_empty() {
            let given = if given.is_empty() {
                "no controller".to_owned()
            } else {
                given.into_iter().collect::<Vec<_>>().join(" ")
            };
            fail(
                "delegate",
                format!(
                    "{name} runs builds and delegates {given}, not {}: the daemon makes its \
                     own cgroups and needs cpu, cpuset, memory and pids delegated \
                     (Delegate=yes), cpuset to pin each lease to one NUMA node \
                     (--lease-placement=numa-node)",
                    missing.join(" ")
                ),
            );
        }
    }
    if let Some(off) = disabled(unit, "Service").filter(|_| mode.runs_builds()) {
        fail(
            "delegate",
            format!(
                "{name} disables {off} for its own cgroups (DisableControllers=): the daemon \
                 needs cpu, cpuset, memory and pids in supervisor/ and actions/"
            ),
        );
    }

    // credential
    if mode.holds_no_key() || mode.holds_key_beside_builds() {
        let allowed = mode.holds_key_beside_builds();
        for how in credential::key_given(unit, &key_file(config), allowed) {
            fail("credential", credential_message(&name, &how));
        }
    }
    if let Some(how) = credential::holder(unit, &config.user).filter(|_| mode.holds_key()) {
        fail("credential", holder_message(&name, &how));
    }

    // storage
    if mode == Mode::Storage || mode.holds_key_beside_builds() {
        let exact = mode.holds_key_beside_builds();
        for how in stores::storage(unit, &key_file(config), exact) {
            fail("storage", format!("{name} {how}"));
        }
    }

    // state-store
    for how in stores::state_store(unit) {
        fail("state-store", format!("{name}: {how}"));
    }

    // ports
    for how in stores::ports(unit) {
        fail("ports", format!("{name}: {how}"));
    }

    // handover
    if mode == Mode::Frontend {
        for how in handover::check(unit) {
            fail("handover", format!("{name} {how}"));
        }
    }

    // stop
    if mode.drains() {
        for how in stop::check(unit) {
            fail("stop", format!("{name} {how}"));
        }
    }
    failures
}

/// The line every check of `all-in-one` prints: it is for a trusted box.
pub const TRUSTED_NOTE: &str = "WARN trusted: kbf all-in-one runs builds beside the farm's \
     state and its cold store, as one user; the container driver runs each action as a \
     subordinate id of that user (--userns=nomap), but no test yet proves an action cannot \
     read what the user holds: use it on a trusted single box or for development only";

/// The report of `failures` in the unit of `mode`.
fn report(mode: Mode, failures: Vec<Failure>) -> Report {
    let summary = if failures.is_empty() {
        format!("UNIT OK: {} runs kbf {mode}", mode.unit_name())
    } else {
        format!("UNIT BROKEN: {} failed check(s)", failures.len())
    };
    let notes = if mode == Mode::AllInOne {
        vec![TRUSTED_NOTE.to_owned()]
    } else {
        Vec::new()
    };
    Report {
        failures,
        notes,
        summary,
    }
}

#[cfg(test)]
#[path = "modes_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "modes_matrix_tests.rs"]
mod matrix_tests;

#[cfg(test)]
#[path = "modes_keys_tests.rs"]
mod keys_tests;
