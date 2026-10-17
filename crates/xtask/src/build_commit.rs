//! `cargo xtask build-commit`: kbf-server names the commit it was built from
//! (`crates/kbf-server/build.rs`).
//!
//! - `check [--json] <binary> <commit>`: `<binary> --version` prints exactly `<name>
//!   <package version>+<first 12 of commit>`, where `<name>` is the binary's file name.
//!   Control: the same output must not pass for the commit with its first digit
//!   changed, so a comparison that accepts anything fails instead of passing. With
//!   `--json`, `<binary> --version --json` (kbf's build identity, crates/kbf-cli) must
//!   also print a JSON object whose `commit` is the first 12 of `<commit>` and whose
//!   `profile` is `release`: the shipped bytes say what they are.
//! - `stamps <commit>`: from the checkout, a plain build of kbf-server passes `check`
//!   for `<commit>`; builds with `KBF_BUILD_COMMIT_OVERRIDE` set to two stamps each pass
//!   `check` for their own stamp (so they name no other commit; a `<commit>` that begins
//!   with a stamp is refused); and a build with an override that is not 12 lowercase
//!   hex digits fails on the override. The last good stamp is left built in
//!   `target/debug`. Cargo is the `CARGO` the verb was started with.

use std::ffi::OsStr;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use crate::Error;

/// The variable kbf-server's build script takes in place of the checkout's commit.
pub const OVERRIDE: &str = "KBF_BUILD_COMMIT_OVERRIDE";

/// The two stamps `stamps` builds with.
pub const STAMPS: [&str; 2] = ["a1a1a1a1a1a1", "b2b2b2b2b2b2"];

/// An override the build must refuse.
pub const BAD_STAMP: &str = "not-a-commit";

/// What the build script says when it refuses an override (after `OVERRIDE=<value>`).
const REFUSAL: &str = "expected exactly 12 lowercase hex digits";

/// Whether `version` (one `--version` line) is `<name> <x.y.z[-pre]>+<first 12 of
/// commit>`.
pub fn names(version: &str, name: &str, commit: &str) -> bool {
    let short = &commit[..commit.len().min(12)];
    let Some(rest) = version.strip_prefix(name).and_then(|r| r.strip_prefix(' ')) else {
        return false;
    };
    let Some((package, stamp)) = rest.split_once('+') else {
        return false;
    };
    stamp == short && is_package_version(package)
}

/// `x.y.z` in decimal digits, with an optional `-` and a pre-release of ASCII letters,
/// digits, `.` and `-`.
fn is_package_version(text: &str) -> bool {
    let (core, pre) = match text.split_once('-') {
        Some((core, pre)) => (core, Some(pre)),
        None => (text, None),
    };
    let numbers: Vec<&str> = core.split('.').collect();
    let decimal = |n: &&str| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit());
    let pre_ok = pre.is_none_or(|p| {
        !p.is_empty()
            && p.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
    });
    numbers.len() == 3 && numbers.iter().all(decimal) && pre_ok
}

/// `commit` with its first digit changed: a commit no correct check accepts in its
/// place.
pub fn control_commit(commit: &str) -> String {
    let other = if commit.starts_with('0') { '1' } else { '0' };
    format!("{other}{}", &commit[1..])
}

