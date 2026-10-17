//! Rule `handover` of [`super::modes`]: the frontend's unit is a template whose two
//! instances can hand over to one another (docs/design/frontend-handover.md).
//!
//! An update starts the idle instance (`kbf frontend handover`) while the other still
//! serves; the new process takes the listeners, the state store and the daemons from the
//! old one over a control socket, and the old one drains its open builds and exits. The
//! unit must have, in `[Service]` unless said:
//!
//! - `Type=notify`, and `NotifyAccess=` not `none` or `exec`: kbf sends `READY=1` from its
//!   main process once it serves. With any other type `systemctl start` returns at once, so
//!   a successor that cannot take the listeners over looks like a success, and nothing can
//!   wait for the one that can.
//! - A finite start timeout (`TimeoutSec=` sets it too, the last of the two wins): the
//!   command waits for `systemctl start`, so a successor that never says ready would hold
//!   it for ever.
//! - `--handover-socket=` an absolute path in a `RuntimeDirectory=` of the unit, with
//!   `RuntimeDirectoryPreserve=yes`: the instance that stops must not take the directory,
//!   and so the socket the next handover asks at, with it.
//! - No `Conflicts=` in `[Unit]`: starting an instance would stop the other, which is the
//!   one still draining the builds in flight, and every one of them would fail.
//!
//! What it cannot see: `Conflicts=` that another unit holds against this one, and a
//! drop-in that `systemctl revert`s the directives (it reads the unit and the drop-ins it
//! is given, as the other rules do).

use super::modes::boolean;
use crate::unit::{UnitFile, WHITESPACE};

/// The flag that names the control socket.
const SOCKET_FLAG: &str = "handover-socket";

/// Rule `handover` on `unit`: one line per way a handover between its instances breaks.
pub(super) fn check(unit: &UnitFile) -> Vec<String> {
    let mut out = Vec::new();
    match unit.get_in("Service", "Type").map(trim) {
        Some("notify") => {}
        other => out.push(format!(
            "has {}, not Type=notify: `systemctl start` of the new instance would return \
             before it serves, so a handover that failed would look like one that worked",
            other.map_or_else(|| "no Type=".to_owned(), |t| format!("Type={t}")),
        )),
    }
    if let Some(access @ ("none" | "exec")) = unit.get_in("Service", "NotifyAccess").map(trim) {
        out.push(format!(
            "has NotifyAccess={access}: systemd drops the READY=1 kbf sends from its main \
             process, and the start never returns"
        ));
    }
    let start = unit
        .directives()
        .iter()
        .rev()
        .find(|d| {
            d.section == "Service" && ["TimeoutSec", "TimeoutStartSec"].contains(&d.key.as_str())
        })
        .map(|d| (d.key.as_str(), trim(&d.value)));
    if let Some((key, value @ "infinity")) = start {
        out.push(format!(
            "has {key}={value}: a new instance that never says ready would hold the \
             handover for ever; give a finite TimeoutStartSec="
        ));
    }
    match unit.flag(SOCKET_FLAG) {
        None => out.push(
            "gives no --handover-socket: the instances cannot hand over, and an update would \
             drain the serving one and refuse every new build until the other starts"
                .to_owned(),
        ),
        Some(socket) => {
            let dirs = runtime_directories(unit);
            let inside = socket
                .strip_prefix("/run/")
                .and_then(|rest| rest.split_once('/'))
                .is_some_and(|(dir, file)| !file.is_empty() && dirs.contains(&dir));
            if !inside {
                out.push(format!(
                    "has --handover-socket={socket} outside its RuntimeDirectory= ({}): \
                     /run/<directory>/<file> with the directory named there, so systemd makes \
                     it for the unit's user",
                    if dirs.is_empty() {
                        "none given".to_owned()
                    } else {
                        dirs.join(" ")
                    }
                ));
            }
        }
    }
    let preserve = unit.get_in("Service", "RuntimeDirectoryPreserve").map(trim);
    if preserve.and_then(boolean) != Some(true) {
        out.push(format!(
            "has {}, not RuntimeDirectoryPreserve=yes: the instance that stops would take \
             the control socket's directory with it",
            preserve.map_or_else(
                || "no RuntimeDirectoryPreserve=".to_owned(),
                |p| format!("RuntimeDirectoryPreserve={p}")
            )
        ));
    }
    if let Some(conflicts) = unit
        .directives()
        .iter()
        .find(|d| d.section == "Unit" && d.key == "Conflicts" && !trim(&d.value).is_empty())
    {
        out.push(format!(
            "has Conflicts={}: starting an instance would stop the other, which is draining \
             the builds in flight",
            trim(&conflicts.value)
        ));
    }
    out
}

/// The directories the `RuntimeDirectory=` lines name, in order (a list per line; an
/// empty value starts over, as systemd's list directives do).
fn runtime_directories(unit: &UnitFile) -> Vec<&str> {
    let mut dirs = Vec::new();
    for d in unit
        .directives()
        .iter()
        .filter(|d| d.section == "Service" && d.key == "RuntimeDirectory")
    {
        let value = trim(&d.value);
        if value.is_empty() {
            dirs.clear();
        }
        dirs.extend(value.split(WHITESPACE).filter(|w| !w.is_empty()));
    }
    dirs
}

fn trim(value: &str) -> &str {
    value.trim_matches(WHITESPACE)
}

#[cfg(test)]
#[path = "handover_tests.rs"]
mod tests;
