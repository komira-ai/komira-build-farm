//! How much output one action may leave.

/// How much output one action may leave. An action past a limit fails with
/// [`crate::OutputsError::Limit`], and none of its outputs is recorded. The flags are
/// the container driver's, so a node's command line reads the same for either driver.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::Args)]
pub struct OutputLimits {
    /// The deepest directory an output directory may hold, in levels below it (its own
    /// entries are level 1).
    #[arg(long = "output-max-depth", default_value_t = OutputLimits::DEFAULT.max_depth)]
    pub max_depth: usize,
    /// The most entries (files, directories, symlinks and anything else) one action's
    /// outputs may hold together, the declared outputs included.
    #[arg(long = "output-max-entries", default_value_t = OutputLimits::DEFAULT.max_entries)]
    pub max_entries: u64,
    /// The most bytes one action's output files may hold together.
    #[arg(long = "output-max-bytes", default_value_t = OutputLimits::DEFAULT.max_bytes)]
    pub max_bytes: u64,
    /// The most bytes each of the action's stdout and stderr may hold.
    #[arg(
        long = "output-max-stdio-bytes",
        default_value_t = OutputLimits::DEFAULT.max_stdio_bytes
    )]
    pub max_stdio_bytes: u64,
}

impl OutputLimits {
    /// 512 levels, a million entries, 16 GiB of files, 1 GiB each of stdout and stderr.
    pub const DEFAULT: Self = Self {
        max_depth: 512,
        max_entries: 1_000_000,
        max_bytes: 16 << 30,
        max_stdio_bytes: 1 << 30,
    };
}

impl Default for OutputLimits {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Which of the [`OutputLimits`] an action's outputs passed, shown with the flag that
/// sets it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exceeded {
    Depth,
    Entries,
    Bytes,
    /// The bytes of stdout or of stderr.
    Stdio,
}

impl std::fmt::Display for Exceeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Depth => "directory depth, --output-max-depth",
            Self::Entries => "entries, --output-max-entries",
            Self::Bytes => "file bytes, --output-max-bytes",
            Self::Stdio => "stdout or stderr bytes, --output-max-stdio-bytes",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches a limit flag that is missing, misnamed or defaulted to something other
    /// than [`OutputLimits::DEFAULT`] in a command line that flattens the limits.
    #[test]
    fn the_limits_are_flags() {
        #[derive(clap::Parser)]
        struct Line {
            #[command(flatten)]
            limits: OutputLimits,
        }
        let parse = |args: &[&str]| {
            <Line as clap::Parser>::try_parse_from(
                std::iter::once("kbf").chain(args.iter().copied()),
            )
            .map(|line| line.limits)
        };
        assert_eq!(parse(&[]).expect("defaults"), OutputLimits::default());
        let set = [
            "--output-max-depth=7",
            "--output-max-entries=8",
            "--output-max-bytes=9",
            "--output-max-stdio-bytes=10",
        ];
        let limits = OutputLimits {
            max_depth: 7,
            max_entries: 8,
            max_bytes: 9,
            max_stdio_bytes: 10,
        };
        assert_eq!(parse(&set).expect("set"), limits);
        assert!(parse(&["--output-max-depth=-1"]).is_err());
    }

    /// Catches a limit error that does not name the flag that would raise it.
    #[test]
    fn each_limit_names_its_flag() {
        assert!(Exceeded::Depth.to_string().ends_with("--output-max-depth"));
        assert!(
            Exceeded::Entries
                .to_string()
                .ends_with("--output-max-entries")
        );
        assert!(Exceeded::Bytes.to_string().ends_with("--output-max-bytes"));
        assert!(
            Exceeded::Stdio
                .to_string()
                .ends_with("--output-max-stdio-bytes")
        );
    }
}
