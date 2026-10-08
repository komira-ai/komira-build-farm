//! Reading REAPI platforms as requests, node reports as capabilities, and how an unmet
//! requirement reads.

use kbf_caps::{
    Arch, ArmVersion, Consumable, CpuCaps, FromPlatformError, IsaLevel, NodeCaps, ReportError,
    Request, RequestError, UnknownArch, Unmet, X86Level,
};

fn from(props: &[(&str, &str)]) -> Result<Request, FromPlatformError> {
    Request::from_platform(props.iter().copied())
}

fn req(props: &[(&str, &str)]) -> Request {
    Request::parse(props.iter().copied()).unwrap()
}

fn report(entries: &[(&str, &str)]) -> Result<NodeCaps, ReportError> {
    NodeCaps::from_report(entries.iter().copied())
}

/// A Linux x86-64 v3 node and an Apple silicon Mac, as their daemons report them.
fn linux_v3() -> NodeCaps {
    let mut entries = vec![("arch", "x86_64"), ("os", "linux"), ("cpus", "8")];
    for f in X86Level::ALL.iter().take(3).flat_map(|l| l.adds()) {
        entries.push(("cpu.features", f));
    }
    report(&entries).unwrap()
}

fn mac() -> NodeCaps {
    let cpu = CpuCaps::from_macos_sysctl(include_str!("fixtures/apple_m3_ultra.sysctl")).unwrap();
    let mut entries = vec![("arch", "arm64"), ("os", "macos"), ("label.pool", "darwin")];
    for f in cpu.features() {
        entries.push(("cpu.features", f));
    }
    report(&entries).unwrap()
}

/// Catches: `OSFamily` passed through verbatim (Bazel sends `Linux`, Buck2 `linux`; a
/// node reports `linux`), or a Mac's family names not mapped to the report's `macos`.
/// Either sends every action of that family to the wrong place or nowhere.
#[test]
fn os_family_becomes_the_reports_os() {
    for linux in ["linux", "Linux", "LINUX"] {
        assert_eq!(from(&[("OSFamily", linux)]), Ok(req(&[("os", "linux")])));
    }
    for mac in ["darwin", "Darwin", "macos", "MacOSX", "osx"] {
        assert_eq!(from(&[("OSFamily", mac)]), Ok(req(&[("os", "macos")])));
    }
    let linux = from(&[("OSFamily", "Linux")]).unwrap();
    assert!(linux.matches(&linux_v3()));
    assert_eq!(
        linux.unmet(&mac()),
        [Unmet::Exact {
            key: "os",
            want: "linux"
        }]
    );
}

/// Catches: `ISA`/`Arch` ignored, or REAPI's names (`x86-64`, `arm-a64`) and the
/// common aliases not mapped to the report's `arch`, so an arm64 action lands on an
/// x86-64 node. An ISA level also asks for its level, not just its family.
#[test]
fn isa_becomes_arch_and_level() {
    for (key, value, arch) in [
        ("ISA", "x86-64", "x86_64"),
        ("ISA", "X86_64", "x86_64"),
        ("Arch", "amd64", "x86_64"),
        ("ISA", "arm-a64", "arm64"),
        ("Arch", "arm64", "arm64"),
        ("ISA", "AArch64", "arm64"),
    ] {
        assert_eq!(from(&[(key, value)]), Ok(req(&[("arch", arch)])), "{value}");
    }
    assert_eq!(
        from(&[("ISA", "x86-64-v3")]),
        Ok(req(&[("arch", "x86_64"), ("isa_level", "x86-64-v3")]))
    );
    assert_eq!(
        from(&[("Arch", "ARMv8.2-A")]),
        Ok(req(&[("arch", "arm64"), ("isa_level", "armv8.2-a")]))
    );
    let arm = from(&[("ISA", "arm-a64")]).unwrap();
    assert!(arm.matches(&mac()));
    assert_eq!(arm.unmet(&linux_v3()), [Unmet::Arch { want: Arch::Arm64 }]);
    assert!(from(&[("ISA", "x86-64-v3")]).unwrap().matches(&linux_v3()));
    assert!(!from(&[("ISA", "x86-64-v4")]).unwrap().matches(&linux_v3()));
}

