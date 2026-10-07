//! `sysctl hw.optional` text from an Apple silicon Mac, read into kernel-named features.

use std::collections::{BTreeMap, BTreeSet};

use crate::cpu::ParseError;

/// Sysctl keys (after `hw.optional.`) and the Linux arm64 hwcap names they stand for.
/// A key without a Linux equivalent is still reported under its own name.
const KERNEL_NAMES: &[(&str, &[&str])] = &[
    ("floatingpoint", &["fp"]),
    ("AdvSIMD", &["asimd"]),
    ("neon", &["asimd"]),
    ("armv8_crc32", &["crc32"]),
    ("armv8_1_atomics", &["atomics"]),
    ("arm.FEAT_AES", &["aes"]),
    ("arm.FEAT_PMULL", &["pmull"]),
    ("arm.FEAT_SHA1", &["sha1"]),
    ("arm.FEAT_SHA256", &["sha2"]),
    ("arm.FEAT_SHA512", &["sha512"]),
    ("arm.FEAT_SHA3", &["sha3"]),
    ("arm.FEAT_CRC32", &["crc32"]),
    ("arm.FEAT_LSE", &["atomics"]),
    ("arm.FEAT_LSE2", &["uscat"]),
    ("arm.FEAT_RDM", &["asimdrdm"]),
    ("arm.FEAT_DotProd", &["asimddp"]),
    ("arm.FEAT_FP16", &["fphp", "asimdhp"]),
    ("arm.FEAT_FHM", &["asimdfhm"]),
    ("arm.FEAT_JSCVT", &["jscvt"]),
    ("arm.FEAT_FCMA", &["fcma"]),
    ("arm.FEAT_LRCPC", &["lrcpc"]),
    ("arm.FEAT_LRCPC2", &["ilrcpc"]),
    ("arm.FEAT_DPB", &["dcpop"]),
    ("arm.FEAT_DPB2", &["dcpodp"]),
    ("arm.FEAT_DIT", &["dit"]),
    ("arm.FEAT_FlagM", &["flagm"]),
    ("arm.FEAT_FlagM2", &["flagm2"]),
    ("arm.FEAT_FRINTTS", &["frint"]),
    ("arm.FEAT_SB", &["sb"]),
    ("arm.FEAT_SSBS", &["ssbs"]),
    ("arm.FEAT_BTI", &["bti"]),
    ("arm.FEAT_PAuth", &["paca", "pacg"]),
    ("arm.FEAT_BF16", &["bf16"]),
    ("arm.FEAT_I8MM", &["i8mm"]),
    ("arm.FEAT_RPRES", &["rpres"]),
    ("arm.FEAT_AFP", &["afp"]),
    ("arm.FEAT_ECV", &["ecv"]),
    ("arm.FEAT_WFxT", &["wfxt"]),
    ("arm.FEAT_SME", &["sme"]),
    ("arm.FEAT_SME2", &["sme2"]),
];

const PREFIX: &str = "hw.optional.";

/// Every `hw.optional` key and its value. Blank lines are skipped; any other line
/// must be `hw.optional.<key>: <unsigned integer>`.
fn parse(text: &str) -> Result<BTreeMap<&str, u64>, ParseError> {
    let mut keys = BTreeMap::new();
    for (i, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let bad = || ParseError::SysctlLine {
            line: i + 1,
            text: line.to_owned(),
        };
        let (key, value) = line.split_once(": ").ok_or_else(bad)?;
        let key = key
            .strip_prefix(PREFIX)
            .filter(|k| !k.is_empty())
            .ok_or_else(bad)?;
        let value: u64 = value.trim().parse().map_err(|_| bad())?;
        if keys.insert(key, value).is_some() {
            return Err(ParseError::SysctlDuplicate(format!("{PREFIX}{key}")));
        }
    }
    Ok(keys)
}

/// The feature set of an Apple silicon Mac; see [`crate::CpuCaps::from_macos_sysctl`].
pub(crate) fn features(text: &str) -> Result<BTreeSet<String>, ParseError> {
    let keys = parse(text)?;
    if keys.get("arm64") != Some(&1) {
        return Err(ParseError::NotArm64Mac);
    }
    let mut features = BTreeSet::new();
    for (&key, _) in keys.iter().filter(|&(_, &v)| v == 1) {
        features.insert(key.strip_prefix("arm.").unwrap_or(key).to_owned());
        if let Some((_, names)) = KERNEL_NAMES.iter().find(|(k, _)| *k == key) {
            features.extend(names.iter().map(|&n| n.to_owned()));
        }
    }
    Ok(features)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches: a parser that accepts a line it cannot read (a missing value, a
    /// foreign prefix, a non-integer) instead of naming it, or that lets a repeated
    /// key silently overwrite the first.
    #[test]
    fn rejects_malformed_lines() {
        for bad in [
            "hw.optional.neon",
            "kern.version: 1",
            "hw.optional.neon: yes",
            "hw.optional.: 1",
        ] {
            assert!(
                matches!(parse(bad), Err(ParseError::SysctlLine { line: 1, .. })),
                "{bad:?}"
            );
        }
        assert_eq!(
            parse("hw.optional.neon: 1\nhw.optional.neon: 0"),
            Err(ParseError::SysctlDuplicate("hw.optional.neon".to_owned()))
        );
    }

    /// Catches: treating an Intel Mac (or a truncated capture) as arm64.
    #[test]
    fn requires_arm64() {
        assert_eq!(
            features("hw.optional.avx2_0: 1\n"),
            Err(ParseError::NotArm64Mac)
        );
    }
}
