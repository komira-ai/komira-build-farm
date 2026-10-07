//! A node's CPU capabilities: architecture, feature set and ISA level, parsed from
//! the text the operating system already publishes.

use std::collections::BTreeSet;
use std::fmt;
use std::str::FromStr;

use crate::level::{ArmVersion, IsaLevel, X86Level};
use crate::macos;

/// A CPU architecture, named as in node reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Arch {
    X86_64,
    Arm64,
}

impl Arch {
    /// The report name: `x86_64` or `arm64`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::X86_64 => "x86_64",
            Self::Arm64 => "arm64",
        }
    }
}

impl fmt::Display for Arch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// A string that names no architecture this crate knows.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("unknown architecture {0:?}")]
pub struct UnknownArch(pub String);

impl FromStr for Arch {
    type Err = UnknownArch;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "x86_64" => Ok(Self::X86_64),
            "arm64" => Ok(Self::Arm64),
            _ => Err(UnknownArch(s.to_owned())),
        }
    }
}

/// Why capability text could not be parsed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    /// The cpuinfo text has neither an x86-64 `flags` line nor an arm64 `Features` line.
    #[error("cpuinfo has no `flags` (x86-64) or `Features` (arm64) line")]
    NoFeatureLine,
    /// The cpuinfo text has both kinds of line.
    #[error("cpuinfo has both x86-64 `flags` and arm64 `Features` lines")]
    MixedArch,
    /// A sysctl line is not `hw.optional.<key>: <unsigned integer>`.
    #[error("sysctl line {line}: expected `hw.optional.<key>: <integer>`, got {text:?}")]
    SysctlLine { line: usize, text: String },
    /// A sysctl key appears twice.
    #[error("sysctl key {0:?} appears more than once")]
    SysctlDuplicate(String),
    /// The sysctl text lacks `hw.optional.arm64: 1`: not an Apple silicon Mac.
    #[error("sysctl text has no `hw.optional.arm64: 1`; only Apple silicon Macs are supported")]
    NotArm64Mac,
}

/// What a node's CPU can do: its architecture, every feature it reports, and the
/// highest ISA level those features reach.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CpuCaps {
    arch: Arch,
    features: BTreeSet<String>,
    level: Option<IsaLevel>,
}

impl CpuCaps {
    /// Capabilities from an architecture and a feature set in kernel names. The ISA
    /// level is computed from the features.
    #[must_use]
    pub fn new(arch: Arch, features: BTreeSet<String>) -> Self {
        let level = match arch {
            Arch::X86_64 => X86Level::highest(&features).map(IsaLevel::X86_64),
            Arch::Arm64 => ArmVersion::highest(&features).map(IsaLevel::Arm64),
        };
        Self {
            arch,
            features,
            level,
        }
    }

    /// Parses Linux `/proc/cpuinfo`: x86-64 `flags` lines or arm64 `Features` lines.
    ///
    /// The kernel's flags already omit features the OS has not enabled (an AVX flag
    /// is cleared when XCR0 lacks the wide registers), so no separate check is
    /// needed. Every flag is kept. With more than one processor block, the result is
    /// the features every processor reports, so a heterogeneous machine never claims
    /// a feature some of its cores lack.
    pub fn from_linux_cpuinfo(text: &str) -> Result<Self, ParseError> {
        let mut arch = None;
        let mut common: Option<BTreeSet<String>> = None;
        for line in text.lines() {
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            let this = match key.trim() {
                "flags" => Arch::X86_64,
                "Features" => Arch::Arm64,
                _ => continue,
            };
            if arch.is_some_and(|a| a != this) {
                return Err(ParseError::MixedArch);
            }
            arch = Some(this);
            let cpu: BTreeSet<String> = value.split_whitespace().map(str::to_owned).collect();
            common = Some(match common {
                None => cpu,
                Some(mut seen) => {
                    seen.retain(|f| cpu.contains(f));
                    seen
                }
            });
        }
        match (arch, common) {
            (Some(arch), Some(features)) => Ok(Self::new(arch, features)),
            _ => Err(ParseError::NoFeatureLine),
        }
    }

    /// Parses `sysctl hw.optional` output from an Apple silicon Mac.
    ///
    /// Every key whose value is 1 is reported: under its own name with the
    /// `hw.optional.` and `arm.` prefixes removed (`FEAT_AES`, `armv8_crc32`), and,
    /// where Linux has a hwcap for it, under the kernel name too (`aes`), so a request
    /// for `cpu.feature=aes` matches a Mac and a Linux arm64 node alike. Keys with
    /// other values (such as the `watchpoint` count) are not features.
    pub fn from_macos_sysctl(text: &str) -> Result<Self, ParseError> {
        macos::features(text).map(|f| Self::new(Arch::Arm64, f))
    }

    #[must_use]
    pub fn arch(&self) -> Arch {
        self.arch
    }

    /// Every feature, sorted.
    #[must_use]
    pub fn features(&self) -> &BTreeSet<String> {
        &self.features
    }

    /// Whether the feature is present.
    #[must_use]
    pub fn has(&self, feature: &str) -> bool {
        self.features.contains(feature)
    }

    /// The highest ISA level, or `None` if the features do not reach the lowest.
    #[must_use]
    pub fn level(&self) -> Option<IsaLevel> {
        self.level
    }

    /// Every ISA level the node supports, lowest first: the report's `isa_level` list.
    #[must_use]
    pub fn levels(&self) -> Vec<IsaLevel> {
        self.level.map(IsaLevel::with_lower).unwrap_or_default()
    }
}