/// Catches: a platform no daemon can ever run (another OS, another architecture)
/// accepted, so it waits out the bound instead of failing at once, or refused as
/// INVALID_ARGUMENT although REAPI allows it.
#[test]
fn a_platform_no_daemon_runs_is_never_served() {
    for props in [
        [("OSFamily", "Windows")],
        [("os", "freebsd")],
        [("ISA", "x86-32")],
        [("Arch", "power-isa-le")],
    ] {
        let Err(FromPlatformError::NeverServed(why)) = from(&props) else {
            panic!("{props:?} was not refused as never served");
        };
        let value = props[0].1.to_ascii_lowercase();
        assert!(why.to_ascii_lowercase().contains(&value), "{why}");
    }
}

/// Catches: kbf's own keys dropped in translation, `gpu` matched as a capability
/// (the scheduler books it), and properties that are not capabilities refused, which
/// would fail every action carrying a `container-image` or `kbf-lease`.
#[test]
fn own_keys_pass_through_and_others_are_left_alone() {
    let props = [
        ("os", "linux"),
        ("isa_level", "x86-64-v2"),
        ("cpu.feature", "avx2"),
        ("label.pool", "big"),
        ("cpus", "4"),
        ("gpu", "1"),
        ("container-image", "docker://img@sha256:00"),
        ("kbf-lease", "whole_machine"),
        ("Pool", "default"),
    ];
    assert_eq!(
        from(&props),
        Ok(req(&[
            ("os", "linux"),
            ("isa_level", "x86-64-v2"),
            ("cpu.feature", "avx2"),
            ("label.pool", "big"),
            ("cpus", "4"),
        ]))
    );
    assert_eq!(from(&[]), Ok(Request::default()));
}

/// Catches: a property name kbf reads ignored because of its case (`osfamily=darwin`,
/// `isa=arm-a64`, `OS=macos` dropped as unknown, so the action may run on any worker,
/// Linux included); a label's own name folded to lower case (labels are compared
/// exactly); and a property kbf does not read taken as one it does.
#[test]
fn property_names_are_read_in_any_case() {
    for (sent, read) in [
        ("OSFamily", "OSFamily"),
        ("osfamily", "OSFamily"),
        ("OSFAMILY", "OSFamily"),
        ("isa", "ISA"),
        ("Isa", "ISA"),
        ("Arch", "Arch"),
        ("ARCH", "Arch"),
        ("arch", "arch"),
        ("OS", "os"),
        ("Os_Image", "os_image"),
        ("ISA_Level", "isa_level"),
        ("CPU.Feature", "cpu.feature"),
        ("Cpus", "cpus"),
        ("GPU", "gpu"),
        ("gpu", "gpu"),
        ("KBF-Lease", "kbf-lease"),
        ("label.pool", "label.pool"),
        ("Label.Pool", "label.Pool"),
        ("LABEL.x", "label.x"),
    ] {
        assert_eq!(
            kbf_caps::property_name(sent).as_deref(),
            Some(read),
            "{sent}"
        );
    }
    for ignored in [
        "container-image",
        "Pool",
        "dockerNetwork",
        "LABEL.",
        "label.",
        "labels",
        "Label",
        "",
        "ÖS",
        "label\u{e9}x",
    ] {
        assert_eq!(kbf_caps::property_name(ignored), None, "{ignored:?}");
    }
    assert_eq!(kbf_caps::REAPI_KEYS, ["OSFamily", "ISA", "Arch"]);

    // Read as requests: each spelling asks for what the canonical one asks for.
    for (props, want) in [
        (vec![("osfamily", "darwin")], vec![("os", "macos")]),
        (vec![("OSFAMILY", "Linux")], vec![("os", "linux")]),
        (vec![("isa", "arm-a64")], vec![("arch", "arm64")]),
        (vec![("ARCH", "amd64")], vec![("arch", "x86_64")]),
        (
            vec![("Isa", "x86-64-v3")],
            vec![("arch", "x86_64"), ("isa_level", "x86-64-v3")],
        ),
        (vec![("OS", "macos")], vec![("os", "macos")]),
        (
            vec![("Label.Pool", "darwin")],
            vec![("label.Pool", "darwin")],
        ),
        (vec![("CPUS", "4"), ("GPU", "1")], vec![("cpus", "4")]),
    ] {
        assert_eq!(from(&props), Ok(req(&want)), "{props:?}");
    }
    let mac_only = from(&[("osfamily", "darwin")]).unwrap();
    assert!(mac_only.matches(&mac()));
    assert!(
        !mac_only.matches(&linux_v3()),
        "a lowercase osfamily ignored"
    );
    assert!(!from(&[("isa", "arm-a64")]).unwrap().matches(&linux_v3()));

    // In another case, a value no daemon runs is still never served, and named as sent.
    let Err(FromPlatformError::NeverServed(why)) = from(&[("isa", "x86-32")]) else {
        panic!("isa=x86-32 was not refused as never served");
    };
    assert!(why.contains("isa=\"x86-32\""), "{why}");
    assert!(matches!(
        from(&[("osfamily", "Windows")]),
        Err(FromPlatformError::NeverServed(_))
    ));

    // One name in two spellings is one key given twice.
    for props in [
        [("OSFamily", "linux"), ("osfamily", "linux")],
        [("os", "linux"), ("OS", "linux")],
        [("ISA", "arm-a64"), ("arch", "arm64")],
    ] {
        assert!(
            matches!(
                from(&props),
                Err(FromPlatformError::Invalid(RequestError::Repeated(_)))
            ),
            "{props:?}"
        );
    }
}

