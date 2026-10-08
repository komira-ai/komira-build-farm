//! Requests against the node classes' fixture capabilities.

use kbf_caps::{
    Arch, Consumable, CpuCaps, IsaLevel, NodeCaps, Request, RequestError, Unmet, X86Level,
};

fn node(cpu: &str) -> NodeCaps {
    NodeCaps::new(CpuCaps::from_linux_cpuinfo(cpu).unwrap())
}

fn broadwell() -> NodeCaps {
    node(include_str!("fixtures/broadwell_e5_2699_v4.cpuinfo"))
}

fn skylake() -> NodeCaps {
    node(include_str!("fixtures/skylake_sp_platinum_8180.cpuinfo"))
}

fn ampere() -> NodeCaps {
    node(include_str!("fixtures/ampere_altra_max_m128_30.cpuinfo"))
}

/// A Mac with the Xcode builds in `xcodes` installed.
fn mac(xcodes: &[&str]) -> NodeCaps {
    let cpu = CpuCaps::from_macos_sysctl(include_str!("fixtures/apple_m3_ultra.sysctl")).unwrap();
    let mut n = NodeCaps::new(cpu);
    for xcode in xcodes {
        n.members
            .entry("xcode".to_owned())
            .or_default()
            .insert((*xcode).to_owned());
    }
    n.exact.insert("os".to_owned(), "macos".to_owned());
    n
}

fn req(props: &[(&str, &str)]) -> Request {
    Request::parse(props.iter().copied()).unwrap()
}

/// Catches: `isa_level` compared exactly. Skylake (v4) must serve a v3 request, or
/// it idles while Broadwells queue v3 work.
#[test]
fn skylake_serves_v3() {
    let v3 = req(&[("isa_level", "x86-64-v3")]);
    assert!(v3.matches(&skylake()));
    assert!(v3.matches(&broadwell()));
}

/// Catches: an at-least test written backwards, or one that ignores the level, so a
/// Broadwell runs AVX-512 code.
#[test]
fn broadwell_does_not_serve_v4() {
    let v4 = req(&[("isa_level", "x86-64-v4")]);
    assert!(v4.matches(&skylake()));
    assert_eq!(
        v4.unmet(&broadwell()),
        [Unmet::IsaLevel {
            want: IsaLevel::X86_64(X86Level::V4)
        }]
    );
}

/// Catches: an x86-64 level satisfied by an arm64 node's version, and an arm64
/// request served past the node's version.
#[test]
fn levels_do_not_cross_families() {
    assert!(!req(&[("isa_level", "x86-64-v1")]).matches(&ampere()));
    assert!(!req(&[("isa_level", "armv8.0-a")]).matches(&skylake()));
    assert!(req(&[("isa_level", "armv8.2-a")]).matches(&ampere()));
    assert!(!req(&[("isa_level", "armv8.3-a")]).matches(&ampere()));
    assert!(req(&[("isa_level", "armv8.4-a")]).matches(&mac(&["x"])));
}

/// Catches: features matched as "any of" instead of "all of". The Ampere has `aes`
/// but not `sve`, so a request for both must fail and name only `sve`.
#[test]
fn features_are_a_subset() {
    let r = req(&[("cpu.feature", "aes"), ("cpu.feature", "sve")]);
    assert_eq!(r.unmet(&ampere()), [Unmet::Feature("sve")]);
    assert!(req(&[("cpu.feature", "aes"), ("cpu.feature", "asimddp")]).matches(&ampere()));
    assert!(req(&[("cpu.feature", "aes"), ("cpu.feature", "asimddp")]).matches(&mac(&["x"])));
}

/// Catches: a consumable compared exactly or backwards, and a missing capacity
/// treated as unlimited.
#[test]
fn consumables_are_minimums() {
    let mut n = broadwell();
    n.consumables.insert(Consumable::Cpus, 44);
    assert!(req(&[("cpus", "8")]).matches(&n));
    assert!(req(&[("cpus", "44")]).matches(&n));
    assert_eq!(
        req(&[("cpus", "45")]).unmet(&n),
        [Unmet::Consumable {
            what: Consumable::Cpus,
            want: 45,
            have: 44
        }]
    );
    assert!(!req(&[("nvme_gib", "1")]).matches(&n));
}

