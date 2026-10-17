//! Rule `stop` of [`super::modes`]: a mode that drains on SIGTERM (`frontend`,
//! `all-in-one`, which runs a frontend, and `storage` and `storage-and-daemon`, whose
//! storage half finishes the Writes in flight) is stopped with SIGTERM and never killed
//! for taking long.
//!
//! On SIGTERM kbf's frontend refuses new Execute calls and keeps every open Execute and
//! WaitExecution stream until its action is done (`--shutdown-timeout-secs`, 0 by
//! default: no deadline). Buck2 never recovers a stream that breaks mid-action, so a
//! stop that ends before the drain fails every build in flight. A storage node drains
//! its large Writes at stop the same way, and a Write cut off fails the upload. systemd sends
//! `KillSignal=` and, after `TimeoutStopSec=` (`DefaultTimeoutStopSec=`, 90 s, when the
//! unit gives none), SIGKILL. So the unit's `[Service]` must have:
//!
//! - `TimeoutStopSec=infinity`, read as systemd reads it: `TimeoutSec=` sets the stop
//!   timeout too (`config_parse_service_timeout`), so the last of the two wins. Any other
//!   value fails, `0` among them: systemd takes 0 for no timeout as well
//!   (`parse_sec_fix_0`), but the rule asks for the word, which says so.
//! - `KillSignal=SIGTERM` (or `TERM` or `15`, as `signal_from_string` reads it), given:
//!   kbf drains on it, and the rule does not trust the default to stay SIGTERM.
//!
//! Not for `daemon`: a daemon does not drain on SIGTERM (its leases are abandoned; the
//! operator API's `POST /v1/nodes/{node}:drain` empties a node before it is stopped), so
//! an infinite stop would only hold a reboot on a daemon that hangs.
//!
//! What it cannot see: a value systemd ignores because it cannot parse it (a later
//! `TimeoutStopSec=` it ignores keeps the earlier one, and the rule fails the unit: a
//! false red), and `DefaultTimeoutStopSec=` in `system.conf` (never read: the unit sets
//! its own).

use crate::unit::{UnitFile, WHITESPACE};

/// The `KillSignal=` values that are SIGTERM.
const SIGTERM: [&str; 3] = ["SIGTERM", "TERM", "15"];

/// Rule `stop` on `unit`: one line per way its stop cuts a drain short.
pub(super) fn check(unit: &UnitFile) -> Vec<String> {
    let mut out = Vec::new();
    let timeout = unit
        .directives()
        .iter()
        .rev()
        .find(|d| {
            d.section == "Service" && ["TimeoutSec", "TimeoutStopSec"].contains(&d.key.as_str())
        })
        .map(|d| (d.key.as_str(), d.value.trim_matches(WHITESPACE)));
    match timeout {
        Some((_, "infinity")) => {}
        Some((key, value)) => out.push(format!(
            "has {key}={value}, not TimeoutStopSec=infinity: systemd kills kbf when it runs \
             out, before the drain ends, and every build in flight fails"
        )),
        None => out.push(
            "gives no TimeoutStopSec=infinity: systemd kills kbf after its default 90 s, \
             before the drain ends, and every build in flight fails"
                .to_owned(),
        ),
    }
    let signal = unit
        .get_in("Service", "KillSignal")
        .map(|s| s.trim_matches(WHITESPACE));
    match signal {
        Some(s) if SIGTERM.contains(&s) => {}
        Some(s) => out.push(format!(
            "has KillSignal={s}, not KillSignal=SIGTERM: kbf drains on SIGTERM"
        )),
        None => out.push(
            "gives no KillSignal=SIGTERM: kbf drains on SIGTERM, and the rule does not \
             trust the default to stay so"
                .to_owned(),
        ),
    }
    out
}

#[cfg(test)]
#[path = "stop_tests.rs"]
mod tests;