/// Catches: one requirement named twice (through `OSFamily` and `os`) silently
/// resolved by order, and a malformed kbf value accepted.
#[test]
fn malformed_platforms_are_invalid() {
    assert_eq!(
        from(&[("OSFamily", "linux"), ("os", "linux")]),
        Err(FromPlatformError::Invalid(RequestError::Repeated(
            "os".to_owned()
        )))
    );
    assert_eq!(
        from(&[("ISA", "x86-64"), ("arch", "x86_64")]),
        Err(FromPlatformError::Invalid(RequestError::Repeated(
            "arch".to_owned()
        )))
    );
    let bad = from(&[("cpus", "many")]).unwrap_err();
    assert!(matches!(
        bad,
        FromPlatformError::Invalid(RequestError::BadValue { .. })
    ));
    assert!(bad.to_string().contains("many"));
}

/// Catches: a report read without its features (so every level and feature request
/// fails), without its exact values or capacity, or one whose repeated or malformed
/// entries are accepted.
#[test]
fn a_report_becomes_node_caps() {
    let node = report(&[
        ("arch", "x86_64"),
        ("cpu.features", "avx2"),
        ("cpu.features", "sse2"),
        ("os", "linux"),
        ("label.rack", "r1"),
        ("cpus", "16"),
        ("mem_gib", "64"),
        ("gpu", "2"),
        ("isa_level", "x86-64-v1"),
        ("drivers", "container"),
    ])
    .unwrap();
    assert_eq!(node.cpu.arch(), Arch::X86_64);
    assert!(node.cpu.has("avx2") && node.cpu.has("sse2"));
    assert_eq!(node.exact.get("os").map(String::as_str), Some("linux"));
    assert_eq!(node.exact.get("label.rack").map(String::as_str), Some("r1"));
    assert!(!node.exact.contains_key("drivers"));
    assert_eq!(node.consumables.get(&Consumable::Gpus), Some(&2));
    assert_eq!(node.consumables.get(&Consumable::MemGib), Some(&64));
    assert!(req(&[("cpus", "16"), ("os", "linux"), ("cpu.feature", "avx2")]).matches(&node));

    let arm = report(&[("arch", "arm64")]).unwrap();
    assert_eq!(arm.cpu.level(), None);
    assert!(
        mac()
            .cpu
            .level()
            .is_some_and(|l| l.satisfies(IsaLevel::Arm64(ArmVersion::V8_0)))
    );

    assert_eq!(report(&[("os", "linux")]), Err(ReportError::NoArch));
    assert_eq!(
        report(&[("arch", "riscv64")]),
        Err(ReportError::UnknownArch(UnknownArch("riscv64".to_owned())))
    );
    for (entries, key) in [
        (&[("arch", "arm64"), ("arch", "arm64")][..], "arch"),
        (
            &[("arch", "arm64"), ("cpus", "1"), ("cpus", "2")][..],
            "cpus",
        ),
        (
            &[("arch", "arm64"), ("os", "linux"), ("os", "macos")][..],
            "os",
        ),
    ] {
        assert_eq!(report(entries), Err(ReportError::Repeated(key.to_owned())));
    }
    let nan = report(&[("arch", "arm64"), ("gpu", "one")]).unwrap_err();
    assert_eq!(
        nan,
        ReportError::NotANumber {
            key: "gpu".to_owned(),
            value: "one".to_owned()
        }
    );
    assert!(nan.to_string().contains("\"one\""));
    assert!(ReportError::NoArch.to_string().contains("arch"));
}

