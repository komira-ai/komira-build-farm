//! Rule `handover` read on its own: the directives that let two instances of the
//! frontend's template hand over.

use super::*;

const OK: &str = "[Unit]\nDescription=x\n[Service]\nType=notify\nNotifyAccess=main\n\
TimeoutStartSec=120\nRuntimeDirectory=kbf-frontend\nRuntimeDirectoryPreserve=yes\n\
ExecStart=/usr/local/bin/kbf frontend --handover-socket=/run/kbf-frontend/control.sock\n";

fn unit(text: &str) -> UnitFile {
    UnitFile::parse(text).expect("a unit")
}

fn without(line: &str) -> String {
    let planted = OK.replace(line, "");
    assert_ne!(planted, OK, "the defect is not planted: {line}");
    planted
}

fn with(from: &str, to: &str) -> String {
    let planted = OK.replace(from, to);
    assert_ne!(planted, OK, "the defect is not planted: {from}");
    planted
}

/// Catches a template that ships a defect of the rule, and a rule that passes the shipped
/// shape (a false red would fail every frontend unit).
#[test]
fn the_unit_the_template_makes_passes() {
    assert_eq!(check(&unit(OK)), Vec::<String>::new());
}

/// Catches a unit that is not `Type=notify` (`systemctl start` returns before the new
/// frontend serves: a handover that failed looks like one that worked), one whose
/// `NotifyAccess=` drops kbf's READY=1, and a start with no deadline. The last value of a
/// key wins, as systemd reads it, and `TimeoutSec=` sets the start timeout too.
#[test]
fn the_start_waits_for_ready_and_gives_up() {
    let type_simple = "has Type=simple, not Type=notify: `systemctl start` of the new instance \
        would return before it serves, so a handover that failed would look like one that worked";
    let no_type = type_simple.replace("Type=simple", "no Type=");
    let rows: [(String, Vec<String>); 8] = [
        (
            with("Type=notify", "Type=simple"),
            vec![type_simple.to_owned()],
        ),
        (without("Type=notify\n"), vec![no_type]),
        (with("Type=notify", "Type=simple\nType=notify"), Vec::new()),
        (
            with("NotifyAccess=main", "NotifyAccess=none"),
            vec![
                "has NotifyAccess=none: systemd drops the READY=1 kbf sends from its main \
                  process, and the start never returns"
                    .to_owned(),
            ],
        ),
        (
            with("NotifyAccess=main", "NotifyAccess=exec"),
            vec![
                "has NotifyAccess=exec: systemd drops the READY=1 kbf sends from its main \
                  process, and the start never returns"
                    .to_owned(),
            ],
        ),
        (with("NotifyAccess=main", "NotifyAccess=all"), Vec::new()),
        (
            with("TimeoutStartSec=120", "TimeoutSec=infinity"),
            vec![
                "has TimeoutSec=infinity: a new instance that never says ready would hold the \
                  handover for ever; give a finite TimeoutStartSec="
                    .to_owned(),
            ],
        ),
        (
            with(
                "TimeoutStartSec=120",
                "TimeoutStartSec=infinity\nTimeoutSec=90",
            ),
            Vec::new(),
        ),
    ];
    for (text, want) in rows {
        assert_eq!(check(&unit(&text)), want, "{text}");
    }
}

/// Catches a unit with no control socket (it would drain at an update and refuse builds
/// until the other starts), a socket outside the unit's own runtime directory (systemd
/// would not make its directory for the user), a directory the next stop removes with the
/// socket in it, and a `Conflicts=` that stops the instance still draining.
#[test]
fn the_control_socket_outlives_an_instance_and_no_instance_stops_the_other() {
    let outside = |socket: &str, dirs: &str| {
        format!(
            "has --handover-socket={socket} outside its RuntimeDirectory= ({dirs}): \
             /run/<directory>/<file> with the directory named there, so systemd makes it for \
             the unit's user"
        )
    };
    let preserve = |what: &str| {
        format!(
            "has {what}, not RuntimeDirectoryPreserve=yes: the instance that stops would take \
             the control socket's directory with it"
        )
    };
    let rows: [(String, Vec<String>); 10] = [
        (
            with(" --handover-socket=/run/kbf-frontend/control.sock", ""),
            vec![
                "gives no --handover-socket: the instances cannot hand over, and an update \
                  would drain the serving one and refuse every new build until the other starts"
                    .to_owned(),
            ],
        ),
        (
            with("/run/kbf-frontend/control.sock", "/run/other/control.sock"),
            vec![outside("/run/other/control.sock", "kbf-frontend")],
        ),
        (
            with("/run/kbf-frontend/control.sock", "/tmp/control.sock"),
            vec![outside("/tmp/control.sock", "kbf-frontend")],
        ),
        (
            with("/run/kbf-frontend/control.sock", "/run/kbf-frontend/"),
            vec![outside("/run/kbf-frontend/", "kbf-frontend")],
        ),
        (
            with(
                "RuntimeDirectory=kbf-frontend\n",
                "RuntimeDirectory=kbf-frontend\nRuntimeDirectory=\n",
            ),
            vec![outside("/run/kbf-frontend/control.sock", "none given")],
        ),
        (
            without("RuntimeDirectory=kbf-frontend\n"),
            vec![outside("/run/kbf-frontend/control.sock", "none given")],
        ),
        (
            with(
                "RuntimeDirectory=kbf-frontend",
                "RuntimeDirectory=other kbf-frontend",
            ),
            Vec::new(),
        ),
        (
            with(
                "RuntimeDirectoryPreserve=yes",
                "RuntimeDirectoryPreserve=restart",
            ),
            vec![preserve("RuntimeDirectoryPreserve=restart")],
        ),
        (
            without("RuntimeDirectoryPreserve=yes\n"),
            vec![preserve("no RuntimeDirectoryPreserve=")],
        ),
        (
            with("[Service]", "Conflicts=kbf-frontend@b.service\n[Service]"),
            vec![
                "has Conflicts=kbf-frontend@b.service: starting an instance would stop the \
                  other, which is draining the builds in flight"
                    .to_owned(),
            ],
        ),
    ];
    for (text, want) in rows {
        assert_eq!(check(&unit(&text)), want, "{text}");
    }
}
