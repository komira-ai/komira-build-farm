//! The contract with `kbf-host-install`, tested against its real types: every answer it can
//! print is serialized by its own code and read by the adapter, and every request the adapter
//! builds is parsed by its own parser. A change on either side that the other does not follow
//! turns one of these red.

use kbf_host_install::contract::{
    Failure, FailureCode, Outcome, QueryReport, Refusal, install_request, is_query,
};

use super::answer::{install_answer, query_answer};
use super::*;
use crate::ports::{Apply, InstallAnswer, Installed};

/// A version for the tests that do not care which.
fn v1() -> Version {
    Version::parse("1").expect("version")
}

/// Every refusal. The `match` is the guard: a new variant is a compile error here until the
/// list below (and so the tests) name it.
fn refusals() -> Vec<Refusal> {
    let all = vec![
        Refusal::BadCommand,
        Refusal::ChannelNotAllowed,
        Refusal::ConfigInvalid,
        Refusal::ConfigUnsafe,
        Refusal::ChannelUnsigned,
        Refusal::KeyUntrusted,
        Refusal::ChannelInsecure,
        Refusal::UnknownRelease,
        Refusal::AmbiguousRelease,
        Refusal::FetchFailed,
    ];
    for r in &all {
        match r {
            Refusal::BadCommand
            | Refusal::ChannelNotAllowed
            | Refusal::ConfigInvalid
            | Refusal::ConfigUnsafe
            | Refusal::ChannelUnsigned
            | Refusal::KeyUntrusted
            | Refusal::ChannelInsecure
            | Refusal::UnknownRelease
            | Refusal::AmbiguousRelease
            | Refusal::FetchFailed => {}
        }
    }
    all
}

/// Every failure code, guarded the same way.
fn failures() -> Vec<FailureCode> {
    let all = vec![
        FailureCode::AuditUnavailable,
        FailureCode::QueryFailed,
        FailureCode::InstalledVersionInvalid,
        FailureCode::PackageStateUnclean,
        FailureCode::UpdateFailed,
        FailureCode::InstallFailed,
        FailureCode::VerifyFailed,
        FailureCode::Busy,
        FailureCode::LockUnavailable,
        FailureCode::RestartFailed,
        FailureCode::RestartTimeout,
    ];
    for f in &all {
        match f {
            FailureCode::AuditUnavailable
            | FailureCode::QueryFailed
            | FailureCode::InstalledVersionInvalid
            | FailureCode::PackageStateUnclean
            | FailureCode::UpdateFailed
            | FailureCode::InstallFailed
            | FailureCode::VerifyFailed
            | FailureCode::Busy
            | FailureCode::LockUnavailable
            | FailureCode::RestartFailed
            | FailureCode::RestartTimeout => {}
        }
    }
    all
}

/// The exit status and the stdout bytes of an outcome, as the host's own `handle` writes them.
fn printed(outcome: &Outcome) -> (i32, Vec<u8>) {
    (
        i32::from(outcome.exit_code()),
        format!("{}\n", outcome.json_line()).into_bytes(),
    )
}

/// Catches a drift of the install answers: both successes are Ok, and every refusal and
/// every failure code the host can print reads as `refused: <code>` / `failed: <code>`
/// and as no other kind of error.
#[test]
fn every_install_outcome_of_the_host_is_read() {
    for version in ["1.2.3-1", "20261021.1+git9ba38c2-2"] {
        for outcome in [
            Outcome::Installed {
                version: version.into(),
            },
            Outcome::AlreadyInstalled {
                version: version.into(),
            },
        ] {
            let (code, out) = printed(&outcome);
            let want = match outcome {
                Outcome::Installed { .. } => InstallAnswer::Installed,
                _ => InstallAnswer::AlreadyInstalled,
            };
            assert_eq!(
                install_answer(code, &out, &Version::parse(version).expect("version")),
                Ok(want),
                "{outcome:?}"
            );
        }
    }
    for refusal in refusals() {
        let (code, out) = printed(&Outcome::Refused(refusal));
        let got = install_answer(code, &out, &v1()).expect_err("refused");
        assert_eq!(
            got,
            PortError::other(format!("refused: {}", refusal.code()))
        );
    }
    for failure in failures() {
        let outcome = Outcome::Failed(Failure {
            code: failure,
            detail: "raw apt words".into(),
        });
        let (code, out) = printed(&outcome);
        let got = install_answer(code, &out, &v1()).expect_err("failed");
        assert_eq!(got, PortError::other(format!("failed: {}", failure.code())));
        assert!(!got.to_string().contains("raw apt"));
    }
}

