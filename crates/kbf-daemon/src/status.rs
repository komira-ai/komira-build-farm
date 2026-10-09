//! The node's software status: what operating system, kernel, daemon and Xcodes it
//! runs, sent to the server as `NodeStatus` after each `Welcome`
//! (`docs/design/fleet-updates.md` section 3.1).
//!
//! Status routes no work, so it is not part of the node report and does not change the
//! report hash. A Linux node reads `/etc/os-release` (or `/usr/lib/os-release`, its
//! fallback) for `NAME`, `VERSION_ID` and `BUILD_ID`, and `/proc/sys/kernel/osrelease`
//! for the kernel release. A Mac asks `sw_vers` for its product name, version and
//! build. The Xcode builds are the node report's `xcode` entries: whatever discovered
//! them (the native driver) is the one source, so status and placement never disagree.
//! Every installed Xcode, ready or not, with why one is not and how to fix it, comes
//! from the driver too ([`DriverReport`], issue #164). A field that cannot be read is
//! left empty; status never stops a node from joining.

use std::path::Path;

use kbf_proto::worker::{NodeStatus, XcodeStatus};

use crate::report::NodeReport;

/// The node report key whose values are the installed Xcode builds.
pub const XCODE_KEY: &str = "xcode";

/// What a driver reports that may change while the daemon runs: the node report
/// entries it adds (each ready Xcode's `xcode` entry among them) and every Xcode it
/// found, ready or not, for `NodeStatus.xcodes`. See `Daemon::with_driver_report`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DriverReport {
    /// Entries added to the report the daemon was started with.
    pub entries: Vec<(String, String)>,
    /// Every installed Xcode, in the order the driver found them.
    pub xcodes: Vec<XcodeStatus>,
}

/// What the operating system says about itself. An empty field is not known.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Software {
    /// `macOS`, or the `os-release` `NAME` (`Ubuntu`).
    pub os_name: String,
    /// `15.1`, or the `os-release` `VERSION_ID` (`24.04`).
    pub os_version: String,
    /// `24B83`, or the `os-release` `BUILD_ID` where the distribution sets one.
    pub os_build: String,
    /// The running kernel's release (Linux).
    pub kernel: String,
}

impl Software {
    /// Detects this machine's software. Never fails: what cannot be read stays empty.
    #[must_use]
    pub fn detect() -> Self {
        detect_here()
    }

    /// The `NodeStatus` this node sends: this software, the daemon's version, and the
    /// `xcode` entries of `report`, sorted (the report keeps its entries sorted).
    #[must_use]
    pub fn status(&self, report: &NodeReport) -> NodeStatus {
        NodeStatus {
            os_name: self.os_name.clone(),
            os_version: self.os_version.clone(),
            os_build: self.os_build.clone(),
            kernel: self.kernel.clone(),
            daemon_version: env!("CARGO_PKG_VERSION").to_owned(),
            xcode_builds: report
                .capabilities()
                .iter()
                .filter(|c| c.key == XCODE_KEY)
                .map(|c| c.value.clone())
                .collect(),
            // The driver's, which the daemon adds (`DriverReport`).
            xcodes: Vec::new(),
        }
    }
}

/// Where Linux keeps the release description, and its fallback (os-release(5)).
pub const OS_RELEASE: [&str; 2] = ["/etc/os-release", "/usr/lib/os-release"];

/// Where Linux publishes the running kernel's release, as `uname -r` prints it.
pub const KERNEL_RELEASE: &str = "/proc/sys/kernel/osrelease";

/// A Linux node's software from the first readable file of `os_release` and from
/// `kernel_release`.
#[must_use]
pub fn linux_software(os_release: &[&Path], kernel_release: &Path) -> Software {
    let text = os_release
        .iter()
        .find_map(|path| std::fs::read_to_string(path).ok())
        .unwrap_or_default();
    let field = |key| os_release_field(&text, key).unwrap_or_default();
    let kernel = std::fs::read_to_string(kernel_release).unwrap_or_default();
    Software {
        os_name: field("NAME"),
        os_version: field("VERSION_ID"),
        os_build: field("BUILD_ID"),
        kernel: kernel.trim().to_owned(),
    }
}

/// The value of `key` in os-release text: the last assignment wins, as in a shell;
/// one pair of matching quotes is removed and, inside double quotes, the backslash
/// escapes `\"`, `\\`, `` \` `` and `\$` are undone (os-release(5)).
#[must_use]
pub fn os_release_field(text: &str, key: &str) -> Option<String> {
    text.lines()
        .filter_map(|line| line.trim().strip_prefix(key)?.strip_prefix('='))
        .next_back()
        .map(unquote)
}

fn unquote(value: &str) -> String {
    let inner = |q: char| value.strip_prefix(q).and_then(|v| v.strip_suffix(q));
    if let Some(single) = inner('\'') {
        return single.to_owned();
    }
    let Some(double) = inner('"') else {
        return value.to_owned();
    };
    let mut out = String::with_capacity(double.len());
    let mut chars = double.chars();
    while let Some(c) = chars.next() {
        match (c, chars.clone().next()) {
            ('\\', Some(next @ ('"' | '\\' | '`' | '$'))) => {
                out.push(next);
                chars.next();
            }
            _ => out.push(c),
        }
    }
    out
}

