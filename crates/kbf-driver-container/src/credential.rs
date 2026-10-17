//! The node's cold-store key is kept out of every program this driver starts
//! (docs/design/architecture-v2.md, section 9: on a node that holds the key and runs
//! actions, storage and actions are separate uids and an action cannot read the key).
//!
//! A node that holds the key gets it from its unit's `EnvironmentFile=`, which systemd
//! reads as root, so the key is in this process's environment as the provider-standard
//! `AWS_*` variables (`kbf_server::config::s3_store` reads `AWS_ACCESS_KEY_ID` and
//! `AWS_SECRET_ACCESS_KEY`). An action gets only its `Command`'s variables (the
//! bundle's `process.env`), and it runs as a subordinate id of the daemon's user (the
//! bundle's id mappings), which a key file of mode `0600` does not let in. That is the
//! action's side. This module is the runtime's: `crun` is started without any variable
//! of [`CREDENTIAL_PREFIX`], so no runtime feature that copies the host's environment
//! into a container has the key to copy.
//!
//! `tests/credential_env.rs` checks it on the stand-in runtime, which hands its own
//! environment to the action; `tests/crun_credential.rs` checks both sides on real
//! `crun`.

use std::ffi::OsString;
use std::process::Command;

/// The start of the name of every variable that may hold the cold store's key.
pub(crate) const CREDENTIAL_PREFIX: &str = "AWS_";

/// `command`, with every variable of this process whose name starts with
/// [`CREDENTIAL_PREFIX`] removed from what it inherits.
pub(crate) fn without_credentials(command: &mut Command) -> &mut Command {
    remove_credentials(command, std::env::vars_os().map(|(name, _)| name))
}

/// [`without_credentials`], for a process whose variables are named `names`.
fn remove_credentials(
    command: &mut Command,
    names: impl IntoIterator<Item = OsString>,
) -> &mut Command {
    let prefix = CREDENTIAL_PREFIX.as_bytes();
    for name in names {
        if name.as_encoded_bytes().starts_with(prefix) {
            command.env_remove(name);
        }
    }
    command
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches a variable of the prefix left in (`AWS_SESSION_TOKEN` and
    /// `AWS_SHARED_CREDENTIALS_FILE` name a key as surely as the pair does), a match
    /// anywhere but the start, and a variable of another name removed (the runtime
    /// needs `XDG_RUNTIME_DIR`, `HOME` and `PATH`). The web-identity and container
    /// credential variables catch a list of known names in place of the prefix: the
    /// SDKs read more of them than any list here would keep up with.
    #[test]
    fn only_the_prefix_s_variables_are_removed() {
        let names = [
            "AWS_ACCESS_KEY_ID",
            "AWS_SECRET_ACCESS_KEY",
            "AWS_SESSION_TOKEN",
            "AWS_SHARED_CREDENTIALS_FILE",
            "AWS_WEB_IDENTITY_TOKEN_FILE",
            "AWS_CONTAINER_AUTHORIZATION_TOKEN",
            "XDG_RUNTIME_DIR",
            "MY_AWS_NOTE",
            "aws_lower",
            "AWS",
        ];
        let mut command = Command::new("crun");
        remove_credentials(&mut command, names.map(OsString::from));
        let mut removed: Vec<_> = command
            .get_envs()
            .map(|(name, value)| (name.to_string_lossy().into_owned(), value))
            .collect();
        removed.sort();
        let want = [
            "AWS_ACCESS_KEY_ID",
            "AWS_CONTAINER_AUTHORIZATION_TOKEN",
            "AWS_SECRET_ACCESS_KEY",
            "AWS_SESSION_TOKEN",
            "AWS_SHARED_CREDENTIALS_FILE",
            "AWS_WEB_IDENTITY_TOKEN_FILE",
        ]
        .map(|name| (name.to_owned(), None));
        assert_eq!(removed, want);
    }
}
