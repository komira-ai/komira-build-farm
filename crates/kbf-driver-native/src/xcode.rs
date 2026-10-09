//! The Xcodes on this Mac, whether each can run actions, and the one an action names.
//!
//! A Mac may have several Xcodes installed side by side (`/Applications/Xcode.app`,
//! `/Applications/Xcode_16.2.app`, ...). [`survey`] finds every `Xcode*.app` in a
//! directory and asks each, under that Xcode's `DEVELOPER_DIR`, in this order (with
//! its own `xcodebuild`, the one inside the app, never the `/usr/bin` shim):
//!
//! 1. `xcodebuild -version`, which must print a build;
//! 2. `xcodebuild -license check` (an Xcode whose licence is not accepted still answers
//!    `-version` with exit 0, while this, and `cc`, `swiftc` and `xcrun` in every
//!    action, exit 69; issue #164);
//! 3. `xcodebuild -checkFirstLaunchStatus` (its first launch was run);
//! 4. `xcrun --find clang` (its compiler can be found);
//! 5. only on a node that requires it ([`Probe::metal`]): `xcodebuild -showComponent
//!    MetalToolchain` reports `Status: installed`. An Xcode that does not know
//!    `-showComponent` (before Xcode 26, which bundled Metal) passes when `xcrun
//!    --find metal` does.
//!
//! The daemon's survey asks every question as an action runs ([`Probe::sandbox`]):
//! under the actions' sandbox with the network off, in a lease directory of its own,
//! with the user-folder rules. `xcrun` reads and fills its cache there, a file any
//! lease can write (`crate::user_folders`), so what a lease wrote there reaches only
//! sandboxed lookups, never a tool the daemon runs outside the sandbox.
//!
//! Each question must exit 0 within [`ANSWER_WITHIN`]. The first that fails decides the
//! Xcode's [`State`], and a failed check that a human can fix carries the command that
//! fixes it ([`Xcode::fix`]). Every installed Xcode is reported, ready or not, in the
//! node's status ([`Xcode::status`]), so an operator sees what to do; only ready ones are
//! advertised for placement ([`ready`]): the driver reports one `xcode` entry per ready
//! build ([`CAPABILITY`]), which `kbf-caps` matches by membership, so an action that
//! names a build runs on any Mac that has it ready. [`crate::xcode_watch`] asks again
//! every few minutes, so an Xcode a human fixes becomes ready without a restart.
//!
//! An action names its Xcode with the platform property `xcode` (the name in any case,
//! as the front reads it) and runs with `DEVELOPER_DIR` set to that Xcode, so `xcrun`,
//! `cc`, `swiftc` and `xcodebuild` are that Xcode's ([`developer_dir`]). The property
//! wins over a `DEVELOPER_DIR` in the Command's environment: the build the action names
//! is the one the scheduler matched and the one in its digest.

use std::collections::BTreeMap;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use kbf_daemon::RuntimeError;
use kbf_proto::reapi::{Action, Command, Platform};
use kbf_proto::worker::{XcodeState, XcodeStatus};

use crate::network::{Isolation, Network};

/// The platform property, and the node report key, that names an Xcode build.
pub const CAPABILITY: &str = "xcode";

/// The variable that selects an Xcode for `xcrun` and the tools behind it.
pub const DEVELOPER_DIR: &str = "DEVELOPER_DIR";

/// Where an Xcode keeps its `xcodebuild`, inside its `DEVELOPER_DIR`. The survey runs
/// it there, never through the `/usr/bin/xcodebuild` shim, which finds the tool through
/// `xcrun` and its cache, a file leases can rewrite (`crate::user_folders`).
pub const XCODEBUILD: &str = "usr/bin/xcodebuild";

/// Where macOS keeps `xcrun` (a shim that finds tools in the `DEVELOPER_DIR` Xcode),
/// behind every `/usr/bin` developer tool shim.
pub const XCRUN: &str = "/usr/bin/xcrun";

/// The directory Xcodes are installed in.
pub const APPLICATIONS: &str = "/Applications";

/// The build in `xcodebuild -version` output (`Build version 16C5032a`), if any.
#[must_use]
pub fn build_of(version: &str) -> Option<&str> {
    version
        .lines()
        .find_map(|line| line.trim().strip_prefix("Build version "))
        .map(str::trim)
        .filter(|build| !build.contains(char::is_whitespace))
}

