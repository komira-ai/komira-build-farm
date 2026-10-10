//! The program an action runs, found REAPI v2.3's way, and whose fault it is when it
//! cannot be.
//!
//! A path with a slash is relative to the working directory; a bare name is looked up
//! on the Command's `PATH` (relative entries from the working directory too), or on
//! [`DEFAULT_PATH`] when the Command sets none. Resolved here, not by `execvp`, so the
//! sandbox wrapper is handed a path and a missing program is classified the same way
//! with or without it.
//!
//! Whose fault (docs/design/failure-classes.md, 6.7): a program that could only have
//! been in the input root (a relative path, or a bare name on a `PATH` of relative
//! entries only) is the action's, answered as its result: exit 127 when it is not
//! there, 126 when it is there but not an executable file, as a shell answers, with
//! kbf's message on stderr. A program looked up anywhere outside the input root (an
//! absolute path, an absolute `PATH` entry, the default `PATH`) may be missing from
//! this node alone, environment drift, so it stays the farm's failure, naming the
//! program and the `PATH` searched.

use std::path::{Path, PathBuf};

/// Where `PATH` points when the Command sets none: what `execvp` searches then.
pub(crate) const DEFAULT_PATH: &str = "/usr/bin:/bin";

/// The action's own answer for a program it cannot run: its exit code and kbf's line
/// for its stderr.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Refused {
    pub exit_code: i32,
    pub message: String,
}

/// What [`resolve`] found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Resolved {
    /// The executable file to run.
    Program(PathBuf),
    /// The action's own failure: the program could only have been in the input root.
    Refused(Refused),
}

/// The program to run for `program`, from the working directory `work_dir` with the
/// Command's environment `env`. `Err` is the farm's failure, a message naming the
/// program and the `PATH` searched.
pub(crate) fn resolve(
    program: &str,
    work_dir: &Path,
    env: &[(String, String)],
) -> Result<Resolved, String> {
    if program.contains('/') {
        let path = work_dir.join(program);
        if executable(&path) {
            return Ok(Resolved::Program(path));
        }
        let what = what(&path);
        if program.starts_with('/') {
            return Err(format!(
                "the action's program `{program}` {what} on this node (an absolute path, \
                 outside the input root)"
            ));
        }
        return Ok(Resolved::Refused(refused(
            &path,
            format!("kbf: the action's program `{program}` {what} in the input root"),
        )));
    }
    let set = env.iter().rev().find(|(name, _)| name == "PATH");
    let path = set.map_or(DEFAULT_PATH, |(_, value)| value.as_str());
    let candidates: Vec<PathBuf> = path
        .split(':')
        .map(|dir| work_dir.join(dir).join(program))
        .collect();
    if let Some(found) = candidates.iter().find(|c| executable(c)) {
        return Ok(Resolved::Program(found.clone()));
    }
    // A candidate that is there but not executable is what a shell reports (126).
    let there = candidates.iter().find(|c| c.exists());
    let what = there.map_or("was not found", |c| what(c));
    if set.is_some() && path.split(':').all(|dir| !dir.starts_with('/')) {
        return Ok(Resolved::Refused(refused(
            there.map_or(Path::new(""), PathBuf::as_path),
            format!(
                "kbf: the action's program `{program}` {what} on the Command's PATH {path:?} \
                 (entries relative to the working directory, in the input root)"
            ),
        )));
    }
    let searched = if set.is_some() {
        format!("the Command's PATH {path:?}")
    } else {
        format!("the default PATH {path:?} (the Command sets none)")
    };
    Err(format!(
        "the action's program `{program}` {what} on this node, on {searched}"
    ))
}

/// The action's answer for `path`: 126 if it is there (not an executable file), 127 if
/// not.
fn refused(path: &Path, message: String) -> Refused {
    let exit_code = if path.as_os_str().is_empty() || !path.exists() {
        127
    } else {
        126
    };
    Refused { exit_code, message }
}

fn what(path: &Path) -> &'static str {
    if path.exists() {
        "is not an executable file"
    } else {
        "was not found"
    }
}

fn executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn program(resolved: Result<Resolved, String>) -> PathBuf {
        match resolved {
            Ok(Resolved::Program(path)) => path,
            other => panic!("not a program: {other:?}"),
        }
    }

    fn refusal(resolved: Result<Resolved, String>) -> Refused {
        match resolved {
            Ok(Resolved::Refused(refused)) => refused,
            other => panic!("not refused: {other:?}"),
        }
    }

    /// Catches a bare program name not looked up in the Command's PATH (or looked up in
    /// the daemon's), a relative PATH entry not taken from the working directory, the
    /// default PATH not used when the Command sets none, and a path that is not an
    /// executable file accepted.
    #[test]
    fn programs_resolve_as_reapi_says() {
        let wd = Path::new("/nonexistent/wd");
        let env = |path: &str| vec![("PATH".to_owned(), path.to_owned())];
        assert_eq!(
            program(resolve("sh", wd, &env("/nonexistent:/bin"))),
            Path::new("/bin/sh")
        );
        assert_eq!(program(resolve("env", wd, &[])), Path::new("/usr/bin/env"));
        assert!(resolve("sh", wd, &env("/nonexistent")).is_err());
        assert_eq!(refusal(resolve("sh", wd, &env("bin"))).exit_code, 127);
        assert_eq!(program(resolve("/bin/sh", wd, &[])), Path::new("/bin/sh"));
        assert_eq!(refusal(resolve("./tool", wd, &[])).exit_code, 127);
        assert!(resolve("/", wd, &[]).is_err(), "a directory");
        assert!(resolve("/etc/hosts", wd, &[]).is_err(), "not executable");
    }

    /// Catches whose fault a missing program is decided wrongly: inside the input root
    /// (a relative path, a PATH of relative entries) it is the action's, 127 when not
    /// there and 126 when there but not executable; anywhere outside (an absolute
    /// path, an absolute entry beside relative ones, the default PATH) it is the
    /// farm's. And the message not naming the program or the PATH searched.
    #[test]
    fn a_program_only_the_input_root_could_hold_is_the_actions() {
        let wd = Path::new("/");
        let env = |path: &str| vec![("PATH".to_owned(), path.to_owned())];
        let dir = refusal(resolve("./etc", wd, &[]));
        assert_eq!(dir.exit_code, 126, "a directory is there, not executable");
        assert!(
            dir.message.contains("`./etc` is not an executable file"),
            "{dir:?}"
        );
        let hosts = refusal(resolve("hosts", wd, &env("etc:nope")));
        assert_eq!(hosts.exit_code, 126);
        assert!(
            hosts
                .message
                .contains("`hosts` is not an executable file on the Command's PATH \"etc:nope\""),
            "{hosts:?}"
        );
        let missing = refusal(resolve("kbf-none", wd, &env("")));
        assert_eq!(missing.exit_code, 127);
        assert!(missing.message.contains("PATH \"\""), "{missing:?}");

        let mixed = resolve("kbf-none", wd, &env("bin:/usr/bin")).expect_err("farm");
        assert!(mixed.contains("`kbf-none` was not found"), "{mixed}");
        assert!(
            mixed.contains("the Command's PATH \"bin:/usr/bin\""),
            "{mixed}"
        );
        let default = resolve("kbf-none", wd, &[]).expect_err("farm");
        assert!(default.contains("default PATH"), "{default}");
        assert!(default.contains("\"/usr/bin:/bin\""), "{default}");
        let absolute = resolve("/etc/hosts", wd, &[]).expect_err("farm");
        assert!(
            absolute.contains("`/etc/hosts` is not an executable file"),
            "{absolute}"
        );
    }
}
