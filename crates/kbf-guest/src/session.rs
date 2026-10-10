//! Facts about the process the agent runs as: which launchd session it is in, and a
//! refusal to run as root.

/// The launchd session type this process runs in, as `launchctl managername` prints
/// it: `Aqua` for a logged-in GUI session, `Background` or `System` otherwise. The
/// driver refuses a guest whose agent is not in `Aqua`, since UI tests need one. A
/// failure to ask is reported as `unknown: <why>`.
#[cfg(target_os = "macos")]
#[must_use]
pub fn manager_name() -> String {
    match std::process::Command::new("/bin/launchctl")
        .arg("managername")
        .stdin(std::process::Stdio::null())
        .output()
    {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).trim().to_owned(),
        Ok(out) => format!("unknown: launchctl managername exited with {}", out.status),
        Err(e) => format!("unknown: {e}"),
    }
}

/// Off macOS there is no launchd session to ask about: empty.
#[cfg(not(target_os = "macos"))]
#[must_use]
pub fn manager_name() -> String {
    String::new()
}

/// Refuses a real or effective uid of 0. The agent runs the command it is sent as
/// itself, so as root it would hand the guest's root to whoever holds the token.
///
/// # Errors
/// The process is root.
pub fn refuse_root(uid: u32, euid: u32) -> Result<(), String> {
    if uid == 0 || euid == 0 {
        Err(format!(
            "kbf-guest refuses to run as root (uid {uid}, euid {euid}); run it as the guest's non-admin user"
        ))
    } else {
        Ok(())
    }
}

/// [`refuse_root`] for this process.
///
/// # Errors
/// This process is root.
pub fn refuse_root_here() -> Result<(), String> {
    // SAFETY: getuid and geteuid take no arguments and cannot fail.
    let (uid, euid) = unsafe { (libc::getuid(), libc::geteuid()) };
    refuse_root(uid, euid)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches a check of only one of the two ids: a set-uid-root binary has euid 0
    /// and a non-zero uid, and a root process that dropped only its effective id the
    /// other way round.
    #[test]
    fn root_in_either_id_is_refused() {
        assert!(refuse_root(0, 0).is_err());
        assert!(refuse_root(501, 0).is_err());
        assert!(refuse_root(0, 501).is_err());
        assert_eq!(refuse_root(501, 501), Ok(()));
    }

    /// Catches the session reported where none exists. On macOS the value depends on
    /// how the test was started, so `tools/ci/guest-tests.sh` checks it from a
    /// LaunchAgent and from a LaunchDaemon instead.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn off_macos_the_session_is_empty() {
        assert_eq!(manager_name(), "");
    }

    /// Catches a broken call to launchctl: whatever session the test runs in, the
    /// answer is a name, not a failure to ask.
    #[cfg(target_os = "macos")]
    #[test]
    fn on_macos_launchctl_answers() {
        let name = manager_name();
        assert!(!name.is_empty() && !name.starts_with("unknown"), "{name}");
    }
}