/// How long an Xcode has to answer each question. A first run after an install can
/// take some seconds; one that takes longer is hung (waiting on a licence prompt or a
/// broken install), and is not ready rather than holding the node out of the farm.
pub const ANSWER_WITHIN: Duration = Duration::from_secs(60);

/// Whether an installed Xcode can run actions, and if not, which check it failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// It answered every check; its build is advertised.
    Ready,
    /// `xcodebuild -license check` exited non-zero.
    LicenseNotAccepted,
    /// `xcodebuild -checkFirstLaunchStatus` exited non-zero.
    FirstLaunchNotRun,
    /// The node requires the Metal toolchain ([`Probe::metal`]) and this Xcode has none.
    MetalToolchainMissing,
    /// Anything else: no build, no compiler, a check not answered in time, an app whose
    /// path does not resolve.
    Failed,
}

/// One `Xcode*.app` in the searched directory, as [`survey`] found it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Xcode {
    /// The app as found (`/Applications/Xcode_16.2.app`).
    pub app: PathBuf,
    /// Its `DEVELOPER_DIR`: the app's real path (links resolved) joined with
    /// `Contents/Developer`; `None` when the app's path does not resolve.
    pub developer_dir: Option<PathBuf>,
    /// Its build, once `xcodebuild -version` printed one.
    pub build: Option<String>,
    /// Whether it can run actions.
    pub state: State,
    /// Why it cannot: the question that failed and its answer (stderr, cut to
    /// [`REASON_BYTES`]). Empty when ready.
    pub reason: String,
}

/// At most this much of a failed question's answer is kept as the reason.
pub const REASON_BYTES: usize = 500;

impl Xcode {
    /// The command an administrator runs on the node to make this Xcode ready, when
    /// one is known: this Xcode's own `xcodebuild` (by path, as `sudo` drops
    /// `DEVELOPER_DIR`) with `-license accept` or `-runFirstLaunch` (as root), or
    /// `-downloadComponent MetalToolchain` (as the daemon's user).
    #[must_use]
    pub fn fix(&self) -> Option<String> {
        let (sudo, args) = match self.state {
            State::LicenseNotAccepted => ("sudo ", "-license accept"),
            State::FirstLaunchNotRun => ("sudo ", "-runFirstLaunch"),
            State::MetalToolchainMissing => ("", "-downloadComponent MetalToolchain"),
            State::Ready | State::Failed => return None,
        };
        let xcodebuild = self.developer_dir.as_ref()?.join("usr/bin/xcodebuild");
        Some(format!("{sudo}{} {args}", shell_quoted(&xcodebuild)))
    }

    /// This Xcode as `NodeStatus.xcodes` carries it.
    #[must_use]
    pub fn status(&self) -> XcodeStatus {
        let state = match self.state {
            State::Ready => XcodeState::Ready,
            State::LicenseNotAccepted => XcodeState::LicenseNotAccepted,
            State::FirstLaunchNotRun => XcodeState::FirstLaunchNotRun,
            State::MetalToolchainMissing => XcodeState::MetalToolchainMissing,
            State::Failed => XcodeState::Failed,
        };
        XcodeStatus {
            app: self.app.display().to_string(),
            build: self.build.clone().unwrap_or_default(),
            state: state.into(),
            reason: self.reason.clone(),
            fix: self.fix().unwrap_or_default(),
        }
    }
}

/// `path` as one shell word: as it is when it holds only characters no shell treats
/// specially, otherwise in single quotes.
fn shell_quoted(path: &Path) -> String {
    let text = path.display().to_string();
    if text
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "/._-+".contains(c))
    {
        text
    } else {
        format!("'{}'", text.replace('\'', r"'\''"))
    }
}

/// How [`survey`] asks each Xcode.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Probe {
    /// The `xcodebuild` run, as a path inside each Xcode's `DEVELOPER_DIR`
    /// ([`XCODEBUILD`]). A path that leads out of it (absolute, or with `..`) is never
    /// run: the Xcode is [`State::Failed`].
    pub xcodebuild: PathBuf,
    /// The `xcrun` run ([`XCRUN`]).
    pub xcrun: PathBuf,
    /// How long each question may take ([`ANSWER_WITHIN`]).
    pub within: Duration,
    /// Whether this node's actions need the Metal toolchain (a node meant for GPU
    /// work): then an Xcode without it is not ready.
    pub metal: bool,
    /// Where every question runs: under this sandbox (the daemon's survey,
    /// `crate::NativeRuntime::sandbox`), its directory made before the survey and
    /// removed after it; `None`: as the caller runs (a test, [`discover`]).
    pub sandbox: Option<Sandbox>,
}

