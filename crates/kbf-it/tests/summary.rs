//! The M1 exit rule over build summaries (`kbf_it::summary`): the parsers for Bazel's
//! and buck2's end-of-build lines, and the rule a first build and its rebuild must meet.

use kbf_it::summary::{Spawns, SummaryError, Tool, check, check_logs, parse};

fn spawns(remote: u64, cached: u64, other: &[(&str, u64)]) -> Spawns {
    Spawns {
        remote,
        cached,
        other: other.iter().map(|(k, n)| ((*k).to_owned(), *n)).collect(),
    }
}

const BAZEL_FIRST: &str = "\
INFO: Analyzed 3 targets (5 packages loaded, 9 targets configured).
INFO: 7 processes: 4 internal, 3 remote.
INFO: Build completed successfully, 7 total actions
";

const BAZEL_SECOND: &str = "\
INFO: 7 processes: 4 internal, 3 remote cache hit.
INFO: Build completed successfully, 7 total actions
";

/// Catches: a parser that counts Bazel's internal processes as actions, confuses
/// `remote` with `remote cache hit`, or reads an earlier summary than the last.
#[test]
fn bazel_lines_are_read() {
    assert_eq!(parse(Tool::Bazel, BAZEL_FIRST), Ok(spawns(3, 0, &[])));
    assert_eq!(parse(Tool::Bazel, BAZEL_SECOND), Ok(spawns(0, 3, &[])));
    let both = format!("{BAZEL_FIRST}{BAZEL_SECOND}");
    assert_eq!(parse(Tool::Bazel, &both), Ok(spawns(0, 3, &[])));
    assert_eq!(
        parse(Tool::Bazel, "INFO: 1 process: 1 internal."),
        Ok(spawns(0, 0, &[]))
    );
    assert_eq!(
        parse(
            Tool::Bazel,
            "INFO: 6 processes: 1 internal, 2 linux-sandbox, 1 remote, 2 disk cache hit."
        ),
        Ok(spawns(1, 0, &[("disk cache hit", 2), ("linux-sandbox", 2)]))
    );
}

/// Catches: a summary that does not add up, or a line of another shape, being read as
/// counts instead of refused.
#[test]
fn bazel_lines_that_do_not_add_up_are_refused() {
    for bad in [
        "INFO: 7 processes: 4 internal, 2 remote.",
        "INFO: x processes: 1 remote.",
        "INFO: 1 processes: one remote.",
        "INFO: 1 processes: 1.",
        "INFO: +1 processes: 1 remote.",
    ] {
        assert!(
            matches!(parse(Tool::Bazel, bad), Err(SummaryError::Malformed(_))),
            "{bad}"
        );
    }
    assert!(matches!(
        parse(Tool::Bazel, "INFO: Build completed successfully"),
        Err(SummaryError::Missing(_))
    ));
}

/// Catches: a buck2 parser that swaps cached and remote, drops local commands, or
/// accepts a line that does not add up.
#[test]
fn buck2_lines_are_read() {
    let first = "Jobs completed: 9.\nCache hits: 0%. Commands: 3 (cached: 0, remote: 3, local: 0)\n\
                 BUILD SUCCEEDED\n";
    let second = "Cache hits: 100%. Commands: 3 (cached: 3, remote: 0, local: 0)\n";
    assert_eq!(parse(Tool::Buck2, first), Ok(spawns(3, 0, &[])));
    assert_eq!(parse(Tool::Buck2, second), Ok(spawns(0, 3, &[])));
    assert_eq!(
        parse(Tool::Buck2, "Commands: 3 (cached: 1, remote: 1, local: 1)"),
        Ok(spawns(1, 1, &[("local", 1)]))
    );
    // Commands that ran again elsewhere after a remote failure are not farm work.
    assert_eq!(
        parse(
            Tool::Buck2,
            "Cache hits: 0%. Commands: 2 (cached: 0, remote: 1, local: 1). Fallback: 1/2"
        ),
        Ok(spawns(1, 0, &[("fallback", 1), ("local", 1)]))
    );
    for bad in [
        "Commands: 2 (cached: 0, remote: 1, local: 1). Fallback: x/2",
        "Commands: 3 (cached: 1, remote: 1, local: 0)",
        "Commands: 3",
        "Commands: x (cached: 3)",
        "Commands: 3 (cached: 3",
        "Commands: 3 (cached 3)",
        "Commands: 3 (cached: three)",
    ] {
        assert!(
            matches!(parse(Tool::Buck2, bad), Err(SummaryError::Malformed(_))),
            "{bad}"
        );
    }
    assert!(matches!(
        parse(Tool::Buck2, "BUILD SUCCEEDED"),
        Err(SummaryError::Missing(_))
    ));
}

/// Catches: an exit rule that passes a first build with nothing remote, a stale cell,
/// local execution in either build, or a rebuild that ran anything again.
#[test]
fn the_exit_rule_holds_only_for_all_remote_then_all_hits() {
    assert_eq!(check(&spawns(3, 0, &[]), &spawns(0, 3, &[])), Ok(()));

    let broken = |first: Spawns, second: Spawns| check(&first, &second).unwrap_err();
    assert_eq!(broken(spawns(0, 0, &[]), spawns(0, 0, &[])).len(), 1);
    assert!(broken(spawns(3, 1, &[]), spawns(0, 3, &[]))[0].contains("not fresh"));
    assert!(broken(spawns(3, 0, &[("local", 1)]), spawns(0, 3, &[]))[0].contains("first"));
    assert!(broken(spawns(3, 0, &[]), spawns(0, 3, &[("local", 1)]))[0].contains("second"));
    assert!(broken(spawns(3, 0, &[]), spawns(1, 2, &[]))[0].contains("2 of the first"));
    assert!(broken(spawns(3, 0, &[]), spawns(1, 3, &[]))[0].contains("ran 1 again"));
}

/// Catches: a report that says OK when the rule failed or a log did not parse, or
/// that hides which rule broke.
#[test]
fn check_logs_reports_each_outcome() {
    let (report, passed) = check_logs(Tool::Bazel, BAZEL_FIRST, BAZEL_SECOND);
    assert!(passed, "{report}");
    assert!(report.contains("OK Bazel: 3 of 3"), "{report}");

    let (report, passed) = check_logs(Tool::Bazel, BAZEL_FIRST, BAZEL_FIRST);
    assert!(!passed);
    assert!(
        report.contains("FAIL Bazel: the second build answered 0"),
        "{report}"
    );

    let (report, passed) = check_logs(Tool::Buck2, "nothing", "nothing");
    assert!(!passed);
    assert!(
        report.starts_with("FAIL Buck2: no summary line"),
        "{report}"
    );
    let (_, passed) = check_logs(Tool::Bazel, BAZEL_FIRST, "nothing");
    assert!(!passed);

    let other = spawns(1, 0, &[("local", 2)]).to_string();
    assert_eq!(other, "remote 1, remote cache hit 0, local 2");
}