/// Catches: a report with two `xcode` entries refused as repeated (a Mac with two
/// Xcodes installed could not join), or one that keeps only one of them, so actions
/// for the other wait for a node that has it.
#[test]
fn a_report_lists_every_xcode() {
    let node = report(&[
        ("arch", "arm64"),
        ("os", "macos"),
        ("xcode", "16E140"),
        ("xcode", "16C5032a"),
        ("xcode", "16E140"),
    ])
    .unwrap();
    let xcodes: Vec<&str> = node.members["xcode"].iter().map(String::as_str).collect();
    assert_eq!(xcodes, ["16C5032a", "16E140"]);
    assert!(!node.exact.contains_key("xcode"));
    for build in ["16C5032a", "16E140"] {
        let wants = from(&[("OSFamily", "darwin"), ("xcode", build)]).unwrap();
        assert!(wants.matches(&node), "{build}");
    }
    assert!(matches!(
        from(&[("xcode", "")]),
        Err(FromPlatformError::Invalid(RequestError::BadValue { .. }))
    ));
    assert_eq!(
        from(&[("xcode", "16E140"), ("Xcode", "16C5032a")]),
        Err(FromPlatformError::Invalid(RequestError::Repeated(
            "xcode".to_owned()
        )))
    );
    let other = from(&[("xcode", "15F31d")]).unwrap();
    assert!(!other.matches(&node));
    assert!(!other.matches(&mac()), "a node that reports no Xcode");
}

/// Catches: a booking key that is not reserved, so a platform asking for a larger
/// booking is refused as an unknown capability or matched against the node; and one
/// read only in its exact spelling, so `KBF-Book-Mem-GiB=16` books the default.
#[test]
fn booking_keys_are_reserved_in_any_case() {
    for (sent, key) in [
        ("kbf-book-cpus", "kbf-book-cpus"),
        ("KBF-Book-CPUs", "kbf-book-cpus"),
        ("kbf-book-mem-gib", "kbf-book-mem-gib"),
        ("Kbf-Book-Mem-GiB", "kbf-book-mem-gib"),
    ] {
        assert_eq!(
            kbf_caps::property_name(sent).as_deref(),
            Some(key),
            "{sent}"
        );
        assert_eq!(from(&[(sent, "8")]), Ok(Request::default()), "{sent}");
    }
    assert!(kbf_caps::RESERVED_KEYS.contains(&"kbf-book-cpus"));
    assert!(kbf_caps::RESERVED_KEYS.contains(&"kbf-book-mem-gib"));
}

/// Catches: an unmet requirement shown in a form an operator cannot act on: the queue
/// reason and the FAILED_PRECONDITION message are built from these.
#[test]
fn unmet_requirements_read_as_requests() {
    let shown: Vec<String> = [
        Unmet::Arch { want: Arch::Arm64 },
        Unmet::IsaLevel {
            want: IsaLevel::X86_64(X86Level::V3),
        },
        Unmet::Feature("avx2"),
        Unmet::Exact {
            key: "os",
            want: "macos",
        },
        Unmet::Member {
            key: "xcode",
            want: "16E140",
        },
        Unmet::Consumable {
            what: Consumable::Gpus,
            want: 2,
            have: 1,
        },
    ]
    .iter()
    .map(ToString::to_string)
    .collect();
    assert_eq!(
        shown,
        [
            "arch=arm64",
            "isa_level>=x86-64-v3",
            "cpu.feature=avx2",
            "os=macos",
            "xcode=16E140",
            "gpu>=2 (has 1)"
        ]
    );
}