impl Probe {
    /// Each Xcode's own `xcodebuild` and the system's `xcrun`, each question given
    /// [`ANSWER_WITHIN`], with no sandbox (the daemon sets [`Probe::sandbox`]).
    #[must_use]
    pub fn system(metal: bool) -> Self {
        Self {
            xcodebuild: PathBuf::from(XCODEBUILD),
            xcrun: PathBuf::from(XCRUN),
            within: ANSWER_WITHIN,
            metal,
            sandbox: None,
        }
    }
}

/// Every `Xcode*.app` in `apps`, in name order, each asked the questions in the module
/// documentation as `probe` says. A directory that cannot be read has none. A question
/// not answered in time (exit and close its output) is killed if still running. Each
/// Xcode is asked on a thread of its own, so the survey takes as long as its slowest
/// Xcode, up to `probe.within` per question, not as long as all of them: an `xcrun`
/// lookup it has not cached takes seconds, and the node says nothing to the server
/// until its first survey is done. With [`Probe::sandbox`], its directory is made
/// first (every Xcode is [`State::Failed`], and nothing is run, when it cannot be)
/// and removed when every Xcode has answered.
#[must_use]
pub fn survey(apps: &Path, probe: &Probe) -> Vec<Xcode> {
    let Ok(entries) = std::fs::read_dir(apps) else {
        return Vec::new();
    };
    let mut names: Vec<_> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.file_name())
        .filter(|name| {
            name.to_str()
                .is_some_and(|n| n.starts_with("Xcode") && n.ends_with(".app"))
        })
        .collect();
    names.sort();
    let sandbox = match probe.sandbox.as_ref().map(Sandbox::made).transpose() {
        Ok(sandbox) => sandbox,
        Err(why) => {
            let unasked = |name| Xcode {
                app: apps.join(name),
                developer_dir: None,
                build: None,
                state: State::Failed,
                reason: why.clone(),
            };
            return names.into_iter().map(unasked).collect();
        }
    };
    let sandbox = sandbox.as_ref();
    let found = std::thread::scope(|scope| {
        let asking: Vec<_> = names
            .into_iter()
            .map(|name| {
                let app = apps.join(name);
                scope.spawn(move || check(app, probe, sandbox))
            })
            .collect();
        asking
            .into_iter()
            .map(|asked| asked.join().expect("asking an Xcode does not panic"))
            .collect()
    });
    if let Some(sandbox) = sandbox {
        // One left behind (a removal that failed) is a `lease-` name: the next start
        // sweeps it.
        let _ = kbf_outputs::remove_tree(&sandbox.dir);
    }
    found
}

/// The ready Xcodes of `xcodes`, by build, as their `DEVELOPER_DIR`s. Of two with one
/// build (a link `Xcode.app` to `Xcode_16.2.app`), the first is kept.
#[must_use]
pub fn ready(xcodes: &[Xcode]) -> BTreeMap<String, PathBuf> {
    let mut found = BTreeMap::new();
    let builds = xcodes
        .iter()
        .filter(|x| x.state == State::Ready)
        .filter_map(|x| Some((x.build.as_ref()?, x.developer_dir.as_ref()?)));
    for (build, dir) in builds {
        found.entry(build.clone()).or_insert_with(|| dir.clone());
    }
    found
}

/// The ready Xcodes in `apps`, by build, as their `DEVELOPER_DIR`s: [`ready`] of
/// [`survey`], asking with `xcodebuild` and `xcrun`, `within` for each question, and
/// not asking for the Metal toolchain.
#[must_use]
pub fn discover(
    apps: &Path,
    xcodebuild: &Path,
    xcrun: &Path,
    within: Duration,
) -> BTreeMap<String, PathBuf> {
    let probe = Probe {
        xcodebuild: xcodebuild.to_owned(),
        xcrun: xcrun.to_owned(),
        within,
        metal: false,
        sandbox: None,
    };
    ready(&survey(apps, &probe))
}