/// Catches: `xcode` matched as a minimum (a Mac mid-upgrade to a newer build takes an
/// action built for the old one, and the cache mixes compilers), or exactly against
/// one reported value (a Mac with two Xcodes then serves at most one of them).
#[test]
fn xcode_is_matched_by_membership() {
    let old = req(&[("xcode", "16C5032a"), ("os", "macos")]);
    let new = req(&[("xcode", "16E140")]);
    let both = mac(&["16C5032a", "16E140"]);
    assert!(old.matches(&both), "the first of two Xcodes");
    assert!(new.matches(&both), "the second of two Xcodes");
    assert!(old.matches(&mac(&["16C5032a"])));
    assert_eq!(
        old.unmet(&mac(&["16E140"])),
        [Unmet::Member {
            key: "xcode",
            want: "16C5032a"
        }]
    );
    assert_eq!(
        new.unmet(&mac(&[])),
        [Unmet::Member {
            key: "xcode",
            want: "16E140"
        }],
        "a node without Xcode"
    );
    assert_eq!(
        Unmet::Member {
            key: "xcode",
            want: "16E140"
        }
        .to_string(),
        "xcode=16E140"
    );
}

/// Catches: a request naming two Xcodes (it can run under one `DEVELOPER_DIR`) or an
/// empty one accepted, which no node could serve as meant.
#[test]
fn a_request_names_one_xcode() {
    let err = |props: &[(&str, &str)]| Request::parse(props.iter().copied()).unwrap_err();
    assert_eq!(
        err(&[("xcode", "16E140"), ("xcode", "16C5032a")]),
        RequestError::Repeated("xcode".to_owned())
    );
    assert_eq!(
        err(&[("xcode", "")]),
        RequestError::BadValue {
            key: "xcode".to_owned(),
            value: String::new()
        }
    );
}

/// Catches: an arch mismatch ignored, and labels compared loosely.
#[test]
fn arch_and_labels_are_exact() {
    let mut n = ampere();
    n.exact.insert("label.rack".to_owned(), "r1".to_owned());
    assert!(req(&[("arch", "arm64"), ("label.rack", "r1")]).matches(&n));
    assert_eq!(
        req(&[("arch", "x86_64")]).unmet(&n),
        [Unmet::Arch { want: Arch::X86_64 }]
    );
    assert!(!req(&[("label.rack", "r2")]).matches(&n));
}

/// Catches: an unknown key or bad value silently ignored (the action would run
/// anywhere), a single-valued key accepted twice, and the reserved lease and booking
/// keys rejected as unknown.
#[test]
fn parse_rejects_what_it_cannot_compare() {
    let err = |props: &[(&str, &str)]| Request::parse(props.iter().copied()).unwrap_err();
    assert_eq!(
        err(&[("isa", "x86-64-v3")]),
        RequestError::UnknownKey("isa".to_owned())
    );
    assert_eq!(
        err(&[("label.", "x")]),
        RequestError::UnknownKey("label.".to_owned())
    );
    for (k, v) in [
        ("isa_level", "x86-64-v9"),
        ("arch", "riscv64"),
        ("cpus", "-1"),
        ("cpu.feature", ""),
        ("cpu.feature", "aes sve"),
    ] {
        assert_eq!(
            err(&[(k, v)]),
            RequestError::BadValue {
                key: k.to_owned(),
                value: v.to_owned()
            }
        );
    }
    for k in ["isa_level", "arch", "cpus", "xcode"] {
        let v = match k {
            "isa_level" => "x86-64-v3",
            "arch" => "arm64",
            "cpus" => "1",
            _ => "b",
        };
        assert_eq!(err(&[(k, v), (k, v)]), RequestError::Repeated(k.to_owned()));
    }
    assert_eq!(
        req(&[
            ("kbf-lease", "action"),
            ("kbf-cpu", "dedicated"),
            ("kbf-book-cpus", "8"),
            ("kbf-book-mem-gib", "16"),
        ]),
        Request::default()
    );
}
