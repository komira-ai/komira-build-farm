//! The platforms kbf releases are built for (`docs/design/pull-updates.md`).

use std::fmt;

/// A platform a kbf release tarball is built for: the `<platform>` of
/// `<name>-<commit>-<platform>.tar.gz`. A host's platform comes from the `os` and
/// `arch` entries of its node report ([`ReleasePlatform::from_report`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ReleasePlatform {
    /// `linux-x86_64`.
    LinuxX86_64,
    /// `linux-arm64`.
    LinuxArm64,
    /// `darwin-arm64`.
    DarwinArm64,
}

impl ReleasePlatform {
    /// Every platform, in the order [`Ord`] gives.
    pub const ALL: [Self; 3] = [Self::LinuxX86_64, Self::LinuxArm64, Self::DarwinArm64];

    /// The platform of a node that reports `os` (`linux` or `macos`) and `arch`
    /// (`x86_64` or `arm64`); `None` for any other pair, including `macos` on `x86_64`,
    /// for which no release is built.
    #[must_use]
    pub fn from_report(os: &str, arch: &str) -> Option<Self> {
        match (os, arch) {
            ("linux", "x86_64") => Some(Self::LinuxX86_64),
            ("linux", "arm64") => Some(Self::LinuxArm64),
            ("macos", "arm64") => Some(Self::DarwinArm64),
            _ => None,
        }
    }

    /// The name in file names and on the wire: `linux-x86_64`, `linux-arm64` or
    /// `darwin-arm64`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LinuxX86_64 => "linux-x86_64",
            Self::LinuxArm64 => "linux-arm64",
            Self::DarwinArm64 => "darwin-arm64",
        }
    }

    /// The tarballs a host of this platform needs for a release: `kbf` on Linux, `kbf-daemon`
    /// on a Mac (`docs/design/pull-updates.md` section 5). The release store calls a platform
    /// complete when it holds each of these with its attestation.
    #[must_use]
    pub const fn components(self) -> &'static [&'static str] {
        match self {
            Self::LinuxX86_64 | Self::LinuxArm64 => &["kbf"],
            Self::DarwinArm64 => &["kbf-daemon"],
        }
    }

    /// The platform [`ReleasePlatform::as_str`] names `name` for; `None` for any other
    /// text (the match is exact: no case folding, no trimming).
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.as_str() == name)
    }
}

impl fmt::Display for ReleasePlatform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches: a pair mapped to another platform, `macos` on `x86_64` or an arch spelled
    /// `aarch64` accepted, an `os` of another case accepted, and any pair outside the table
    /// mapped to something. Every `os` x `arch` pair below is listed, so a new platform
    /// added to the match but not here changes a row.
    #[test]
    fn a_node_report_maps_to_its_platform() {
        let table = [
            ("linux", "x86_64", Some(ReleasePlatform::LinuxX86_64)),
            ("linux", "arm64", Some(ReleasePlatform::LinuxArm64)),
            ("linux", "aarch64", None),
            ("linux", "", None),
            ("macos", "arm64", Some(ReleasePlatform::DarwinArm64)),
            ("macos", "x86_64", None),
            ("macos", "aarch64", None),
            ("macos", "", None),
            ("darwin", "arm64", None),
            ("Linux", "x86_64", None),
            ("windows", "x86_64", None),
            ("", "x86_64", None),
            ("", "", None),
        ];
        for (os, arch, want) in table {
            assert_eq!(ReleasePlatform::from_report(os, arch), want, "{os}/{arch}");
        }
    }

    /// Catches: a name that does not read back as its platform, a platform listed twice or
    /// missing from `ALL`, text that is not exactly a name accepted, and a `Display` that
    /// differs from the name.
    #[test]
    fn names_round_trip() {
        for platform in ReleasePlatform::ALL {
            assert_eq!(ReleasePlatform::parse(platform.as_str()), Some(platform));
            assert_eq!(platform.to_string(), platform.as_str());
        }
        let names: Vec<_> = ReleasePlatform::ALL.map(ReleasePlatform::as_str).to_vec();
        assert_eq!(names, ["linux-x86_64", "linux-arm64", "darwin-arm64"]);
        for bad in [
            "",
            "linux",
            "Linux-x86_64",
            "linux-x86_64 ",
            "darwin-x86_64",
            "macos-arm64",
        ] {
            assert_eq!(ReleasePlatform::parse(bad), None, "{bad:?}");
        }
    }
}