/// The Xcode at `app`, asked as `probe` says, under `sandbox` (made) if any.
fn check(app: PathBuf, probe: &Probe, sandbox: Option<&Sandbox>) -> Xcode {
    let mut xcode = Xcode {
        app,
        developer_dir: None,
        build: None,
        state: State::Ready,
        reason: String::new(),
    };
    if let Err((state, reason)) = ask(&mut xcode, probe, sandbox) {
        xcode.state = state;
        xcode.reason = reason;
    }
    xcode
}

/// Asks `xcode` the questions in order, filling in its `DEVELOPER_DIR` and build as
/// they are learnt; the first that fails decides the state it is not ready in, and why.
fn ask(xcode: &mut Xcode, probe: &Probe, sandbox: Option<&Sandbox>) -> Result<(), (State, String)> {
    let failed = |why: String| (State::Failed, why);
    let dir = std::fs::canonicalize(&xcode.app)
        .map_err(|e| failed(e.to_string()))?
        .join("Contents")
        .join("Developer");
    xcode.developer_dir = Some(dir.clone());
    if !probe
        .xcodebuild
        .components()
        .all(|part| matches!(part, std::path::Component::Normal(_)))
    {
        return Err(failed(format!(
            "{} is not inside the Xcode",
            probe.xcodebuild.display()
        )));
    }
    let xcodebuild = dir.join(&probe.xcodebuild);
    let question =
        |program: &Path, args: &[&str]| answer(program, args, &dir, probe.within, sandbox);
    let version = question(&xcodebuild, &["-version"]).map_err(Unanswered::into_failed)?;
    let build = build_of(&version).ok_or_else(|| {
        failed(format!(
            "no build in xcodebuild -version: {:?}",
            version.trim()
        ))
    })?;
    xcode.build = Some(build.to_owned());
    question(&xcodebuild, &["-license", "check"])
        .map_err(|e| e.into_state(State::LicenseNotAccepted))?;
    question(&xcodebuild, &["-checkFirstLaunchStatus"])
        .map_err(|e| e.into_state(State::FirstLaunchNotRun))?;
    question(&probe.xcrun, &["--find", "clang"]).map_err(Unanswered::into_failed)?;
    if probe.metal {
        let asked = "xcodebuild -showComponent MetalToolchain";
        match question(&xcodebuild, &["-showComponent", "MetalToolchain"]) {
            Ok(shown) => match component_status(&shown) {
                Some("installed") => {}
                status => {
                    return Err((
                        State::MetalToolchainMissing,
                        format!("{asked} says Status: {}", status.unwrap_or("(none)")),
                    ));
                }
            },
            // An Xcode before 26 has no -showComponent; its Metal is bundled.
            Err(Unanswered::Refused(_)) => {
                question(&probe.xcrun, &["--find", "metal"])
                    .map_err(|e| e.into_state(State::MetalToolchainMissing))?;
            }
            Err(e) => return Err(e.into_failed()),
        }
    }
    Ok(())
}

/// The `Status:` line's value in `xcodebuild -showComponent` output, if it has one.
fn component_status(shown: &str) -> Option<&str> {
    shown
        .lines()
        .find_map(|line| line.trim().strip_prefix("Status:"))
        .map(str::trim)
}

/// Why a question has no answer.
#[derive(Debug, PartialEq, Eq)]
enum Unanswered {
    /// It ran and exited non-zero: the answer is no.
    Refused(String),
    /// It could not be run, or did not answer in time.
    NoAnswer(String),
}

impl Unanswered {
    /// The state a refusal means, with why; no answer at all is [`State::Failed`].
    fn into_state(self, refused: State) -> (State, String) {
        match self {
            Self::Refused(why) => (refused, why),
            Self::NoAnswer(why) => (State::Failed, why),
        }
    }

    fn into_failed(self) -> (State, String) {
        self.into_state(State::Failed)
    }
}