/// Catches a drift of the two query fields the roll acts on: the node id and the running
/// commit are read as the host's own types print them, and an absent one is `None` (an old
/// host that prints neither is still read).
#[test]
fn the_node_id_and_the_running_commit_of_the_host_are_read() {
    let report = |c: Option<&str>, id: Option<&str>| {
        let (code, out) = printed(&Outcome::Queried(QueryReport {
            installed_version: Some("1.2.3-1".to_owned()),
            running_commit: c.map(str::to_owned),
            node_id: id.map(str::to_owned),
            in_progress: false,
        }));
        query_answer(code, &out).expect("read")
    };
    let both = report(Some("0123456789ab"), Some("node-1"));
    assert_eq!(both.running_commit.as_deref(), Some("0123456789ab"));
    assert_eq!(both.node_id.as_deref(), Some("node-1"));
    let neither = report(None, None);
    assert_eq!((neither.running_commit, neither.node_id), (None, None));
    let old = br#"{"result":"query","installed_version":"1","in_progress":false}"#;
    assert_eq!(query_answer(0, old).expect("old host").node_id, None);
}

/// Catches a drift of the query report: the field names and types, `null` for nothing
/// installed (the answer `NotInstalled`, not an error), and in-progress taking precedence.
#[test]
fn every_query_outcome_of_the_host_is_read() {
    let report = |v: Option<&str>, c: Option<&str>, p: bool| {
        Outcome::Queried(QueryReport {
            installed_version: v.map(str::to_owned),
            running_commit: c.map(str::to_owned),
            node_id: Some("node-1".to_owned()),
            in_progress: p,
        })
    };
    let read = |o: &Outcome| {
        let (code, out) = printed(o);
        query_answer(code, &out).map(|r| r.installed)
    };
    assert_eq!(
        read(&report(Some("1.2.3-1"), Some("abc"), false)),
        Ok(Installed::Version("1.2.3-1".into()))
    );
    assert_eq!(
        read(&report(Some("1.2.3-1"), None, false)),
        Ok(Installed::Version("1.2.3-1".into()))
    );
    assert_eq!(
        read(&report(Some("1.2.3-1"), Some("abc"), true)),
        Ok(Installed::InProgress)
    );
    assert_eq!(read(&report(None, None, true)), Ok(Installed::InProgress));
    assert_eq!(
        read(&report(None, None, false)),
        Ok(Installed::NotInstalled)
    );
    // A query the host refuses or fails is read like an install's.
    for refusal in refusals() {
        let got = read(&Outcome::Refused(refusal)).expect_err("refused");
        assert_eq!(got.to_string(), format!("refused: {}", refusal.code()));
    }
    for failure in failures() {
        let got = read(&Outcome::Failed(Failure {
            code: failure,
            detail: String::new(),
        }))
        .expect_err("failed");
        assert_eq!(got.to_string(), format!("failed: {}", failure.code()));
    }
}

/// Catches an answer read as the wrong verb's: a query report is no install answer and an
/// install answer is no query report.
#[test]
fn an_answer_of_the_other_verb_is_not_read() {
    let (code, out) = printed(&Outcome::Installed {
        version: "1".into(),
    });
    assert_eq!(
        query_answer(code, &out),
        Err(PortError::other("bad answer"))
    );
    let q = Outcome::Queried(QueryReport {
        installed_version: Some("1".into()),
        running_commit: None,
        node_id: None,
        in_progress: false,
    });
    let (code, out) = printed(&q);
    assert_eq!(
        install_answer(code, &out, &v1()),
        Err(PortError::other("bad answer"))
    );
}

/// Catches the adapter's requests drifting from the host's parser: the install command
/// and the query word are accepted, and the host reads back the version and channel the
/// adapter sent.
#[test]
fn the_requests_built_here_are_the_ones_the_host_parses() {
    let r = SshInstaller::new(
        PathBuf::from("/k"),
        "u",
        "nightly-2",
        Duration::from_secs(1),
        Duration::from_secs(1),
    )
    .expect("installer");
    for v in crate::version::ACCEPTED {
        let version = Version::parse(v).expect("version");
        let command = r.install_command(&version);
        assert_eq!(
            install_request(&command),
            Some(((*v).to_owned(), "nightly-2".to_owned())),
            "{command}"
        );
    }
    assert!(is_query("query"));
}

