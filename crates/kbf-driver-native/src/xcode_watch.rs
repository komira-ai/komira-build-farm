//! Asking the Xcodes again while the daemon runs, so one a human fixes (accepts its
//! licence, runs its first launch, downloads its Metal toolchain) becomes ready without
//! a restart, and one that stops being ready (an update whose new licence is not
//! accepted) stops being advertised (issue #164).
//!
//! [`watch`] surveys the Xcodes once before it returns, then again every `every` on a
//! thread of its own. Each survey that differs from the last is applied (the caller
//! makes its ready Xcodes the ones actions may name and returns what the daemon
//! reports) and sent to the daemon, which resends its Hello when the ready set changed
//! and its `NodeStatus` either way. Each change of an Xcode's state is logged once:
//! at `WARN` when it is installed but not ready, with why and the fix, and at `INFO`
//! when it is ready or gone. A survey that matches the last logs and sends nothing.
//!
//! Surveys are compared by each Xcode's app, `DEVELOPER_DIR`, build and state (which
//! decides its fix), not by its reason: that is the failed question's stderr, and
//! `xcodebuild` starts its NSLog lines with the time and its pid, which differ on every
//! survey. A survey whose only difference is a reason logs and sends nothing, so the
//! reason the daemon reports is the one its Xcode had when it last changed.

use std::path::PathBuf;
use std::thread::JoinHandle;
use std::time::Duration;

use kbf_daemon::DriverReport;
use tokio::sync::watch;

use crate::xcode::{self, Probe, State, Xcode};

/// How often the Xcodes are asked again by default: often enough that a fix shows
/// within minutes, rarely enough that the questions (a few process starts per Xcode)
/// cost nothing.
pub const EVERY: Duration = Duration::from_secs(180);

/// What [`watch`] calls with each survey that differs from the last: makes its ready
/// Xcodes the ones actions may name and returns what the daemon reports.
pub type Apply = Box<dyn Fn(&[Xcode]) -> DriverReport + Send>;

/// Surveys the Xcodes in `apps` as `probe` says, applies the survey, and keeps doing so
/// every `every` on a thread of its own (see the module documentation). Returns what to
/// hand the daemon, and the thread, which ends at its next survey once every receiver
/// is gone.
#[must_use]
pub fn watch(
    apps: PathBuf,
    probe: Probe,
    every: Duration,
    apply: Apply,
) -> (watch::Receiver<DriverReport>, JoinHandle<()>) {
    let mut last = xcode::survey(&apps, &probe);
    tracing::info!(apps = %apps.display(), found = last.len(), "Xcodes");
    log(&changes(&[], &last));
    let (send, receive) = watch::channel(apply(&last));
    let thread = std::thread::spawn(move || {
        loop {
            std::thread::sleep(every);
            if send.is_closed() {
                return;
            }
            let now = xcode::survey(&apps, &probe);
            if !same(&last, &now) {
                log(&changes(&last, &now));
                send.send_replace(apply(&now));
                last = now;
            }
        }
    });
    (receive, thread)
}

/// Whether a change wants a human (`WARN`) or is news (`INFO`), and what it says.
type Change = (bool, String);

/// What a survey is compared by: an Xcode's app, `DEVELOPER_DIR`, build and state (and
/// so its fix), not its reason (see the module documentation).
fn key(xcode: &Xcode) -> (&PathBuf, Option<&PathBuf>, Option<&str>, State) {
    (
        &xcode.app,
        xcode.developer_dir.as_ref(),
        xcode.build.as_deref(),
        xcode.state,
    )
}

/// Whether `a` and `b` name the same Xcodes, in the same order, alike but for reasons.
fn same(a: &[Xcode], b: &[Xcode]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(a, b)| key(a) == key(b))
}

/// What changed from `before` to `after`, one line per app whose `DEVELOPER_DIR`, build
/// or state changed, appeared or went: an Xcode not ready says why and how to fix it.
fn changes(before: &[Xcode], after: &[Xcode]) -> Vec<Change> {
    let found = |list: &[Xcode], app: &PathBuf| list.iter().position(|x| &x.app == app);
    let mut lines: Vec<Change> = after
        .iter()
        .filter(|x| found(before, &x.app).is_none_or(|i| key(&before[i]) != key(x)))
        .map(|x| match x.state {
            State::Ready => (false, format!("{} ready", named(x))),
            _ => (
                true,
                format!(
                    "{} installed but not ready: {}; fix: {}",
                    named(x),
                    x.reason,
                    x.fix().as_deref().unwrap_or("none known")
                ),
            ),
        })
        .collect();
    lines.extend(
        before
            .iter()
            .filter(|x| found(after, &x.app).is_none())
            .map(|x| (false, format!("{} no longer installed", named(x)))),
    );
    lines
}

