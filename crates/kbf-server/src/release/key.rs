//! What names a release file, and the only way text becomes a path segment.
//!
//! A segment of a request path is checked here against the shape it must have (40
//! lowercase hex digits, a [`ReleasePlatform`] name, a component name) and the store builds
//! its paths from the typed values ([`Commit`], [`ReleasePlatform`], [`Component`]), whose
//! text it knows. Nothing a request carries is joined into a path as it arrived.

use std::fmt;

use kbf_types::ReleasePlatform;

/// A commit: 40 lowercase hex digits.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Commit(String);

impl Commit {
    /// The commit `text` names; `None` unless it is exactly 40 lowercase hex digits.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        is_lower_hex(text, 40).then(|| Self(text.to_owned()))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Commit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A SHA-256 digest: 64 lowercase hex digits. The name a stored tarball is served under.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Sha256Hex(String);

impl Sha256Hex {
    /// The digest `text` names; `None` unless it is exactly 64 lowercase hex digits.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        is_lower_hex(text, 64).then(|| Self(text.to_owned()))
    }

    /// The digest of `bytes`' hasher, as lowercase hex.
    pub(super) fn of_digest(digest: &[u8]) -> Self {
        Self(hex::encode(digest))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Sha256Hex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

fn is_lower_hex(text: &str, len: usize) -> bool {
    text.len() == len && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// A binary a release holds, one tarball each.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Component {
    /// `kbf`.
    Kbf,
    /// `kbf-daemon`.
    KbfDaemon,
}

impl Component {
    /// Every component, in the order [`Ord`] gives.
    pub const ALL: [Self; 2] = [Self::Kbf, Self::KbfDaemon];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Kbf => "kbf",
            Self::KbfDaemon => "kbf-daemon",
        }
    }

    /// The component named exactly `name`.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.as_str() == name)
    }
}

impl fmt::Display for Component {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The file suffix of a component's attestation bundle in a request path.
pub const ATTESTATION_SUFFIX: &str = ".attestation";

/// Which file of a component a request is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Part {
    /// The tarball.
    Tarball,
    /// Its build attestation bundle.
    Attestation,
}

/// A release component: the key the store is immutable on.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Key {
    pub commit: Commit,
    pub platform: ReleasePlatform,
    pub component: Component,
}

impl fmt::Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}/{}", self.commit, self.platform, self.component)
    }
}

/// A path segment that is not what it must be.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum KeyError {
    #[error("the commit {0:?} is not 40 lowercase hex digits")]
    Commit(String),
    #[error("the platform {0:?} is not linux-x86_64, linux-arm64 or darwin-arm64")]
    Platform(String),
    #[error("the name {0:?} is not kbf, kbf-daemon or either with the suffix .attestation")]
    Name(String),
}

impl Key {
    /// The key and part the three segments of `/v1/releases/{commit}/{platform}/{name}`
    /// name.
    ///
    /// # Errors
    /// The first segment that is not exactly what it must be. Case is not folded and
    /// nothing is trimmed.
    pub fn parse(commit: &str, platform: &str, name: &str) -> Result<(Self, Part), KeyError> {
        let commit = Commit::parse(commit).ok_or_else(|| KeyError::Commit(commit.to_owned()))?;
        let platform = ReleasePlatform::parse(platform)
            .ok_or_else(|| KeyError::Platform(platform.to_owned()))?;
        let (stem, part) = match name.strip_suffix(ATTESTATION_SUFFIX) {
            Some(stem) => (stem, Part::Attestation),
            None => (name, Part::Tarball),
        };
        let component = Component::parse(stem).ok_or_else(|| KeyError::Name(name.to_owned()))?;
        Ok((
            Self {
                commit,
                platform,
                component,
            },
            part,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

    /// Catches: a segment that is not the shape accepted (the path-traversal class: `..`,
    /// separators, encoded dots decoded by the router, NUL, a trailing newline, upper case,
    /// an empty segment), and a good one refused.
    #[test]
    fn only_exact_segments_become_a_key() {
        let (key, part) = Key::parse(COMMIT, "linux-x86_64", "kbf-daemon").expect("good");
        assert_eq!(key.component, Component::KbfDaemon);
        assert_eq!(key.commit.as_str(), COMMIT);
        assert_eq!(part, Part::Tarball);
        let (_, part) = Key::parse(COMMIT, "darwin-arm64", "kbf.attestation").expect("good");
        assert_eq!(part, Part::Attestation);

        for bad in [
            "..",
            ".",
            "",
            "../../etc",
            "0123456789abcdef0123456789abcdef0123456/",
            "0123456789ABCDEF0123456789abcdef01234567",
            "0123456789abcdef0123456789abcdef0123456",
            "0123456789abcdef0123456789abcdef012345678",
            "0123456789abcdef0123456789abcdef01234567\n",
            "0123456789abcdef0123456789abcdef0123456\0",
        ] {
            assert_eq!(
                Key::parse(bad, "linux-x86_64", "kbf"),
                Err(KeyError::Commit(bad.to_owned())),
                "{bad:?}"
            );
        }
        for bad in [
            "",
            "..",
            "linux",
            "Linux-x86_64",
            "linux-x86_64/",
            "../linux-arm64",
        ] {
            assert_eq!(
                Key::parse(COMMIT, bad, "kbf"),
                Err(KeyError::Platform(bad.to_owned())),
                "{bad:?}"
            );
        }
        for bad in [
            "",
            "..",
            "kbf/../x",
            "kbf.partial",
            "kbf.attestation.attestation",
            ".attestation",
            "KBF",
            "kbf ",
            "other",
            "kbf-daemon.attestation\n",
        ] {
            assert_eq!(
                Key::parse(COMMIT, "linux-arm64", bad),
                Err(KeyError::Name(bad.to_owned())),
                "{bad:?}"
            );
        }
    }

    /// Catches: a digest of the wrong length or case accepted (it is a file name), and
    /// the rendering differing from the text.
    #[test]
    fn a_digest_is_sixty_four_lowercase_hex_digits() {
        let good = "a".repeat(64);
        assert_eq!(Sha256Hex::parse(&good).expect("good").to_string(), good);
        assert_eq!(Sha256Hex::of_digest(&[0xab; 32]).as_str(), "ab".repeat(32));
        for bad in [
            String::new(),
            "a".repeat(63),
            "A".repeat(64),
            "g".repeat(64),
            "../".repeat(22),
        ] {
            assert_eq!(Sha256Hex::parse(&bad), None, "{bad:?}");
        }
        assert_eq!(Commit::parse(COMMIT).expect("good").to_string(), COMMIT);
        assert_eq!(Component::Kbf.to_string(), "kbf");
    }
}
