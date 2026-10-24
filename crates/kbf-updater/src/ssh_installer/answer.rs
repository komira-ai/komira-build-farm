//! What `kbf-host-install` printed, read against a closed set of shapes.
//!
//! The wrapper prints one JSON line and exits 0 (done), 2 (refused) or 3 (failed); a
//! `query` answers with the report on exit 0. The exit status and the shape must agree, so
//! an answer that is plausible JSON from the wrong place (a banner, a shell that printed
//! a line of its own) is not taken for one. A Mac's `install` answers `apply_started`, and its
//! `query` carries one more field, `apply`. Nothing the host sent is passed on except a
//! reason code, held to `[a-z_]`, and an installed version, held to the version alphabet:
//! a [`PortError`] never carries free text from the other side.

use serde::Deserialize;

use super::exec::Exited;
use crate::inventory::valid_node_id;
use crate::ports::{Apply, InstallAnswer, Installed, PortError, Report};
use crate::version::Version;

/// The most stdout that is read, in bytes. The longest real line is under 200.
pub const MAX_STDOUT: usize = 4096;

const MAX_REASON: usize = 48;
const MAX_INSTALLED: usize = 256;

/// The one text for every answer that is not a known shape.
pub fn bad_answer() -> PortError {
    PortError::other("bad answer")
}

/// The exit status and stdout of a child that ended on its own with an answer that fits: a
/// signal, or more output than [`MAX_STDOUT`], is a bad answer.
pub fn checked(done: &Exited) -> Result<(i32, &[u8]), PortError> {
    match done.code {
        Some(code) if !done.stdout_overflow && done.stdout.len() <= MAX_STDOUT => {
            Ok((code, &done.stdout))
        }
        _ => Err(bad_answer()),
    }
}

/// The single line of `stdout`, or `None` when it has two lines, holds a control
/// character other than the line end, or is not UTF-8.
fn one_line(stdout: &[u8]) -> Option<&str> {
    let text = std::str::from_utf8(stdout).ok()?;
    let line = text.strip_suffix('\n').unwrap_or(text);
    let clean = !line.chars().any(char::is_control);
    clean.then_some(line)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Done {
    result: DoneResult,
    installed_version: String,
    unit_state: UnitState,
}

#[derive(Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum DoneResult {
    Installed,
    AlreadyInstalled,
}

#[derive(Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum UnitState {
    Finished,
    NotRun,
}

