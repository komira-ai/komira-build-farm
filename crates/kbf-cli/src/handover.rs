//! `kbf frontend handover`: start the idle instance of the frontend's unit while the
//! other serves, so the frontend is updated without failing a build
//! (docs/design/frontend-handover.md; the unit is `kbf-frontend@.service`).
//!
//! It asks systemd, with `systemctl`, and starts nothing else:
//!
//! 1. It reads each instance's `ActiveState`. Exactly one `active` is the serving one;
//!    none is a start of the first instance (there is nothing to hand over); both
//!    `active` means an earlier handover's old instance is still draining its builds, and
//!    an instance `activating`, `reloading` or `deactivating` is changing: either is
//!    refused (exit 1), starting nothing.
//! 2. It runs `systemctl start` on the idle instance. The unit is `Type=notify`: the start
//!    returns when the new frontend has taken the listeners, the state store and the
//!    daemons over and said `READY=1`, and fails when it could not. A failed start leaves
//!    the old instance serving whenever the new one failed before it was ready (the old
//!    one then does not stand down), and the command says what it finds the old one to be.
//! 3. It prints that the new instance serves and that the old drains its open builds and
//!    exits by itself. With `--wait` it then polls the old instance until it is no longer
//!    `active`: exit 0 when it ended `inactive`, 1 when it ended `failed`.
//!
//! The binary to run must be put in place before (by rename: a running frontend keeps its
//! old inode), the updater does not restart what it replaces. Exit 0 for a new instance
//! that is ready, 1 for any other outcome (the reason goes to stderr), 2 for a command
//! line that does not parse.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, ExitCode};
use std::time::Duration;

use clap::Args;

/// The instances of `kbf-frontend@.service`, in the order a start with none up picks
/// (`kbf-deploy`'s `FRONTEND_INSTANCES`; its tests hold the two equal).
pub const INSTANCES: [&str; 2] = ["a", "b"];

/// The template's name before the `@`.
pub const TEMPLATE: &str = "kbf-frontend";

/// `kbf frontend handover`'s command line.
#[derive(Args, Clone, Debug, PartialEq, Eq)]
pub struct Handover {
    /// The `systemctl` to run.
    #[arg(long, value_name = "PATH", default_value = "systemctl")]
    pub systemctl: PathBuf,
    /// After the new instance is ready, wait until the old one has exited.
    #[arg(long)]
    pub wait: bool,
    /// How often `--wait` looks at the old instance.
    #[arg(long, value_name = "MILLISECONDS", default_value_t = 1000, requires = "wait",
          value_parser = clap::value_parser!(u64).range(1..))]
    pub poll_ms: u64,
}

/// What systemd says an instance is (`ActiveState`).
#[derive(Clone, Debug, PartialEq, Eq)]
enum State {
    Active,
    /// `inactive` and `failed`: not running, and free to start.
    Stopped,
    /// `activating`, `reloading`, `deactivating`, or a word this does not know.
    Changing(String),
}

impl State {
    fn of(word: &str) -> State {
        match word.trim() {
            "active" => State::Active,
            "inactive" | "failed" => State::Stopped,
            other => State::Changing(other.to_owned()),
        }
    }
}

/// What to do about the two instances' states.
#[derive(Debug, PartialEq, Eq)]
enum Step {
    /// Start `new` while `old` serves.
    Handover { old: String, new: String },
    /// Nothing serves: start `new`.
    Start { new: String },
}

/// Why the command starts nothing.
#[derive(Debug, PartialEq, Eq)]
enum Refusal {
    /// Both instances are active.
    Draining { a: String, b: String },
    /// An instance is neither active nor stopped.
    Changing { unit: String, state: String },
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::Draining { a, b } => write!(
                f,
                "{a} and {b} are both active: the one an earlier handover replaced is still \
                 draining its open builds and exits when they are done; run this again then"
            ),
            Refusal::Changing { unit, state } => write!(
                f,
                "{unit} is {state}: run this again when it is active or stopped"
            ),
        }
    }
}

fn unit(instance: &str) -> String {
    format!("{TEMPLATE}@{instance}.service")
}

fn decide(states: &[(String, State); 2]) -> Result<Step, Refusal> {
    if let Some((unit, State::Changing(state))) =
        states.iter().find(|(_, s)| matches!(s, State::Changing(_)))
    {
        return Err(Refusal::Changing {
            unit: unit.clone(),
            state: state.clone(),
        });
    }
    match (&states[0], &states[1]) {
        ((a, State::Active), (b, State::Active)) => Err(Refusal::Draining {
            a: a.clone(),
            b: b.clone(),
        }),
        ((old, State::Active), (new, _)) | ((new, _), (old, State::Active)) => Ok(Step::Handover {
            old: old.clone(),
            new: new.clone(),
        }),
        ((new, _), _) => Ok(Step::Start { new: new.clone() }),
    }
}

