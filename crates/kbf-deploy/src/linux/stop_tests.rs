//! Rule `stop` read on its own: which stop timeout and kill signal systemd uses.

use super::*;

fn unit(text: &str) -> UnitFile {
    UnitFile::parse(text).expect("a unit")
}

const OK: &str = "[Service]\nTimeoutStopSec=infinity\nKillSignal=SIGTERM\n";

/// Catches a unit with no stop timeout passed (systemd's 90 s cuts the drain), a finite
/// one passed (`1y` and `0` too: the rule wants the word), `TimeoutSec=` not read as the
/// stop timeout or read where a later `TimeoutStopSec=` wins (and the other way), a key
/// outside `[Service]` taken for it, and a value not trimmed as systemd trims it.
#[test]
fn the_stop_timeout_is_infinity_by_the_last_of_timeout_sec_and_timeout_stop_sec() {
    assert_eq!(check(&unit(OK)), Vec::<String>::new());
    let finite = |key: &str, value: &str| {
        format!(
            "has {key}={value}, not TimeoutStopSec=infinity: systemd kills kbf when it runs out, before the drain ends, and every build in flight fails"
        )
    };
    let none = "gives no TimeoutStopSec=infinity: systemd kills kbf after its default 90 s, before the drain ends, and every build in flight fails";
    let rows: [(&str, Vec<String>); 8] = [
        ("KillSignal=SIGTERM\n", vec![none.to_owned()]),
        (
            "TimeoutStopSec=90\nKillSignal=SIGTERM\n",
            vec![finite("TimeoutStopSec", "90")],
        ),
        (
            "TimeoutStopSec=1y\nKillSignal=SIGTERM\n",
            vec![finite("TimeoutStopSec", "1y")],
        ),
        (
            "TimeoutStopSec=0\nKillSignal=SIGTERM\n",
            vec![finite("TimeoutStopSec", "0")],
        ),
        (
            "TimeoutStopSec=infinity\nTimeoutSec=5min\nKillSignal=SIGTERM\n",
            vec![finite("TimeoutSec", "5min")],
        ),
        (
            "TimeoutSec=5min\nTimeoutStopSec=infinity\nKillSignal=SIGTERM\n",
            Vec::new(),
        ),
        (
            "TimeoutSec=infinity\nTimeoutStartSec=5\nKillSignal=SIGTERM\n",
            Vec::new(),
        ),
        (
            "TimeoutStopSec=  infinity \nKillSignal=SIGTERM\n",
            Vec::new(),
        ),
    ];
    for (service, want) in rows {
        assert_eq!(
            check(&unit(&format!("[Service]\n{service}"))),
            want,
            "{service}"
        );
    }
    let elsewhere = unit("[Unit]\nTimeoutStopSec=infinity\n[Service]\nKillSignal=SIGTERM\n");
    assert_eq!(check(&elsewhere), [none]);
}

/// Catches a unit with no `KillSignal=` passed, another signal passed (SIGINT, which kbf
/// also drains on, is not what the templates give; SIGKILL drains nothing), a spelling
/// of SIGTERM systemd takes refused, and an earlier `KillSignal=` read where the last
/// wins (a drop-in's).
#[test]
fn the_kill_signal_is_sigterm_given() {
    let base = "[Service]\nTimeoutStopSec=infinity\n";
    for ok in ["SIGTERM", "TERM", "15", " SIGTERM "] {
        assert_eq!(
            check(&unit(&format!("{base}KillSignal={ok}\n"))),
            Vec::<String>::new(),
            "{ok}"
        );
    }
    assert_eq!(
        check(&unit(base)),
        [
            "gives no KillSignal=SIGTERM: kbf drains on SIGTERM, and the rule does not trust the default to stay so"
        ]
    );
    for bad in ["SIGKILL", "SIGINT", "9"] {
        assert_eq!(
            check(&unit(&format!("{base}KillSignal={bad}\n"))),
            [format!(
                "has KillSignal={bad}, not KillSignal=SIGTERM: kbf drains on SIGTERM"
            )],
            "{bad}"
        );
    }
    let mut overridden = unit(OK);
    overridden.extend(unit("[Service]\nKillSignal=SIGKILL\n"));
    assert_eq!(check(&overridden).len(), 1);
}