/// A Mac's answer to `install`: the apply is launched and goes on by itself.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Started {
    #[serde(rename = "result")]
    _result: StartedResult,
    version: String,
    commit: String,
    #[serde(rename = "apply")]
    _apply: StartedApply,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum StartedResult {
    ApplyStarted,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum StartedApply {
    Running,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Reason {
    result: ReasonResult,
    reason: String,
}

#[derive(Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ReasonResult {
    Refused,
    Failed,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct QueryLine {
    // The host's query line says what it is; any other word is not its answer.
    #[serde(rename = "result")]
    _result: QueryResult,
    installed_version: Option<String>,
    running_commit: Option<String>,
    node_id: Option<String>,
    in_progress: bool,
    // Only a Mac's line has it.
    apply: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum QueryResult {
    Query,
}

fn parse<'a, T: Deserialize<'a>>(line: &'a str) -> Result<T, PortError> {
    serde_json::from_str(line).map_err(|_| bad_answer())
}

/// A reason code: lower case and underscores, short.
fn code_ok(reason: &str) -> bool {
    !reason.is_empty()
        && reason.len() <= MAX_REASON
        && reason.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
}

/// An installed version as the package manager spells it: the version alphabet and `:`.
fn installed_ok(version: &str) -> bool {
    !version.is_empty()
        && version.len() <= MAX_INSTALLED
        && version
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".+~:-".contains(&b))
}

/// A refusal or failure line read at its exit status.
fn reason_line(line: &str, code: i32) -> Result<PortError, PortError> {
    let got: Reason = parse(line)?;
    let (want, label) = match code {
        2 => (ReasonResult::Refused, "refused"),
        _ => (ReasonResult::Failed, "failed"),
    };
    if got.result != want || !code_ok(&got.reason) {
        return Err(bad_answer());
    }
    Ok(PortError::other(format!("{label}: {}", got.reason)))
}

/// The answer of `install` to the request for `version`: what the host accepted on exit 0,
/// else the error to report. An `apply_started` for another version is a bad answer.
pub fn install_answer(
    code: i32,
    stdout: &[u8],
    version: &Version,
) -> Result<InstallAnswer, PortError> {
    let line = one_line(stdout).ok_or_else(bad_answer)?;
    match code {
        0 => match parse::<Done>(line) {
            Ok(done) => {
                let answer = match (&done.result, &done.unit_state) {
                    (DoneResult::Installed, UnitState::Finished) => InstallAnswer::Installed,
                    (DoneResult::AlreadyInstalled, UnitState::NotRun) => {
                        InstallAnswer::AlreadyInstalled
                    }
                    _ => return Err(bad_answer()),
                };
                if installed_ok(&done.installed_version) {
                    Ok(answer)
                } else {
                    Err(bad_answer())
                }
            }
            Err(_) => {
                let started: Started = parse(line)?;
                if started.version == version.as_str() && commit_ok(&started.commit) {
                    Ok(InstallAnswer::ApplyStarted)
                } else {
                    Err(bad_answer())
                }
            }
        },
        2 | 3 => Err(reason_line(line, code)?),
        _ => Err(bad_answer()),
    }
}

/// A commit as the host reports one: hex digits, at most 64.
fn commit_ok(commit: &str) -> bool {
    (1..=64).contains(&commit.len()) && commit.bytes().all(|b| b.is_ascii_hexdigit())
}

/// A Mac's apply state: `running`, `done`, or `failed:` and a reason of lower case letters,
/// `_` and `:`, at most 64 bytes (the host's own bound).
fn apply_of(text: &str) -> Option<Apply> {
    match text {
        "running" => Some(Apply::Running),
        "done" => Some(Apply::Done),
        _ => {
            let why = text.strip_prefix("failed:")?;
            let plain = !why.is_empty()
                && why.len() <= 64
                && why
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b == b'_' || b == b':');
            plain.then(|| Apply::Failed(why.to_owned()))
        }
    }
}

/// The answer of `query`.
pub fn query_answer(code: i32, stdout: &[u8]) -> Result<Report, PortError> {
    let line = one_line(stdout).ok_or_else(bad_answer)?;
    match code {
        0 => {
            let report: QueryLine = parse(line)?;
            let commit_fine = report.running_commit.as_deref().is_none_or(commit_ok);
            let id_fine = report.node_id.as_deref().is_none_or(valid_node_id);
            if !commit_fine || !id_fine {
                return Err(bad_answer());
            }
            let installed = if report.in_progress {
                Installed::InProgress
            } else {
                match report.installed_version {
                    Some(v) if installed_ok(&v) => Installed::Version(v),
                    Some(_) => return Err(bad_answer()),
                    None => Installed::NotInstalled,
                }
            };
            let apply = match report.apply.as_deref() {
                None => None,
                Some(text) => Some(apply_of(text).ok_or_else(bad_answer)?),
            };
            Ok(Report {
                installed,
                running_commit: report.running_commit,
                node_id: report.node_id,
                apply,
            })
        }
        2 | 3 => Err(reason_line(line, code)?),
        _ => Err(bad_answer()),
    }
}

/// A closed classification of what ssh itself said on stderr when it exited 255. The text
/// of stderr is never kept; only which of these it was.
pub fn unreachable_error(stderr: &[u8]) -> PortError {
    let text = String::from_utf8_lossy(stderr);
    let has = |needles: &[&str]| needles.iter().any(|n| text.contains(n));
    let class = if has(&[
        "Host key verification failed",
        "REMOTE HOST IDENTIFICATION HAS CHANGED",
        "host key",
    ]) {
        "host key"
    } else if has(&["Permission denied", "publickey"]) {
        "auth"
    } else if has(&[
        "timed out",
        "refused",
        "No route",
        "resolve",
        "unreachable",
        "Connection closed",
        "Connection reset",
    ]) {
        "connect"
    } else {
        "ssh"
    };
    PortError::unreachable(class)
}
