//! Each node class's fixture parses to the architecture, ISA level and features that
//! class really has.

use kbf_caps::{Arch, ArmVersion, CpuCaps, IsaLevel, ParseError, X86Level};

const BROADWELL: &str = include_str!("fixtures/broadwell_e5_2699_v4.cpuinfo");
const SKYLAKE: &str = include_str!("fixtures/skylake_sp_platinum_8180.cpuinfo");
const AMPERE: &str = include_str!("fixtures/ampere_altra_max_m128_30.cpuinfo");
const M3_ULTRA: &str = include_str!("fixtures/apple_m3_ultra.sysctl");

fn x86(levels: &[X86Level]) -> Vec<IsaLevel> {
    levels.iter().copied().map(IsaLevel::X86_64).collect()
}

/// `text` with one flag removed from every `flags` line.
fn without_flag(text: &str, flag: &str) -> String {
    text.lines()
        .map(|line| match line.split_once(':') {
            Some((key, value)) if key.trim() == "flags" => {
                let kept: Vec<&str> = value.split_whitespace().filter(|f| *f != flag).collect();
                format!("{key}: {}", kept.join(" "))
            }
            _ => line.to_owned(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Catches: a v4 claimed without AVX-512 or a v3 level missing (Broadwell takes v3
/// work), and a parser that reads the `vmx flags` or `bugs` lines as CPU flags.
#[test]
fn broadwell_is_x86_64_v3() {
    let cpu = CpuCaps::from_linux_cpuinfo(BROADWELL).unwrap();
    assert_eq!(cpu.arch(), Arch::X86_64);
    assert_eq!(cpu.level(), Some(IsaLevel::X86_64(X86Level::V3)));
    assert_eq!(
        cpu.levels(),
        x86(&[X86Level::V1, X86Level::V2, X86Level::V3])
    );
    for f in [
        "avx2", "bmi2", "fma", "movbe", "abm", "aes", "rdseed", "adx",
    ] {
        assert!(cpu.has(f), "{f}");
    }
    for f in ["avx512f", "ept_x_only", "cpu_meltdown"] {
        assert!(!cpu.has(f), "{f}");
    }
}

/// Catches: Skylake-SP not reaching v4 (a v4 flag misnamed in the level table, or
/// AVX-512 flags dropped by the parser), so it would never take v4 work.
#[test]
fn skylake_sp_is_x86_64_v4() {
    let cpu = CpuCaps::from_linux_cpuinfo(SKYLAKE).unwrap();
    assert_eq!(cpu.arch(), Arch::X86_64);
    assert_eq!(cpu.level(), Some(IsaLevel::X86_64(X86Level::V4)));
    assert_eq!(cpu.levels(), x86(&X86Level::ALL));
    for f in [
        "avx512f", "avx512bw", "avx512cd", "avx512dq", "avx512vl", "clwb", "pku",
    ] {
        assert!(cpu.has(f), "{f}");
    }
}

/// Catches: a level computation that ignores some of a level's flags (any-of instead
/// of all-of, or only the first flag checked). Removing any one flag the table names
/// from the Skylake fixture must drop the node below that flag's level. It does not
/// catch a flag missing from the table; review against the psABI does.
#[test]
fn every_required_flag_is_load_bearing() {
    for level in X86Level::ALL {
        for &flag in level.adds() {
            let cpu = CpuCaps::from_linux_cpuinfo(&without_flag(SKYLAKE, flag)).unwrap();
            assert!(
                cpu.level()
                    .is_none_or(|l| !l.satisfies(IsaLevel::X86_64(level))),
                "Skylake without {flag} still reaches {}",
                level.name()
            );
        }
    }
    let cpu = CpuCaps::from_linux_cpuinfo(&without_flag(SKYLAKE, "avx512vl")).unwrap();
    assert_eq!(cpu.level(), Some(IsaLevel::X86_64(X86Level::V3)));
}

/// Catches: a union across processor blocks, which would let a machine whose cores
/// differ claim a feature some cores lack.
#[test]
fn features_are_common_to_every_processor() {
    let mixed = format!("{SKYLAKE}{}", without_flag(SKYLAKE, "avx512bw"));
    let cpu = CpuCaps::from_linux_cpuinfo(&mixed).unwrap();
    assert!(!cpu.has("avx512bw"));
    assert_eq!(cpu.level(), Some(IsaLevel::X86_64(X86Level::V3)));
}

/// Catches: the Ampere misread as Armv8.3 or later (it lacks PAuth, JSCVT, FCMA), a
/// required N1 feature lost, or SVE claimed on a core that has none.
#[test]
fn ampere_altra_max_is_armv8_2_without_sve() {
    let cpu = CpuCaps::from_linux_cpuinfo(AMPERE).unwrap();
    assert_eq!(cpu.arch(), Arch::Arm64);
    assert_eq!(cpu.level(), Some(IsaLevel::Arm64(ArmVersion::V8_2)));
    assert_eq!(
        cpu.levels(),
        [ArmVersion::V8_0, ArmVersion::V8_1, ArmVersion::V8_2].map(IsaLevel::Arm64)
    );
    for f in [
        "asimd", "aes", "pmull", "sha1", "sha2", "crc32", "atomics", "asimddp", "fphp", "asimdhp",
        "lrcpc", "dcpop", "asimdrdm",
    ] {
        assert!(cpu.has(f), "{f}");
    }
    assert!(!cpu.has("sve"));
}

/// Catches: a Mac whose features use only Apple's names (a `cpu.feature=aes` request
/// would never match it), a key with value 0 or a count reported as a feature, and a
/// FEAT key dropped instead of reported under its own name.
#[test]
fn m3_ultra_reports_kernel_and_arm_names() {
    let cpu = CpuCaps::from_macos_sysctl(M3_ULTRA).unwrap();
    assert_eq!(cpu.arch(), Arch::Arm64);
    assert_eq!(cpu.level(), Some(IsaLevel::Arm64(ArmVersion::V8_4)));
    for f in [
        "fp", "asimd", "aes", "pmull", "sha1", "sha2", "sha512", "sha3", "crc32", "atomics",
        "asimdrdm", "asimddp", "fphp", "asimdhp", "asimdfhm", "bf16", "i8mm", "bti",
    ] {
        assert!(cpu.has(f), "kernel name {f}");
    }
    for f in [
        "FEAT_AES",
        "FEAT_CSV2",
        "FEAT_PAuth2",
        "armv8_gpi",
        "AdvSIMD",
    ] {
        assert!(cpu.has(f), "sysctl name {f}");
    }
    for f in [
        "sve",
        "sme",
        "FEAT_SME",
        "FEAT_SPECRES",
        "watchpoint",
        "breakpoint",
    ] {
        assert!(!cpu.has(f), "{f}");
    }
}

/// Catches: input with no feature line accepted as a featureless CPU (it would match
/// nothing and look like a real node), and x86 and arm64 lines merged.
#[test]
fn rejects_unreadable_cpuinfo() {
    assert_eq!(
        CpuCaps::from_linux_cpuinfo("processor\t: 0\n"),
        Err(ParseError::NoFeatureLine)
    );
    assert_eq!(
        CpuCaps::from_linux_cpuinfo(&format!("{BROADWELL}{AMPERE}")),
        Err(ParseError::MixedArch)
    );
}