/// What `program args` prints to stdout, run with `DEVELOPER_DIR` set to
/// `developer_dir` (and under `sandbox`, if any: [`Sandbox::command`]), if it exits 0
/// within `within`; otherwise what was asked and why it did not answer (with its
/// stderr, cut to [`REASON_BYTES`], when it exited non-zero).
fn answer(
    program: &Path,
    args: &[&str],
    developer_dir: &Path,
    within: Duration,
    sandbox: Option<&Sandbox>,
) -> Result<String, Unanswered> {
    let asked = format!("{} {}", program.display(), args.join(" "));
    let mut command = match sandbox {
        Some(sandbox) => sandbox.command(program, args),
        None => {
            let mut command = std::process::Command::new(program);
            command.args(args);
            command
        }
    };
    command.env(DEVELOPER_DIR, developer_dir);
    let out = output_within(command, within)
        .map_err(|e| Unanswered::NoAnswer(format!("{asked}: {e}")))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(Unanswered::Refused(format!(
            "{asked} exited with {}: {}",
            out.status,
            cut(stderr.trim(), REASON_BYTES)
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// `text`, or its first `bytes` bytes (backed off to a character boundary) and `...`.
fn cut(text: &str, bytes: usize) -> String {
    if text.len() <= bytes {
        return text.to_owned();
    }
    format!("{}...", &text[..text.floor_char_boundary(bytes)])
}

/// The tools [`warm`] has `xcrun` look up: the compilers and linker builds call most.
pub const WARM_TOOLS: [&str; 6] = ["cc", "clang", "clang++", "swift", "swiftc", "ld"];

/// Where the survey's questions ([`Probe::sandbox`]) and the warm-up's lookups
/// ([`warm`]) run: as an action does, under the actions' sandbox with the network off
/// (`isolation`), with `dir` as their lease directory and `TMPDIR`, and the user-folder
/// `rules` (`crate::user_folders::UserFolders::rules`), which let `xcrun` write its
/// cache and nothing else outside `dir`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sandbox {
    pub isolation: Isolation,
    pub dir: PathBuf,
    pub rules: String,
}

impl Sandbox {
    /// This sandbox with its directory made and named by its real path (the sandbox
    /// compares real paths), or why it cannot be.
    fn made(&self) -> Result<Self, String> {
        std::fs::create_dir_all(&self.dir)
            .and_then(|()| std::fs::canonicalize(&self.dir))
            .map(|dir| Self {
                dir,
                ..self.clone()
            })
            .map_err(|e| format!("{}: {e}", self.dir.display()))
    }

    /// `program args` under this sandbox with the network off, `TMPDIR` its directory.
    fn command(&self, program: &Path, args: &[&str]) -> std::process::Command {
        let args: Vec<String> = args.iter().map(|&a| a.to_owned()).collect();
        let (program, args) =
            self.isolation
                .wrap(Network::Off, &self.dir, &self.rules, program.to_owned(), &args);
        let mut command = std::process::Command::new(program);
        command.args(args).env("TMPDIR", &self.dir);
        command
    }
}

/// On a thread of its own, has `xcrun` look up each of [`WARM_TOOLS`] for each of
/// `developer_dirs` (`None`: the node's own Xcode, no `DEVELOPER_DIR`), each lookup given
/// `within` to answer, so `xcrun`'s cache (`crate::user_folders`) holds them before
/// the first action asks: a lookup it has not cached takes seconds. A lookup that
/// fails is logged and the rest go on. `None`, and nothing run, where `xcrun` is not a
/// file (off macOS) or `sandbox.dir` cannot be made.
///
/// `xcrun` is the `/usr/bin` one (the cache it fills is that one's, and no Xcode
/// carries its own), and it reads a cache that leases can write. So every lookup runs
/// as an action does, under `sandbox` ([`lookup`]): the warm-up goes on while the
/// node serves. `sandbox.dir` is made first and removed when the warm-up ends.
pub(crate) fn warm(
    xcrun: &Path,
    developer_dirs: Vec<Option<PathBuf>>,
    within: Duration,
    sandbox: &Sandbox,
) -> Option<std::thread::JoinHandle<()>> {
    if !xcrun.is_file() {
        return None;
    }
    let sandbox = match sandbox.made() {
        Ok(sandbox) => sandbox,
        Err(e) => {
            tracing::warn!("no xcrun warm-up: {e}");
            return None;
        }
    };
    let xcrun = xcrun.to_owned();
    Some(std::thread::spawn(move || {
        for dir in developer_dirs {
            for tool in WARM_TOOLS {
                let command = lookup(&xcrun, dir.as_deref(), tool, &sandbox);
                let failed = match output_within(command, within) {
                    Ok(out) if out.status.success() => continue,
                    Ok(out) => format!(
                        "{}: {}",
                        out.status,
                        String::from_utf8_lossy(&out.stderr).trim()
                    ),
                    Err(e) => e.to_string(),
                };
                tracing::info!(tool, developer_dir = ?dir, "xcrun --find: {failed}");
            }
        }
        // One left behind (a removal that failed) is a `lease-` name: the next start
        // sweeps it.
        let _ = kbf_outputs::remove_tree(&sandbox.dir);
    }))
}

/// `xcrun --find <tool>` under `sandbox` with the network off, for the Xcode at
/// `developer_dir`, or for the node's own Xcode (the one `xcode-select` names) with
/// `DEVELOPER_DIR` removed from what the daemon was started with, which would
/// otherwise pick the Xcode instead.
fn lookup(
    xcrun: &Path,
    developer_dir: Option<&Path>,
    tool: &str,
    sandbox: &Sandbox,
) -> std::process::Command {
    let mut command = sandbox.command(xcrun, &["--find", tool]);
    match developer_dir {
        Some(dir) => command.env(DEVELOPER_DIR, dir),
        None => command.env_remove(DEVELOPER_DIR),
    };
    command
}

/// How often [`output_within`] looks whether its process has exited.
const POLL: Duration = Duration::from_millis(10);

/// What `command` prints and how it exits (stdin `/dev/null`), if it exits and closes
/// its output within `within`; otherwise this is a `TimedOut` error, and the process is
/// killed if it has not exited. A program that is busy (ETXTBSY: a file just written
/// is held open for writing by any child another thread forks before it execs) is
/// started again until `within` is up. Its stdout and stderr are read on two threads
/// while it runs, so a full pipe cannot stall it. A process it started and left holding
/// the pipes keeps those threads reading; they are waited for only until `within` is up
/// and are left behind after that (each ends when the pipe it reads closes).
fn output_within(mut command: std::process::Command, within: Duration) -> io::Result<Output> {
    let deadline = Instant::now() + within;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = loop {
        match command.spawn() {
            Err(e) if e.raw_os_error() == Some(libc::ETXTBSY) && Instant::now() < deadline => {
                std::thread::sleep(POLL);
            }
            spawned => break spawned?,
        }
    };
    let stdout = read_all(child.stdout.take().expect("stdout is piped"));
    let stderr = read_all(child.stderr.take().expect("stderr is piped"));
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            child.wait()?;
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("no answer within {within:?}; killed"),
            ));
        }
        std::thread::sleep(POLL);
    };
    let unclosed = || {
        io::Error::new(
            io::ErrorKind::TimedOut,
            format!("exited, but its output was still open after {within:?}"),
        )
    };
    let left = || deadline.saturating_duration_since(Instant::now());
    let [stdout, stderr] =
        [stdout, stderr].map(|pipe| pipe.recv_timeout(left()).map_err(|_| unclosed()));
    Ok(Output {
        status,
        stdout: stdout?,
        stderr: stderr?,
    })
}