/// `Xcode <build> (<app>)`, or `Xcode (<app>)` while its build is not known.
fn named(xcode: &Xcode) -> String {
    let app = xcode.app.display();
    match &xcode.build {
        Some(build) => format!("Xcode {build} ({app})"),
        None => format!("Xcode ({app})"),
    }
}

fn log(changes: &[Change]) {
    for (warn, line) in changes {
        if *warn {
            tracing::warn!("{line}");
        } else {
            tracing::info!("{line}");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    use kbf_proto::worker::XcodeState;

    use super::*;
    use crate::xcode::tests::{NOT_AGREED, fake, scratch};

    fn at(app: &str, build: Option<&str>, state: State, reason: &str) -> Xcode {
        Xcode {
            app: PathBuf::from(app),
            developer_dir: Some(PathBuf::from(app).join("Contents/Developer")),
            build: build.map(str::to_owned),
            state,
            reason: reason.to_owned(),
        }
    }

    /// Catches: a change logged on every survey rather than once, a change of state or
    /// build not logged, a change of reason alone logged (an NSLog line's time and pid
    /// differ on every survey), an Xcode not ready logged without its fix or not as a
    /// warning, one that became ready, appeared or went not logged, and a not-ready
    /// Xcode with no fix said to have one.
    #[test]
    fn each_change_is_logged_once() {
        let licence = at(
            "/A/Xcode_1.app",
            Some("1A"),
            State::LicenseNotAccepted,
            "69",
        );
        let good = at("/A/Xcode_2.app", Some("2B"), State::Ready, "");
        let mute = at("/A/Xcode_3.app", None, State::Failed, "no build");
        let before = [licence.clone(), good.clone(), mute.clone()];
        assert_eq!(changes(&before, &before), []);
        let fix = "sudo /A/Xcode_1.app/Contents/Developer/usr/bin/xcodebuild -license accept";
        assert_eq!(
            changes(&[], &before),
            [
                (
                    true,
                    format!("Xcode 1A (/A/Xcode_1.app) installed but not ready: 69; fix: {fix}")
                ),
                (false, "Xcode 2B (/A/Xcode_2.app) ready".to_owned()),
                (
                    true,
                    "Xcode (/A/Xcode_3.app) installed but not ready: no build; fix: none known"
                        .to_owned()
                ),
            ]
        );
        let accepted = Xcode {
            state: State::Ready,
            reason: String::new(),
            ..licence.clone()
        };
        let restamped = Xcode {
            reason: "2026-10-09 12:00:03.456 xcodebuild[4321:9876] 69".to_owned(),
            ..licence.clone()
        };
        assert_eq!(
            changes(&before, &[restamped, good.clone(), mute.clone()]),
            []
        );
        let other_build = Xcode {
            build: Some("3C".to_owned()),
            reason: "hung".to_owned(),
            ..mute.clone()
        };
        let new = at("/A/Xcode_4.app", Some("4D"), State::Ready, "");
        assert_eq!(
            changes(&before, &[accepted, other_build, new]),
            [
                (false, "Xcode 1A (/A/Xcode_1.app) ready".to_owned()),
                (
                    true,
                    "Xcode 3C (/A/Xcode_3.app) installed but not ready: hung; fix: none known"
                        .to_owned()
                ),
                (false, "Xcode 4D (/A/Xcode_4.app) ready".to_owned()),
                (
                    false,
                    "Xcode 2B (/A/Xcode_2.app) no longer installed".to_owned()
                ),
            ]
        );
    }

    /// Catches: an Xcode a human fixes never advertised until the daemon restarts, one
    /// that stops being ready still advertised, the daemon sent a report on every
    /// survey rather than on each change (or never), what the daemon is sent not the
    /// newest survey's, and a thread that outlives the daemon's receiver.
    #[test]
    fn a_fixed_xcode_becomes_ready_without_a_restart() {
        let _ = tracing_subscriber::fmt().with_test_writer().try_init();
        let dir = scratch("watch");
        let apps = dir.join("Applications");
        std::fs::create_dir_all(apps.join("Xcode_16.app/Contents/Developer")).expect("app");
        let accepted = dir.join("accepted");
        // `-license check` passes once the file `accepted` exists, as after
        // `sudo xcodebuild -license accept`.
        let xcodebuild = fake(
            &dir,
            "xcodebuild",
            &format!(
                "#!/bin/sh\n\
                 case \"$*\" in\n\
                 -version) echo 'Build version 16C5032a' ;;\n\
                 '-license check') [ -e '{}' ] || {{ echo '{NOT_AGREED}' >&2; exit 69; }} ;;\n\
                 esac\n",
                accepted.display()
            ),
        );
        let probe = Probe {
            xcodebuild,
            xcrun: PathBuf::from("/bin/echo"),
            within: Duration::from_secs(5),
            metal: false,
        };
        let applied = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&applied);
        let apply: Apply = Box::new(move |xcodes| {
            seen.lock().expect("applied").push(xcodes.to_vec());
            DriverReport {
                entries: xcode::ready(xcodes)
                    .into_keys()
                    .map(|build| ("xcode".to_owned(), build))
                    .collect(),
                xcodes: xcodes.iter().map(Xcode::status).collect(),
            }
        });
        let every = Duration::from_millis(50);
        let (mut reports, thread) = watch(apps, probe, every, apply);
        let state = |report: &DriverReport| report.xcodes[0].state();
        assert_eq!(
            state(&reports.borrow_and_update()),
            XcodeState::LicenseNotAccepted
        );
        assert_eq!(reports.borrow().entries, []);
        let next = |reports: &mut watch::Receiver<DriverReport>| {
            let deadline = Instant::now() + Duration::from_secs(20);
            while !reports.has_changed().expect("the watch runs") {
                assert!(Instant::now() < deadline, "no new report");
                std::thread::sleep(Duration::from_millis(10));
            }
            reports.borrow_and_update().clone()
        };

        std::fs::write(&accepted, "").expect("accept");
        let fixed = next(&mut reports);
        assert_eq!(state(&fixed), XcodeState::Ready);
        assert_eq!(fixed.entries, [("xcode".to_owned(), "16C5032a".to_owned())]);
        // Unchanged surveys send nothing.
        std::thread::sleep(every * 6);
        assert!(!reports.has_changed().expect("the watch runs"));

        std::fs::remove_file(&accepted).expect("an update's new licence");
        let broken = next(&mut reports);
        assert_eq!(state(&broken), XcodeState::LicenseNotAccepted);
        assert_eq!(broken.entries, []);
        let states: Vec<Vec<State>> = applied
            .lock()
            .expect("applied")
            .iter()
            .map(|survey| survey.iter().map(|x| x.state).collect())
            .collect();
        assert_eq!(
            states,
            [
                [State::LicenseNotAccepted],
                [State::Ready],
                [State::LicenseNotAccepted]
            ]
        );

        drop(reports);
        thread
            .join()
            .expect("the thread ends once the daemon is gone");
        kbf_outputs::remove_tree(&dir).expect("clean");
    }

    /// Catches: an Xcode that stays not ready logged at `WARN` again, and its report
    /// resent to the daemon (which resends `NodeStatus`, so the server raises it again),
    /// on every survey whose only difference is the reason: here an NSLog-style line
    /// whose time and pid differ on each run, as `xcodebuild`'s do. The log and the
    /// send share one branch, so one application means one `WARN`.
    #[test]
    fn a_reason_that_changes_alone_is_logged_and_sent_once() {
        let _ = tracing_subscriber::fmt().with_test_writer().try_init();
        let dir = scratch("restamp");
        let apps = dir.join("Applications");
        std::fs::create_dir_all(apps.join("Xcode_16.app/Contents/Developer")).expect("app");
        let xcodebuild = fake(
            &dir,
            "xcodebuild",
            &format!(
                "#!/bin/sh\n\
                 case \"$*\" in\n\
                 -version) echo 'Build version 16C5032a' ;;\n\
                 '-license check') echo \"$(date '+%Y-%m-%d %H:%M:%S') xcodebuild[$$:1] \
                 {NOT_AGREED}\" >&2; exit 69 ;;\n\
                 esac\n"
            ),
        );
        let probe = Probe {
            xcodebuild,
            xcrun: PathBuf::from("/bin/echo"),
            within: Duration::from_secs(5),
            metal: false,
        };
        // The reasons do differ from survey to survey, by the pid at least.
        let (one, two) = (xcode::survey(&apps, &probe), xcode::survey(&apps, &probe));
        assert_ne!(one, two);
        assert!(same(&one, &two));
        assert!(one[0].reason.contains(NOT_AGREED), "{:?}", one[0].reason);

        let applied = Arc::new(Mutex::new(0));
        let seen = Arc::clone(&applied);
        let apply: Apply = Box::new(move |xcodes| {
            *seen.lock().expect("applied") += 1;
            DriverReport {
                entries: Vec::new(),
                xcodes: xcodes.iter().map(Xcode::status).collect(),
            }
        });
        let every = Duration::from_millis(50);
        let (mut reports, thread) = watch(apps, probe, every, apply);
        assert_eq!(
            reports.borrow_and_update().xcodes[0].state(),
            XcodeState::LicenseNotAccepted
        );
        std::thread::sleep(every * 10);
        assert!(!reports.has_changed().expect("the watch runs"));
        assert_eq!(*applied.lock().expect("applied"), 1);

        drop(reports);
        thread
            .join()
            .expect("the thread ends once the daemon is gone");
        kbf_outputs::remove_tree(&dir).expect("clean");
    }
}
