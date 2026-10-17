//! What a container is given besides the settings `spec` writes: `crun` applies only
//! what `config.json` holds, so the capability set, the masked and read-only paths and
//! the seccomp filter are listed here, from the containers/common release below, and
//! pinned.
//!
//! - **Capabilities:** containers.conf's `default_capabilities` of containers/common
//!   v0.57.4: the eleven in [`CAPABILITIES`], for the
//!   bounding, effective, permitted and ambient sets; the inheritable set is empty.
//!   They are capabilities of the container's own user namespace, whose ids are the
//!   daemon user's subordinate ids: they give an action's root nothing over an inode
//!   the daemon user owns, since that user is not mapped.
//! - **Masked and read-only paths:** `DefaultMaskedPaths` and `DefaultReadOnlyPaths` of
//!   the same release ([`MASKED_PATHS`], [`READONLY_PATHS`]).
//! - **Seccomp:** the same release's `pkg/seccomp/seccomp.json`, kept as
//!   `seccomp.json` beside this file (sha256 [`PROFILE_SHA256`], which a unit test
//!   checks, so an edit is a reviewed change of the pin), resolved for the container's
//!   capability set and the node's architecture the way containers/common resolves it:
//!   an entry with `includes.caps` applies only if the container holds all of them, one
//!   with `excludes.caps` only if it holds none, and one with `arches` only on those
//!   architectures. The result is a plain OCI `linux.seccomp` section.
//! - **Also set:** `noNewPrivileges`, `oom_score_adj` 0, a private cgroup namespace, and
//!   `/sys/fs/cgroup` read-only (`spec`).
//!
//! Left out, on purpose: the `net.ipv4.ping_group_range` and
//! `net.ipv4.ip_unprivileged_port_start` sysctls (an action has loopback only) and the
//! `gid=5` option of `/dev/pts` (`crun spec --rootless` leaves it out too).

use std::collections::HashSet;

use oci_spec::runtime::{Capability, LinuxSeccomp};
use serde_json::{Value, json};

/// The capabilities every action's container holds (`CAP_` names, as OCI writes them).
pub(crate) const CAPABILITIES: [&str; 11] = [
    "CAP_CHOWN",
    "CAP_DAC_OVERRIDE",
    "CAP_FOWNER",
    "CAP_FSETID",
    "CAP_KILL",
    "CAP_NET_BIND_SERVICE",
    "CAP_SETFCAP",
    "CAP_SETGID",
    "CAP_SETPCAP",
    "CAP_SETUID",
    "CAP_SYS_CHROOT",
];

/// [`CAPABILITIES`] as the typed set `oci-spec` takes. A name it does not know would be
/// left out, so the unit tests count the set.
pub(crate) fn held_capabilities() -> HashSet<Capability> {
    CAPABILITIES
        .iter()
        .filter_map(|name| capability(name))
        .collect()
}

/// The capability `name` (`CAP_...`) names, if `oci-spec` knows it.
fn capability(name: &str) -> Option<Capability> {
    serde_json::from_value(Value::String(name.to_owned())).ok()
}

/// Paths that read as empty or are unreadable inside the container.
pub(crate) const MASKED_PATHS: [&str; 12] = [
    "/proc/acpi",
    "/proc/kcore",
    "/proc/keys",
    "/proc/latency_stats",
    "/proc/sched_debug",
    "/proc/scsi",
    "/proc/timer_list",
    "/proc/timer_stats",
    "/sys/dev/block",
    "/sys/devices/virtual/powercap",
    "/sys/firmware",
    "/sys/fs/selinux",
];

/// Paths that are visible but mounted read-only.
pub(crate) const READONLY_PATHS: [&str; 6] = [
    "/proc/asound",
    "/proc/bus",
    "/proc/fs",
    "/proc/irq",
    "/proc/sys",
    "/proc/sysrq-trigger",
];

/// The pinned profile: containers/common v0.57.4 `pkg/seccomp/seccomp.json`.
const PROFILE: &str = include_str!("seccomp.json");

/// The sha256 of [`PROFILE`] as pinned. Changing the profile changes this in the same
/// change, which is what a reviewer reads.
#[cfg(test)]
pub(crate) const PROFILE_SHA256: &str =
    "cc374cf23846ce1f62f4dc807a8e2b8673c783c6f56cb475467621035d281e6c";

