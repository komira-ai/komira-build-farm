//! Which commit `build.rs` embeds as `KBF_BUILD_COMMIT`. The build script includes this
//! file by path, and the library compiles it only for its unit tests, so the rule the
//! build applies is the rule the tests check.
//!
//! The commit is the value of [`OVERRIDE`] when that variable is set, and otherwise
//! `git rev-parse --short=12 HEAD`, or `unknown` where git gives nothing. The override
//! is for CI tests that need two builds stamped with different commits from one
//! checkout (a deploy from A to B); a release build never sets it, and the artifacts
//! workflow checks that the binary it packages names the commit it was built from.

/// The variable that replaces the commit read from git.
pub const OVERRIDE: &str = "KBF_BUILD_COMMIT_OVERRIDE";

/// The commit to embed. `override_value` is [`OVERRIDE`] as the build environment holds
/// it (`None` when unset); `git_head` reads the checkout's commit and is called only
/// without an override.
///
/// # Errors
/// The override is set but is not exactly 12 lowercase hex digits, the form git gives.
/// An empty value is refused too: a CI script whose variable expanded to nothing must
/// fail, not quietly build from the checkout's commit.
pub fn choose(
    override_value: Option<&std::ffi::OsStr>,
    git_head: impl FnOnce() -> Option<String>,
) -> Result<String, String> {
    match override_value {
        Some(value) => {
            let text = value.to_str().unwrap_or_default();
            let hex = |b: u8| b.is_ascii_digit() || (b'a'..=b'f').contains(&b);
            if text.len() == 12 && text.bytes().all(hex) {
                Ok(text.to_owned())
            } else {
                Err(format!(
                    "{OVERRIDE}={value:?}: expected exactly 12 lowercase hex digits"
                ))
            }
        }
        None => Ok(git_head()
            .filter(|c| !c.is_empty())
            .unwrap_or_else(|| "unknown".to_owned())),
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use super::choose;

    fn from(value: &str) -> Result<String, String> {
        choose(Some(OsStr::new(value)), || {
            panic!("git read despite an override")
        })
    }

    /// Catches: an override that is ignored (git's commit embedded instead), or git
    /// read although an override is set.
    #[test]
    fn an_override_replaces_the_git_commit() {
        assert_eq!(from("0123456789ab"), Ok("0123456789ab".to_owned()));
    }

    /// Catches: an override of another shape accepted, so a stamp that is not a commit
    /// (empty, too short, too long, upper case, not hex) reaches `--version`.
    #[test]
    fn an_override_of_another_shape_is_refused() {
        for bad in [
            "",
            "0123456789a",
            "0123456789abc",
            "0123456789AB",
            "0123456789ag",
        ] {
            let err = from(bad).expect_err(bad);
            assert!(err.contains("KBF_BUILD_COMMIT_OVERRIDE"), "{err}");
        }
    }

    /// Catches: without an override, a commit other than git's, or no fallback when
    /// git gives nothing.
    #[test]
    fn without_an_override_git_decides() {
        assert_eq!(
            choose(None, || Some("fedcba987654".to_owned())),
            Ok("fedcba987654".to_owned())
        );
        assert_eq!(choose(None, || None), Ok("unknown".to_owned()));
        assert_eq!(
            choose(None, || Some(String::new())),
            Ok("unknown".to_owned())
        );
    }
}