/// A Mac's software from `sw_vers` output (`ProductName:\tmacOS` and so on, one per
/// line). The kernel is left empty: Darwin's release says nothing an operator acts on.
#[must_use]
pub fn macos_software(sw_vers: &str) -> Software {
    let field = |key: &str| {
        sw_vers
            .lines()
            .find_map(|line| line.strip_prefix(key)?.strip_prefix(':'))
            .map(|v| v.trim().to_owned())
            .unwrap_or_default()
    };
    Software {
        os_name: field("ProductName"),
        os_version: field("ProductVersion"),
        os_build: field("BuildVersion"),
        kernel: String::new(),
    }
}

/// Where macOS keeps `sw_vers`.
pub const SW_VERS: &str = "/usr/bin/sw_vers";

/// The standard output of `program`, or `None` (logged) if it cannot be run or fails.
#[cfg(any(target_os = "macos", test))]
fn output_of(program: &str) -> Option<String> {
    let out = std::process::Command::new(program)
        .output()
        .map_err(|e| tracing::warn!(program, "cannot run: {e}"))
        .ok()?;
    if !out.status.success() {
        tracing::warn!(program, status = %out.status, "failed");
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(target_os = "linux")]
fn detect_here() -> Software {
    let os_release = OS_RELEASE.map(Path::new);
    linux_software(&os_release, Path::new(KERNEL_RELEASE))
}

#[cfg(target_os = "macos")]
fn detect_here() -> Software {
    output_of(SW_VERS).map_or_else(Software::default, |out| macos_software(&out))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn detect_here() -> Software {
    Software::default()
}

#[cfg(test)]
mod tests {
    use super::*;

    const UBUNTU: &str = "PRETTY_NAME=\"Ubuntu 24.04.1 LTS\"\nNAME=\"Ubuntu\"\n\
        VERSION_ID=\"24.04\"\nVERSION=\"24.04.1 LTS (Noble Numbat)\"\nID=ubuntu\n";

    /// Catches: reading `PRETTY_NAME` or `VERSION` for the name and version (a
    /// `NAME` prefix match), quotes kept, and an unset `BUILD_ID` reported as text.
    #[test]
    fn os_release_fields_are_read_exactly() {
        assert_eq!(os_release_field(UBUNTU, "NAME").as_deref(), Some("Ubuntu"));
        assert_eq!(
            os_release_field(UBUNTU, "VERSION_ID").as_deref(),
            Some("24.04")
        );
        assert_eq!(os_release_field(UBUNTU, "ID").as_deref(), Some("ubuntu"));
        assert_eq!(os_release_field(UBUNTU, "BUILD_ID"), None);
    }

    /// Catches: the first assignment kept instead of the last, single quotes kept, and
    /// escapes inside double quotes left in (or undone where os-release keeps them).
    #[test]
    fn os_release_quoting_follows_the_shell() {
        let text = "NAME=first\nNAME='Fedora Linux'\n\
                    BUILD_ID=\"a \\\"b\\\" \\\\ \\` \\$ \\n\"\nVERSION_ID=\"\"\nEMPTY=\nODD=\"x\n";
        assert_eq!(
            os_release_field(text, "NAME").as_deref(),
            Some("Fedora Linux")
        );
        assert_eq!(
            os_release_field(text, "BUILD_ID").as_deref(),
            Some("a \"b\" \\ ` $ \\n")
        );
        assert_eq!(os_release_field(text, "VERSION_ID").as_deref(), Some(""));
        assert_eq!(os_release_field(text, "EMPTY").as_deref(), Some(""));
        assert_eq!(os_release_field(text, "ODD").as_deref(), Some("\"x"));
        // A trailing backslash has nothing to escape and is kept.
        assert_eq!(os_release_field("K=\"a\\\"", "K").as_deref(), Some("a\\"));
    }

    /// Catches: a field read from the wrong line, and the value kept with its tab.
    #[test]
    fn sw_vers_output_is_parsed() {
        let out = "ProductName:\t\tmacOS\nProductVersion:\t\t15.1\nBuildVersion:\t\t24B83\n";
        assert_eq!(
            macos_software(out),
            Software {
                os_name: "macOS".to_owned(),
                os_version: "15.1".to_owned(),
                os_build: "24B83".to_owned(),
                kernel: String::new(),
            }
        );
        assert_eq!(macos_software(""), Software::default());
    }

    /// Catches: a failed or missing program taken as empty output instead of unknown.
    #[test]
    fn output_of_reports_only_success() {
        assert_eq!(output_of("/bin/echo").as_deref(), Some("\n"));
        assert_eq!(output_of("/bin/false"), None);
        assert_eq!(output_of("/nonexistent/sw_vers"), None);
    }

    /// Catches: Xcode builds taken from another key, or status built from the report's
    /// hash-relevant entries in a way that changes the report.
    #[test]
    fn status_carries_the_reports_xcode_builds_and_the_daemon_version() {
        let report = NodeReport::new([
            ("xcode", "16C5032a"),
            ("os", "macos"),
            ("xcode", "15F31d"),
            ("label.xcode", "nope"),
        ]);
        let before = report.clone();
        let software = macos_software("ProductName: macOS\n");
        let status = software.status(&report);
        assert_eq!(status.xcode_builds, ["15F31d", "16C5032a"]);
        assert_eq!(status.os_name, "macOS");
        assert_eq!(status.daemon_version, env!("CARGO_PKG_VERSION"));
        assert_eq!(report, before);
    }

    /// Catches: detection that fails or reads nothing on this (Linux) host.
    #[cfg(target_os = "linux")]
    #[test]
    fn detect_reads_this_host() {
        let got = Software::detect();
        let kernel = std::fs::read_to_string(KERNEL_RELEASE).expect("kernel release");
        assert_eq!(got.kernel, kernel.trim());
        assert!(!got.os_name.is_empty(), "{got:?}");
    }
}