/// A thread that reads `pipe` to its end and sends what it read.
fn read_all(mut pipe: impl Read + Send + 'static) -> Receiver<Vec<u8>> {
    let (send, receive) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = pipe.read_to_end(&mut bytes);
        // The receiver is gone once its wait timed out; then nobody wants the bytes.
        let _ = send.send(bytes);
    });
    receive
}

/// The `DEVELOPER_DIR` of the Xcode the action's platform names (the Action's
/// platform, or the Command's for clients older than REAPI 2.2), from `xcodes`; `None`
/// when it names none.
///
/// # Errors
/// [`RuntimeError::Failed`] when the named build is not one this node has: the
/// scheduler placed the action here because the node reported it, so the node, not the
/// action, is at fault, and the action is run elsewhere.
#[allow(deprecated)]
pub fn developer_dir(
    xcodes: &BTreeMap<String, PathBuf>,
    action: &Action,
    command: &Command,
) -> Result<Option<PathBuf>, RuntimeError> {
    let platform: Option<&Platform> = action
        .platform
        .as_ref()
        .filter(|p| !p.properties.is_empty())
        .or(command.platform.as_ref());
    let Some(build) = platform
        .into_iter()
        .flat_map(|p| &p.properties)
        .find(|p| p.name.eq_ignore_ascii_case(CAPABILITY))
        .map(|p| p.value.as_str())
    else {
        return Ok(None);
    };
    xcodes.get(build).cloned().map(Some).ok_or_else(|| {
        let have: Vec<&str> = xcodes.keys().map(String::as_str).collect();
        RuntimeError::Failed(format!(
            "the action names Xcode build {build:?}; this node has {have:?}"
        ))
    })
}

#[cfg(test)]
#[path = "xcode_tests.rs"]
pub(crate) mod tests;