/// A commit as git writes it: 12 to 40 lowercase hex digits.
pub fn is_commit(text: &str) -> bool {
    (12..=40).contains(&text.len()) && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// `check` on a `--version` line, with the comparison `names` given (the real one is
/// [`names`]; tests pass a broken one to see the control refuse it).
///
/// # Errors
/// `commit` is not a commit, `version` does not name it, or `version` also names the
/// control commit.
pub fn check_version(
    version: &str,
    name: &str,
    commit: &str,
    names: fn(&str, &str, &str) -> bool,
) -> Result<(), Error> {
    if !is_commit(commit) {
        return Err(Error::NotACommit(commit.to_owned()));
    }
    if !names(version, name, commit) {
        return Err(Error::WrongCommit {
            version: version.to_owned(),
            commit: commit.to_owned(),
        });
    }
    let other = control_commit(commit);
    if names(version, name, &other) {
        return Err(Error::ControlPassed {
            version: version.to_owned(),
            other,
        });
    }
    Ok(())
}

/// `binary --version`, or with `json` `binary --version --json`, without its line end.
fn version_of(binary: &Path, json: bool) -> Result<String, Error> {
    let mut cmd = Command::new(binary);
    cmd.arg("--version");
    if json {
        cmd.arg("--json");
    }
    let out = cmd
        .stderr(Stdio::inherit())
        .output()
        .map_err(Error::spawn(binary.display().to_string()))?;
    if !out.status.success() {
        return Err(Error::Failed(format!(
            "{} --version{}: {}",
            binary.display(),
            if json { " --json" } else { "" },
            out.status
        )));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    Ok(text.strip_suffix('\n').unwrap_or(&text).to_owned())
}

/// What the identity `json` (the output of `--version --json`) says of a release build of
/// `commit`: the line `check --json` adds to what it prints.
///
/// # Errors
/// `json` is not a JSON object, lacks a string `commit` or `profile`, names another
/// commit than the first 12 digits of `commit`, or is not a `release` build.
pub fn check_identity(json: &str, commit: &str) -> Result<String, Error> {
    let bad = |why: String| Error::Failed(format!("--version --json: {why}"));
    let value: serde_json::Value =
        serde_json::from_str(json).map_err(|_| bad(format!("not a JSON object: {json}")))?;
    let object = value
        .as_object()
        .ok_or_else(|| bad(format!("not a JSON object: {json}")))?;
    let field = |name: &str| {
        object
            .get(name)
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| bad(format!("no string field {name:?}")))
    };
    let (stamp, profile) = (field("commit")?, field("profile")?);
    let short = &commit[..commit.len().min(12)];
    if stamp != short {
        return Err(bad(format!("\"commit\" is {stamp:?}; expected {short:?}")));
    }
    if profile != "release" {
        return Err(bad(format!(
            "\"profile\" is {profile:?}; expected \"release\""
        )));
    }
    Ok(format!(
        "--version --json: commit {stamp}, profile {profile}"
    ))
}

/// `check [--json] <binary> <commit>`: the line it prints on success.
///
/// # Errors
/// As [`check_version`] (and, with `json`, [`check_identity`]), or the binary did not run.
pub fn check(binary: &Path, commit: &str, json: bool) -> Result<String, Error> {
    let name = binary
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let version = version_of(binary, false)?;
    check_version(&version, &name, commit, names)?;
    if json {
        let identity = check_identity(&version_of(binary, true)?, commit)?;
        return Ok(format!("ok: {version}; {identity}"));
    }
    Ok(format!("ok: {version}"))
}

/// `cargo build --locked -p kbf-server --bin kbf-server` in `root`, with
/// [`OVERRIDE`] set to `stamp` or removed.
fn build(cargo: &OsStr, root: &Path, stamp: Option<&str>) -> Result<Output, Error> {
    let mut cmd = Command::new(cargo);
    cmd.args([
        "build",
        "--locked",
        "-p",
        "kbf-server",
        "--bin",
        "kbf-server",
    ])
    .current_dir(root)
    .stdout(Stdio::inherit());
    match stamp {
        Some(stamp) => cmd.env(OVERRIDE, stamp),
        None => cmd.env_remove(OVERRIDE),
    };
    cmd.output()
        .map_err(Error::spawn(cargo.display().to_string()))
}

/// A build that must succeed: its stderr is passed on.
fn build_ok(cargo: &OsStr, root: &Path, stamp: Option<&str>) -> Result<(), Error> {
    let out = build(cargo, root, stamp)?;
    eprint!("{}", String::from_utf8_lossy(&out.stderr));
    if out.status.success() {
        Ok(())
    } else {
        Err(Error::Failed(format!(
            "cargo build of kbf-server failed: {}",
            out.status
        )))
    }
}

/// `stamps <commit>`, building with `cargo` in `root`: one line per step that passed.
///
/// # Errors
/// `commit` is not a commit or begins with a stamp, a build or a check failed, or the
/// malformed override did not fail the build on the override.
pub fn stamps(cargo: &OsStr, root: &Path, commit: &str) -> Result<Vec<String>, Error> {
    if !is_commit(commit) {
        return Err(Error::NotACommit(commit.to_owned()));
    }
    // A stamped build that passes `check` for its stamp names no other commit, unless
    // the commit is the stamp: then the stamped builds would prove nothing.
    if STAMPS.contains(&&commit[..12]) {
        return Err(Error::Failed(format!(
            "{commit} begins with a test stamp; the stamped builds cannot be told from it"
        )));
    }
    let binary = root.join("target/debug/kbf-server");
    let mut passed = Vec::new();
    build_ok(cargo, root, None)?;
    passed.push(check(&binary, commit, false)?);
    for stamp in STAMPS {
        build_ok(cargo, root, Some(stamp))?;
        passed.push(check(&binary, stamp, false)?);
    }
    let out = build(cargo, root, Some(BAD_STAMP))?;
    let log = String::from_utf8_lossy(&out.stderr);
    if out.status.success() {
        return Err(Error::Failed(format!(
            "a build with {OVERRIDE}={BAD_STAMP} succeeded"
        )));
    }
    let refused = format!("{OVERRIDE}=\"{BAD_STAMP}\": {REFUSAL}");
    if !log.contains(&refused) {
        return Err(Error::Failed(format!(
            "the build failed, but not on the override:\n{}",
            log.trim_end()
        )));
    }
    passed.push("ok: a malformed override fails the build".to_owned());
    Ok(passed)
}

#[cfg(test)]
mod tests {
    use super::*;

    const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

    #[test]
    fn the_version_line_names_its_commit() {
        // Control: catches a check that refuses the correct line.
        for v in [
            "kbf-server 0.1.0+0123456789ab",
            "kbf-server 10.2.33-rc.1-x+0123456789ab",
        ] {
            assert!(names(v, "kbf-server", COMMIT), "{v}");
            assert_eq!(check_version(v, "kbf-server", COMMIT, names), Ok(()), "{v}");
        }
    }

    #[test]
    fn a_version_naming_another_commit_fails() {
        // Catches: a check that ignores the stamp, compares a prefix shorter than 12, or
        // takes the control commit for the real one.
        let line = "kbf-server 0.1.0+1123456789ab";
        assert_eq!(
            check_version(line, "kbf-server", COMMIT, names),
            Err(Error::WrongCommit {
                version: line.into(),
                commit: COMMIT.into()
            })
        );
        assert!(!names("kbf-server 0.1.0+0123456789a", "kbf-server", COMMIT));
        assert!(!names(
            "kbf-server 0.1.0+0123456789abc",
            "kbf-server",
            COMMIT
        ));
    }

    #[test]
    fn a_line_off_the_form_fails() {
        // Catches: a check that matches the stamp anywhere in the line, or accepts another
        // binary's name or a package version that is not x.y.z[-pre].
        for v in [
            "kbf-daemon 0.1.0+0123456789ab",
            "kbf-server0.1.0+0123456789ab",
            "xkbf-server 0.1.0+0123456789ab",
            "kbf-server 0.1+0123456789ab",
            "kbf-server 192.0.2.1+0123456789ab",
            "kbf-server 0.x.0+0123456789ab",
            "kbf-server .1.0+0123456789ab",
            "kbf-server 0.1.0-+0123456789ab",
            "kbf-server 0.1.0-rc_1+0123456789ab",
            "kbf-server 0.1.0 0123456789ab",
            "kbf-server 0.1.0+0123456789ab\n",
        ] {
            assert!(!names(v, "kbf-server", COMMIT), "{v:?}");
        }
    }

    #[test]
    fn a_comparison_that_accepts_anything_is_caught_by_the_control() {
        // Catches: the control dropped or comparing the commit with itself, which would
        // let a broken comparison pass every binary.
        let line = "kbf-server 0.1.0+0123456789ab";
        assert_eq!(
            check_version(line, "kbf-server", COMMIT, |_, _, _| true),
            Err(Error::ControlPassed {
                version: line.into(),
                other: "1123456789abcdef0123456789abcdef01234567".into()
            })
        );
        assert_eq!(control_commit("a1a1a1a1a1a1"), "01a1a1a1a1a1");
        let err = check_version(line, "kbf-server", COMMIT, |_, _, _| true).unwrap_err();
        assert_eq!(
            err.to_string(),
            "control: \"kbf-server 0.1.0+0123456789ab\" also passed for 1123456789ab"
        );
    }

    #[test]
    fn a_commit_must_be_12_to_40_lowercase_hex_digits() {
        // Catches: an empty or short `$GITHUB_SHA` expansion passing (a 0-length prefix
        // matches every stamp), and an upper-case or 41-digit value.
        for c in [
            "",
            "0123456789a",
            "0123456789AB",
            &format!("{COMMIT}0"),
            "0123456789ag",
        ] {
            assert_eq!(
                check_version("kbf-server 0.1.0+", "kbf-server", c, names),
                Err(Error::NotACommit(c.into())),
                "{c:?}"
            );
            assert!(!is_commit(c));
        }
        assert!(is_commit(&COMMIT[..12]));
        assert!(is_commit(COMMIT));
    }
}