/// Catches the version rule here and the host's disagreeing in either direction, over the
/// shared corpus and the edges the corpus lacks (a trailing hyphen, the byte and unit-name
/// bounds). A rule looser here means a request the host refuses; stricter means a version
/// that cannot be rolled.
#[test]
fn the_version_rule_agrees_with_the_host() {
    let long_plus = |n: usize| format!("1{}", "+".repeat(n));
    let mut cases: Vec<String> = crate::version::ACCEPTED
        .iter()
        .chain(crate::version::REJECTED)
        .map(|s| (*s).to_owned())
        .collect();
    cases.extend(
        [
            "1.0-",
            "1-",
            "1--",
            "1.0-1-",
            "0",
            "1~",
            "~1",
            "1.0+",
            "1.0~",
            "9.9.9-9~9+9",
        ]
        .map(str::to_owned),
    );
    cases.extend([
        long_plus(113),
        long_plus(114),
        long_plus(115),
        long_plus(127),
        long_plus(128),
        format!("1{}", "0".repeat(127)),
        format!("1{}", "0".repeat(128)),
    ]);
    for s in cases {
        let host_says = install_request(&format!("install kbf={s} from stable")).is_some();
        assert_eq!(host_accepts(&s), host_says, "{s:?}");
        // The type may be looser than the host (it is U1's) but never stricter.
        if host_says {
            assert!(Version::parse(&s).is_ok(), "{s:?}");
        }
    }
}

/// Catches the channel rule here and the host's disagreeing.
#[test]
fn the_channel_rule_agrees_with_the_host() {
    let version = "1";
    let mut cases: Vec<String> = [
        "",
        "stable",
        "Stable",
        "-x",
        "x-",
        "a--b",
        "0",
        "a b",
        "a/b",
        "a_b",
        "a.b",
        "a\nb",
        "a\0",
        "\u{ff41}",
        "nightly-2026",
    ]
    .map(str::to_owned)
    .to_vec();
    cases.extend(["a".repeat(31), "a".repeat(32), "a".repeat(33)]);
    for c in cases {
        let host_says = install_request(&format!("install kbf={version} from {c}")).is_some();
        assert_eq!(Channel::parse(&c).is_ok(), host_says, "{c:?}");
    }
}

/// Catches a drift of the Mac answers: the host's own `apply_started` is read as that answer
/// for the version it names and for no other, and its Mac query line (the report and the
/// `apply` word, in every state the host writes) is read with the report intact.
#[test]
fn the_mac_answers_of_the_host_are_read() {
    let started = Outcome::ApplyStarted {
        version: "20261021.1+git9ba38c2-2".into(),
        commit: "0123456789ab".into(),
    };
    let (code, out) = printed(&started);
    let asked = Version::parse("20261021.1+git9ba38c2-2").expect("version");
    assert_eq!(
        install_answer(code, &out, &asked),
        Ok(InstallAnswer::ApplyStarted)
    );
    assert_eq!(
        install_answer(code, &out, &v1()),
        Err(PortError::other("bad answer"))
    );
    let apply_states = [
        ("running", Some(Apply::Running)),
        ("done", Some(Apply::Done)),
        (
            "failed:health_timeout:rolled_back",
            Some(Apply::Failed("health_timeout:rolled_back".to_owned())),
        ),
        (
            "failed:apply_lost",
            Some(Apply::Failed("apply_lost".to_owned())),
        ),
    ];
    for (line, want) in apply_states {
        let outcome = Outcome::MacQueried {
            report: QueryReport {
                installed_version: Some("1.2.3-1".to_owned()),
                running_commit: Some("0123456789ab".to_owned()),
                node_id: Some("mac-1".to_owned()),
                in_progress: false,
            },
            apply: Some(line.to_owned()),
        };
        let (code, out) = printed(&outcome);
        let got = query_answer(code, &out).expect(line);
        assert_eq!(got.apply, want, "{line}");
        assert_eq!(got.installed, Installed::Version("1.2.3-1".into()));
        assert_eq!(got.running_commit.as_deref(), Some("0123456789ab"));
        assert_eq!(got.node_id.as_deref(), Some("mac-1"));
    }
    let none = Outcome::MacQueried {
        report: QueryReport {
            installed_version: None,
            running_commit: None,
            node_id: Some("mac-1".to_owned()),
            in_progress: false,
        },
        apply: None,
    };
    let (code, out) = printed(&none);
    let got = query_answer(code, &out).expect("no apply yet");
    assert_eq!((got.apply, got.installed), (None, Installed::NotInstalled));
}