/// The handover did not happen; the reason went to stderr.
#[derive(Debug)]
struct Failed;

/// The command, to `out` and `err`; the exit code.
fn run_to(h: &Handover, out: &mut dyn Write, err: &mut dyn Write) -> u8 {
    let systemctl = Systemctl(&h.systemctl);
    match handover(h, &systemctl, out, err) {
        Ok(()) => 0,
        Err(Failed) => 1,
    }
}

fn handover(
    h: &Handover,
    systemctl: &Systemctl<'_>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<(), Failed> {
    let say = |w: &mut dyn Write, line: String| {
        let _ = writeln!(w, "kbf frontend handover: {line}");
    };
    let mut states = Vec::new();
    for instance in INSTANCES {
        let unit = unit(instance);
        match systemctl.state(&unit) {
            Ok(state) => states.push((unit, state)),
            Err(e) => {
                say(err, e);
                return Err(Failed);
            }
        }
    }
    let states: [(String, State); 2] = states.try_into().expect("two instances");
    let (old, new) = match decide(&states) {
        Ok(Step::Handover { old, new }) => (Some(old), new),
        Ok(Step::Start { new }) => (None, new),
        Err(refusal) => {
            say(err, refusal.to_string());
            return Err(Failed);
        }
    };
    match &old {
        Some(old) => say(
            out,
            format!("{old} serves; starting {new} to take over from it"),
        ),
        None => say(out, format!("no frontend serves; starting {new}")),
    }
    if let Err(why) = systemctl.start(&new) {
        say(err, format!("{new} did not become ready: {why}"));
        match old {
            Some(old) => match systemctl.state(&old) {
                Ok(State::Active) => say(
                    err,
                    format!(
                        "{old} is still active; it stands down only for a new instance that is ready (see `journalctl -u {new}`)"
                    ),
                ),
                Ok(_) => say(
                    err,
                    format!(
                        "{old} is no longer active either, and no frontend serves: see `journalctl -u {new} -u {old}`"
                    ),
                ),
                Err(e) => say(err, e),
            },
            None => say(err, format!("see `journalctl -u {new}`")),
        }
        return Err(Failed);
    }
    let Some(old) = old else {
        say(out, format!("{new} is ready and serves"));
        return Ok(());
    };
    say(
        out,
        format!(
            "{new} is ready and serves; {old} drains its open builds and exits when they are done"
        ),
    );
    if !h.wait {
        return Ok(());
    }
    loop {
        match systemctl.state(&old) {
            Ok(State::Stopped) => break,
            Ok(State::Active | State::Changing(_)) => {
                std::thread::sleep(Duration::from_millis(h.poll_ms));
            }
            Err(e) => {
                say(err, e);
                return Err(Failed);
            }
        }
    }
    match systemctl.result(&old) {
        Ok(result) if result == "success" => {
            say(out, format!("{old} has exited"));
            Ok(())
        }
        Ok(result) => {
            say(
                err,
                format!("{old} ended with result {result}: see `journalctl -u {old}`"),
            );
            Err(Failed)
        }
        Err(e) => {
            say(err, e);
            Err(Failed)
        }
    }
}

/// `systemctl`, the program at a path.
struct Systemctl<'a>(&'a PathBuf);

impl Systemctl<'_> {
    fn run(&self, args: &[&str]) -> Result<std::process::Output, String> {
        Command::new(self.0)
            .args(args)
            .output()
            .map_err(|e| format!("cannot run {}: {e}", self.0.display()))
    }

    fn property(&self, unit: &str, name: &str) -> Result<String, String> {
        let out = self.run(&["show", "-p", name, "--value", unit])?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
        } else {
            Err(format!(
                "{} show {unit}: {}",
                self.0.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            ))
        }
    }

    fn state(&self, unit: &str) -> Result<State, String> {
        self.property(unit, "ActiveState").map(|s| State::of(&s))
    }

    fn result(&self, unit: &str) -> Result<String, String> {
        self.property(unit, "Result")
    }

    /// `systemctl start unit`, which for a `Type=notify` unit returns at `READY=1`.
    fn start(&self, unit: &str) -> Result<(), String> {
        let out = self.run(&["start", unit])?;
        if out.status.success() {
            return Ok(());
        }
        let said = String::from_utf8_lossy(&out.stderr);
        let said = said.lines().map(str::trim).find(|l| !l.is_empty());
        Err(format!(
            "systemctl start failed ({}){}",
            out.status,
            said.map_or_else(String::new, |s| format!(": {s}"))
        ))
    }
}

/// Runs the command on the process's stdout and stderr.
#[must_use]
pub fn run(h: &Handover) -> ExitCode {
    ExitCode::from(run_to(h, &mut std::io::stdout(), &mut std::io::stderr()))
}

#[cfg(test)]
#[path = "handover_tests.rs"]
mod tests;
