//! The macOS checks on a release: no quarantine attribute on what was handed over, and a
//! code signature of the kind CI's `cargo xtask sign-darwin` makes.
//!
//! Both go through the tools macOS ships (`xattr`, `codesign`) run by a [`Runner`], so
//! they are compiled and tested on every platform with a runner that plays the tool, and
//! run for real on a Mac.
//!
//! - **Quarantine.** A file fetched by our own client has no `com.apple.quarantine`
//!   attribute. One that has it was downloaded by a browser or copied from an
//!   unverified place by hand, and is refused (so a hand-copied file fails closed).
//!   `xattr -p` exits 0 when the attribute is there and 1 with `No such xattr` when it is
//!   not; any other answer is an error, not a pass.
//! - **Signature.** `codesign --verify --strict` must accept the binary, and its code
//!   directory flags (`codesign --display --verbose=4`) must be exactly `adhoc,runtime`:
//!   signed ad hoc with the hardened runtime, which is what `sign-darwin` produces and
//!   checks (crates/xtask/src/darwin.rs). Entitlements are not looked at here.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::refusal::{Refusal, SignatureFault};
use crate::run::{Ran, Runner, Spawn};

pub const XATTR: &str = "/usr/bin/xattr";
pub const CODESIGN: &str = "/usr/bin/codesign";
/// The attribute a downloaded file carries.
pub const QUARANTINE: &str = "com.apple.quarantine";
/// The code directory flags of an ad-hoc signature with the hardened runtime.
pub const FLAGS: &str = "adhoc,runtime";

fn run(
    runner: &dyn Runner,
    what: &'static str,
    program: &str,
    args: Vec<OsString>,
    timeout: Duration,
) -> Result<Ran, Refusal> {
    let spawn = Spawn {
        program: PathBuf::from(program),
        args,
        env: Vec::new(),
        cwd: PathBuf::from("/"),
        run_as: None,
        timeout,
    };
    let ran = runner
        .run(&spawn)
        .map_err(|source| Refusal::Tool { what, source })?;
    if ran.timed_out {
        return Err(Refusal::TimedOut { what });
    }
    Ok(ran)
}

/// Whether `file` carries the quarantine attribute.
pub fn quarantined(runner: &dyn Runner, file: &Path, timeout: Duration) -> Result<bool, Refusal> {
    // `-s`: the file itself, not what a link in its place points at (the file is the
    // unprivileged process's, and this runs before the attestation is checked).
    let args = vec!["-s".into(), "-p".into(), QUARANTINE.into(), file.into()];
    let ran = run(runner, "xattr", XATTR, args, timeout)?;
    let stderr = String::from_utf8_lossy(&ran.stderr);
    match ran.code {
        Some(0) => Ok(true),
        Some(1) if stderr.contains("No such xattr") => Ok(false),
        other => Err(Refusal::Tool {
            what: "xattr",
            source: io::Error::other(format!("exit {other:?}: {}", stderr.trim_end())),
        }),
    }
}

/// `binary` (the component `component`) has a valid signature of the kind CI makes.
pub fn check_signature(
    runner: &dyn Runner,
    component: &str,
    binary: &Path,
    timeout: Duration,
) -> Result<(), Refusal> {
    let bad = |fault| Refusal::BadSignature {
        component: component.to_owned(),
        fault,
    };
    let verify = vec!["--verify".into(), "--strict".into(), binary.into()];
    if run(runner, "codesign --verify", CODESIGN, verify, timeout)?.code != Some(0) {
        return Err(bad(SignatureFault::NotValid));
    }
    // `codesign --display` writes what it finds to stderr.
    let display = vec!["--display".into(), "--verbose=4".into(), binary.into()];
    let ran = run(runner, "codesign --display", CODESIGN, display, timeout)?;
    let info = String::from_utf8_lossy(&ran.stderr);
    match code_directory_flags(&info) {
        Some(FLAGS) => Ok(()),
        found => Err(bad(SignatureFault::Flags {
            found: found.unwrap_or_default().to_owned(),
        })),
    }
}

/// The code directory's flags in `codesign --display --verbose=4` output: the names in
/// the parentheses after `flags=0x<hex>` on the `CodeDirectory` line (`adhoc,runtime`).
pub fn code_directory_flags(info: &str) -> Option<&str> {
    info.lines().find_map(|line| {
        let rest = line.strip_prefix("CodeDirectory ")?;
        let hex = &rest[rest.find("flags=0x")? + "flags=0x".len()..];
        let names = hex.trim_start_matches(|c: char| c.is_ascii_hexdigit());
        let names = names.strip_prefix('(')?;
        Some(&names[..names.find(')')?])
    })
}

#[cfg(test)]
#[path = "darwin_tests.rs"]
mod tests;
