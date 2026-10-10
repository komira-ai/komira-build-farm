//! A program the container could not start: the action's own failure, not the farm's.
//!
//! A container's files are its image, fixed by digest, and the action's input root, and
//! its environment is the `Command`'s alone. A program the OCI runtime cannot find or
//! execute there would be the same on every node, so it is the `Command`'s fault
//! (docs/design/failure-classes.md, 6.7): answered as the action's result, exit 127
//! (not found) or 126 (found, not an executable file), as a shell and `podman run`
//! answer, with kbf's message on stderr before Podman's own words. A non-zero exit is
//! never cached.
//!
//! Only crun's own report of its program lookup counts (crun 1.14, under the nodes'
//! Podman 4.9): ``executable file `<argv[0]>` not found in $PATH`` for any `ENOENT`,
//! and `open executable: ` for a file it may not execute or that is not a regular
//! file. Podman's wrapping does not count: it calls every runtime "No such file or
//! directory" "a command that was not found" (a missing mount source among them) and
//! every "Operation not permitted" "OCI permission denied" (a cgroup it may not write),
//! which are the farm's. Neither does a report from any other runtime: that start
//! stays the farm's failure, with Podman's words in it.

use std::path::Path;

use kbf_daemon::RuntimeError;

use crate::podman::{ContainerSpec, EXEC_ROOT};

/// How much of `podman start`'s stderr is read back when the container did not run.
const SAID_MAX: u64 = 64 << 10;

/// Why the container could not start the action's program.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Unstartable {
    NotFound,
    NotExecutable,
}

impl Unstartable {
    /// What the action answers: the shell's and `podman run`'s exit codes.
    pub(crate) fn exit_code(self) -> i32 {
        match self {
            Self::NotFound => 127,
            Self::NotExecutable => 126,
        }
    }
}

/// What crun's report in `said` (`podman start`'s stderr, for a container that did not
/// run) says of `program`, if it is a report of the program lookup.
pub(crate) fn classify(program: &str, said: &str) -> Option<Unstartable> {
    if said.contains(&format!("executable file `{program}` not found in $PATH")) {
        Some(Unstartable::NotFound)
    } else if said.contains("crun: open executable: ") {
        Some(Unstartable::NotExecutable)
    } else {
        None
    }
}

/// kbf's line for the action's stderr: the program, and where it was looked up.
pub(crate) fn message(
    why: Unstartable,
    program: &str,
    env: &[(String, String)],
    working_directory: &str,
) -> String {
    let what = match why {
        Unstartable::NotFound => "was not found",
        Unstartable::NotExecutable => "is not an executable file",
    };
    let path = env.iter().find(|(name, _)| name == "PATH").map(|(_, v)| v);
    let place = match (program.contains('/'), path) {
        (true, _) => {
            let dir = if working_directory.is_empty() {
                EXEC_ROOT.to_owned()
            } else {
                format!("{EXEC_ROOT}/{working_directory}")
            };
            format!("in the container (image and input root, working directory {dir})")
        }
        (false, Some(path)) => format!("on the Command's PATH {path:?}"),
        (false, None) => "on the Command's PATH: a name without a slash is looked up there, \
            and the Command sets no PATH (an action gets only the Command's environment)"
            .to_owned(),
    };
    format!("kbf: the action's program `{program}` {what} {place}")
}

/// What `podman start` wrote to `stderr` (its own words: the container did not run),
/// at most [`SAID_MAX`] bytes of it.
pub(crate) async fn said(stderr: &Path) -> std::io::Result<String> {
    use tokio::io::AsyncReadExt as _;
    let mut bytes = Vec::new();
    tokio::fs::File::open(stderr)
        .await?
        .take(SAID_MAX)
        .read_to_end(&mut bytes)
        .await?;
    Ok(String::from_utf8_lossy(&bytes).trim().to_owned())
}

/// A container `podman start` did not run to an exit (Podman's `state` for it): the
/// action's exit 127 or 126 when crun reports it cannot find or execute the program,
/// with kbf's message and Podman's words as its stderr; otherwise the farm's failure,
/// naming what Podman said.
pub(crate) async fn not_run(
    spec: &ContainerSpec,
    stderr: &Path,
    state: &str,
) -> Result<i32, RuntimeError> {
    let failed = |e: std::io::Error| RuntimeError::Failed(format!("{}: {e}", stderr.display()));
    let said = said(stderr).await.map_err(failed)?;
    let program = spec.argv.first().map_or("", String::as_str);
    let Some(why) = classify(program, &said) else {
        return Err(RuntimeError::Failed(format!(
            "the container did not run to an exit: podman reports {state:?}; podman start: {said}"
        )));
    };
    let message = message(why, program, &spec.env, &spec.working_directory);
    tracing::info!(container = %spec.name, "{message}");
    tokio::fs::write(stderr, format!("{message}\npodman start: {said}\n"))
        .await
        .map_err(failed)?;
    Ok(why.exit_code())
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOT_FOUND: &str = "Error: unable to start container \"c\": crun: executable file \
        `env` not found in $PATH: No such file or directory: OCI runtime attempted to invoke \
        a command that was not found";

    /// Catches a lookup report for another program taken for this one's, Podman's
    /// wrapping taken for crun's report (a mount source it could not find, a cgroup
    /// file it may not write: the farm's), and the two kinds swapped.
    #[test]
    fn only_crun_reporting_the_lookup_counts() {
        assert_eq!(classify("env", NOT_FOUND), Some(Unstartable::NotFound));
        assert_eq!(classify("sh", NOT_FOUND), None, "another program");
        assert_eq!(classify("en", NOT_FOUND), None, "a prefix of the name");
        let denied = "Error: crun: open executable: Permission denied: OCI permission denied";
        assert_eq!(classify("x", denied), Some(Unstartable::NotExecutable));
        for farm in [
            "Error: crun: mount `/s/root` to `/kbf/root`: No such file or directory: OCI \
             runtime attempted to invoke a command that was not found",
            "Error: crun: write `pids.max`: Operation not permitted: OCI permission denied",
            "",
        ] {
            assert_eq!(classify("env", farm), None, "{farm}");
        }
        assert_eq!(Unstartable::NotFound.exit_code(), 127);
        assert_eq!(Unstartable::NotExecutable.exit_code(), 126);
    }

    /// Catches the message not naming the program, or naming the wrong place it was
    /// looked up: the Command's `PATH` for a bare name, the working directory for a
    /// path.
    #[test]
    fn the_message_names_the_program_and_where_it_was_looked_up() {
        let path = [("PATH".to_owned(), "/a:b".to_owned())];
        assert_eq!(
            message(Unstartable::NotFound, "env", &[], ""),
            "kbf: the action's program `env` was not found on the Command's PATH: a name \
             without a slash is looked up there, and the Command sets no PATH (an action \
             gets only the Command's environment)"
        );
        assert_eq!(
            message(Unstartable::NotFound, "tool", &path, "pkg"),
            "kbf: the action's program `tool` was not found on the Command's PATH \"/a:b\""
        );
        assert_eq!(
            message(Unstartable::NotExecutable, "./data", &path, "pkg"),
            "kbf: the action's program `./data` is not an executable file in the container \
             (image and input root, working directory /kbf/root/pkg)"
        );
        assert!(message(Unstartable::NotFound, "/x", &[], "").ends_with("directory /kbf/root)"));
    }
}