/// The architecture name containers/common filters on (Go's `GOARCH`) for Rust's
/// `std::env::consts::ARCH`; `None` for one the profile has no filter for.
pub(crate) fn profile_arch(rust_arch: &str) -> Option<&'static str> {
    match rust_arch {
        "x86_64" => Some("amd64"),
        "aarch64" => Some("arm64"),
        _ => None,
    }
}

/// Why the pinned profile cannot be turned into a `linux.seccomp` section.
#[derive(Debug, thiserror::Error)]
pub(crate) enum SeccompError {
    #[error("the seccomp profile has no filter for architecture {0:?}")]
    Arch(String),
    #[error("the pinned seccomp profile: {0}")]
    Profile(String),
}

/// The pinned profile for a node of Go architecture `arch` (`amd64`, `arm64`) whose
/// containers hold `caps`.
///
/// # Errors
/// `arch` has no entry in the profile's architecture map, or the profile is malformed
/// (the unit tests parse it, so only an edit can break that).
pub(crate) fn seccomp(arch: &str, caps: &[&str]) -> Result<LinuxSeccomp, SeccompError> {
    from_profile(PROFILE, arch, caps)
}

fn malformed(what: &str) -> SeccompError {
    SeccompError::Profile(what.to_owned())
}

/// [`seccomp`] for the profile text `profile`.
fn from_profile(profile: &str, arch: &str, caps: &[&str]) -> Result<LinuxSeccomp, SeccompError> {
    let profile: Value =
        serde_json::from_str(profile).map_err(|e| SeccompError::Profile(e.to_string()))?;
    let native = native_arch(arch).ok_or_else(|| SeccompError::Arch(arch.to_owned()))?;
    let map = profile["archMap"]
        .as_array()
        .ok_or_else(|| malformed("no archMap"))?;
    let entry = map
        .iter()
        .find(|e| e["architecture"] == native)
        .ok_or_else(|| SeccompError::Arch(arch.to_owned()))?;
    let mut architectures = vec![entry["architecture"].clone()];
    architectures.extend(
        entry["subArchitectures"]
            .as_array()
            .into_iter()
            .flatten()
            .cloned(),
    );
    let calls = profile["syscalls"]
        .as_array()
        .ok_or_else(|| malformed("no syscalls"))?;
    let syscalls: Vec<Value> = calls
        .iter()
        .filter(|call| applies(call, arch, caps))
        .map(resolved)
        .collect();
    let section = json!({
        "defaultAction": profile["defaultAction"],
        "defaultErrnoRet": profile["defaultErrnoRet"],
        "architectures": architectures,
        "syscalls": syscalls,
    });
    serde_json::from_value(section).map_err(|e| SeccompError::Profile(e.to_string()))
}

/// The profile's `SCMP_ARCH_*` name of the native architecture `arch` (Go's name).
fn native_arch(arch: &str) -> Option<&'static str> {
    match arch {
        "amd64" => Some("SCMP_ARCH_X86_64"),
        "arm64" => Some("SCMP_ARCH_AARCH64"),
        _ => None,
    }
}

/// Whether the profile entry `call` applies to a container of architecture `arch`
/// holding `caps`: containers/common's rules for `includes` and `excludes`.
fn applies(call: &Value, arch: &str, caps: &[&str]) -> bool {
    let names = |filter: &str, key: &str| -> Vec<String> {
        call[filter][key]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect()
    };
    let holds = |cap: &String| caps.contains(&cap.as_str());
    let (include_arches, exclude_arches) =
        (names("includes", "arches"), names("excludes", "arches"));
    if !include_arches.is_empty() && !include_arches.iter().any(|a| a == arch) {
        return false;
    }
    if exclude_arches.iter().any(|a| a == arch) {
        return false;
    }
    names("includes", "caps").iter().all(holds) && !names("excludes", "caps").iter().any(holds)
}

