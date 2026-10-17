use super::*;

/// Catches a host name that is not one: each rule of the name.
#[test]
fn host_names_are_labels() {
    assert_eq!(HostName::new("node-01").expect("valid").as_str(), "node-01");
    assert_eq!(HostName::new("a").expect("valid").to_string(), "a");
    assert!(HostName::new(&"a".repeat(63)).is_ok());
    for bad in ["", "1a", "A", "a_b", "a.b", "-a", &"a".repeat(64)] {
        assert!(HostName::new(bad).is_err(), "{bad:?}");
    }
    assert_eq!(
        HostName::new("A").expect_err("refused").to_string(),
        "host name \"A\" is not valid: 1 to 63 characters of a-z, 0-9 and -, starting with a letter"
    );
}

/// Catches a commit that is a branch name, an abbreviation or upper-case.
#[test]
fn commits_are_forty_lower_case_hex_digits() {
    let sha = "0123456789abcdef0123456789abcdef01234567";
    assert_eq!(Commit::new(sha).expect("valid").as_str(), sha);
    assert_eq!(Commit::new(sha).expect("valid").to_string(), sha);
    for bad in [
        "main",
        &sha[..39],
        &format!("{sha}0"),
        &sha.to_uppercase(),
        &format!("g{}", &sha[1..]),
    ] {
        assert!(Commit::new(bad).is_err(), "{bad:?}");
    }
}

/// Catches a run id that could not be a lock owner's name.
#[test]
fn run_ids_are_short_plain_words() {
    assert_eq!(
        RunId::new("123456.7_a-B").expect("valid").as_str(),
        "123456.7_a-B"
    );
    assert_eq!(RunId::new("r1").expect("valid").to_string(), "r1");
    assert!(RunId::new(&"r".repeat(64)).is_ok());
    for bad in ["", "a b", "a/b", &"r".repeat(65)] {
        assert!(RunId::new(bad).is_err(), "{bad:?}");
    }
}

/// Catches a health reading that passes with a part missing, and a problem line that
/// names the wrong part.
#[test]
fn health_needs_all_three_and_names_what_is_missing() {
    let all = Health {
        unit_active: true,
        ready: true,
        registered_active: true,
    };
    assert!(all.is_healthy());
    assert_eq!(all.problem(), "");
    let down = Health {
        unit_active: false,
        ..all
    };
    assert!(!down.is_healthy());
    assert_eq!(down.problem(), "unit not active");
    let unready = Health {
        ready: false,
        ..all
    };
    assert!(!unready.is_healthy());
    assert_eq!(unready.problem(), "not ready");
    let unregistered = Health {
        registered_active: false,
        ..all
    };
    assert!(!unregistered.is_healthy());
    assert_eq!(
        unregistered.problem(),
        "not registered active at the commit"
    );
}