/// The OCI form of a profile entry: its names, action, errno and arguments only.
fn resolved(call: &Value) -> Value {
    let mut out = json!({ "names": call["names"], "action": call["action"] });
    if let Some(errno) = call.get("errnoRet").filter(|v| !v.is_null()) {
        out["errnoRet"] = errno.clone();
    }
    if let Some(args) = call["args"].as_array().filter(|a| !a.is_empty()) {
        out["args"] = Value::Array(args.clone());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest as _, Sha256};

    fn sha256(text: &str) -> String {
        hex::encode(Sha256::digest(text.as_bytes()))
    }

    fn names_of(section: &LinuxSeccomp, action: &str) -> Vec<String> {
        let section = serde_json::to_value(section).expect("serialize");
        section["syscalls"]
            .as_array()
            .expect("syscalls")
            .iter()
            .filter(|c| c["action"] == action)
            .flat_map(|c| c["names"].as_array().cloned().unwrap_or_default())
            .filter_map(|n| n.as_str().map(str::to_owned))
            .collect()
    }

    /// Catches an edit of the vendored profile that does not move its pin (and so
    /// escapes review as "the containers/common profile").
    #[test]
    fn the_profile_is_the_pinned_one() {
        assert_eq!(sha256(PROFILE), PROFILE_SHA256);
    }

    /// Catches the default action being anything but an errno (a profile that allows
    /// what it does not name), and the architectures not following the node.
    #[test]
    fn the_default_is_an_errno_and_the_architectures_follow_the_node() {
        let amd64 = serde_json::to_value(seccomp("amd64", &CAPABILITIES).expect("amd64"))
            .expect("serialize");
        assert_eq!(amd64["defaultAction"], "SCMP_ACT_ERRNO");
        assert_eq!(amd64["defaultErrnoRet"], 38);
        assert_eq!(
            amd64["architectures"],
            json!(["SCMP_ARCH_X86_64", "SCMP_ARCH_X86", "SCMP_ARCH_X32"])
        );
        let arm64 = serde_json::to_value(seccomp("arm64", &CAPABILITIES).expect("arm64"))
            .expect("serialize");
        assert_eq!(
            arm64["architectures"],
            json!(["SCMP_ARCH_AARCH64", "SCMP_ARCH_ARM"])
        );
        assert!(matches!(
            seccomp("riscv64", &CAPABILITIES),
            Err(SeccompError::Arch(_))
        ));
    }

    /// Catches capability filtering applied backwards: without `CAP_SYS_ADMIN` the
    /// filter must refuse `bpf`; holding it must allow it. The pinned capability set
    /// has none of it. (`mount` and `unshare` are in the pinned profile's plain allow
    /// list: the kernel refuses them without the capability.)
    #[test]
    fn the_capability_set_decides_the_privileged_calls() {
        let plain = seccomp("amd64", &CAPABILITIES).expect("plain");
        let denied = names_of(&plain, "SCMP_ACT_ERRNO");
        let allowed = names_of(&plain, "SCMP_ACT_ALLOW");
        assert!(denied.iter().any(|n| n == "bpf"), "bpf denied");
        assert!(!allowed.iter().any(|n| n == "bpf"), "bpf not allowed");
        let mut admin = CAPABILITIES.to_vec();
        admin.push("CAP_SYS_ADMIN");
        let admin = seccomp("amd64", &admin).expect("admin");
        let (denied, allowed) = (
            names_of(&admin, "SCMP_ACT_ERRNO"),
            names_of(&admin, "SCMP_ACT_ALLOW"),
        );
        assert!(allowed.iter().any(|n| n == "bpf"), "bpf allowed with it");
        assert!(!denied.iter().any(|n| n == "bpf"), "bpf not denied with it");
    }

    /// Catches an architecture filter ignored: a call only 32-bit ARM or s390 has must
    /// not appear for amd64, and one amd64 has must.
    #[test]
    fn architecture_filters_apply() {
        let amd64 = names_of(
            &seccomp("amd64", &CAPABILITIES).expect("amd64"),
            "SCMP_ACT_ALLOW",
        );
        assert!(
            amd64.iter().any(|n| n == "arch_prctl"),
            "amd64 has arch_prctl"
        );
        assert!(
            !amd64.iter().any(|n| n == "cacheflush"),
            "cacheflush is arm"
        );
        let arm64 = names_of(
            &seccomp("arm64", &CAPABILITIES).expect("arm64"),
            "SCMP_ACT_ALLOW",
        );
        assert!(
            arm64.iter().any(|n| n == "cacheflush"),
            "arm64 has cacheflush"
        );
        assert!(!arm64.iter().any(|n| n == "arch_prctl"), "arm64 has none");
    }

    /// Catches argument filters and errno values being dropped on the way to the OCI
    /// form (a call allowed only for some arguments would then be allowed for all).
    #[test]
    fn arguments_and_errnos_survive() {
        let section =
            serde_json::to_value(seccomp("amd64", &CAPABILITIES).expect("amd64")).expect("json");
        let calls = section["syscalls"].as_array().expect("syscalls");
        assert!(
            calls
                .iter()
                .any(|c| c["action"] == "SCMP_ACT_ALLOW" && c["args"].is_array()),
            "an allow with arguments"
        );
        assert!(calls.iter().any(|c| c["errnoRet"] == 1), "an EPERM entry");
    }

    /// Catches a capability name `oci-spec` would not take (a typo in the list, which
    /// would leave the capability out), and a held set that is not the eleven.
    #[test]
    fn the_capability_names_are_oci_specs() {
        assert_eq!(held_capabilities().len(), CAPABILITIES.len());
        assert_eq!(CAPABILITIES.len(), 11);
        assert!(capability("CAP_CHOWN").is_some());
        assert!(capability("CAP_NOT_ONE").is_none());
    }

    /// Catches a malformed profile, or a node the profile has no filter for, producing a
    /// section or panicking instead of saying which: text that is not JSON, no `archMap`,
    /// the architecture missing from it, no `syscalls`, and a profile `oci-spec` cannot
    /// read as a section (no default action).
    #[test]
    fn a_profile_that_cannot_be_used_says_why() {
        let why = |profile: &str, arch: &str| {
            from_profile(profile, arch, &[])
                .expect_err("unusable")
                .to_string()
        };
        assert!(why("not json", "amd64").starts_with("the pinned seccomp profile: "));
        assert_eq!(why("{}", "amd64"), "the pinned seccomp profile: no archMap");
        assert_eq!(
            why(r#"{"archMap":[]}"#, "amd64"),
            "the seccomp profile has no filter for architecture \"amd64\""
        );
        let arch = r#"{"archMap":[{"architecture":"SCMP_ARCH_X86_64"}]}"#;
        assert_eq!(
            why(arch, "amd64"),
            "the pinned seccomp profile: no syscalls"
        );
        let no_default = r#"{"archMap":[{"architecture":"SCMP_ARCH_X86_64"}],"syscalls":[]}"#;
        assert!(why(no_default, "amd64").starts_with("the pinned seccomp profile: "));
        assert_eq!(
            why(PROFILE, "riscv64"),
            "the seccomp profile has no filter for architecture \"riscv64\""
        );
    }

    /// Catches an entry's `includes` and `excludes` read wrongly, each of the four
    /// filters on its own: an arch the entry is for, an arch it excludes, a capability
    /// it needs, a capability that rules it out.
    #[test]
    fn an_entry_applies_by_architecture_and_capability() {
        let call = |filters: Value| {
            let mut call = json!({"names": ["x"], "action": "SCMP_ACT_ALLOW"});
            call["includes"] = filters["includes"].clone();
            call["excludes"] = filters["excludes"].clone();
            call
        };
        let applies_to = |filters: Value, caps: &[&str]| applies(&call(filters), "amd64", caps);
        assert!(applies_to(json!({}), &[]));
        assert!(applies_to(json!({"includes": {"arches": ["amd64"]}}), &[]));
        assert!(!applies_to(json!({"includes": {"arches": ["arm64"]}}), &[]));
        assert!(!applies_to(json!({"excludes": {"arches": ["amd64"]}}), &[]));
        assert!(applies_to(json!({"excludes": {"arches": ["arm64"]}}), &[]));
        let needs = json!({"includes": {"caps": ["CAP_A"]}});
        assert!(applies_to(needs.clone(), &["CAP_A", "CAP_B"]));
        assert!(!applies_to(needs, &["CAP_B"]));
        let rules_out = json!({"excludes": {"caps": ["CAP_A"]}});
        assert!(applies_to(rules_out.clone(), &["CAP_B"]));
        assert!(!applies_to(rules_out, &["CAP_A"]));
    }

    /// The architecture a node reports is one the profile covers.
    #[test]
    fn the_nodes_architectures_have_a_filter() {
        assert_eq!(profile_arch("x86_64"), Some("amd64"));
        assert_eq!(profile_arch("aarch64"), Some("arm64"));
        assert_eq!(profile_arch("riscv64"), None);
    }
}
