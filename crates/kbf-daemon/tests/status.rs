//! The node's software status: what the daemon reads, and when it sends `NodeStatus`.

mod support;

use std::time::Duration;

use kbf_daemon::DriverReport;
use kbf_daemon::status::{Software, linux_software};
use kbf_proto::worker::{NodeStatus, XcodeState, XcodeStatus, daemon_message};
use support::{Harness, PROMPT, Peer, scratch};
use tokio::sync::watch;

const UBUNTU: &str = "PRETTY_NAME=\"Ubuntu 24.04.1 LTS\"\nNAME=\"Ubuntu\"\n\
    VERSION_ID=\"24.04\"\nVERSION=\"24.04.1 LTS (Noble Numbat)\"\nID=ubuntu\n";

/// Catches: the fallback os-release file never read, the kernel release kept with its
/// newline, and an unreadable file failing detection instead of leaving fields empty.
#[test]
fn linux_software_reads_the_first_readable_file() {
    let dir = scratch("linux-software");
    let fallback = dir.join("os-release");
    let kernel = dir.join("osrelease");
    std::fs::write(&fallback, UBUNTU).expect("write");
    std::fs::write(&kernel, "6.8.0-45-generic\n").expect("write");
    let missing = dir.join("missing");
    let got = linux_software(&[&missing, &fallback], &kernel);
    assert_eq!(
        got,
        Software {
            os_name: "Ubuntu".to_owned(),
            os_version: "24.04".to_owned(),
            os_build: String::new(),
            kernel: "6.8.0-45-generic".to_owned(),
        }
    );
    assert_eq!(linux_software(&[&missing], &missing), Software::default());
}

/// Catches: a daemon that never sends `NodeStatus`, sends it before `Welcome` (an old
/// server would see a second message before answering) or after the first Heartbeat,
/// sends it only on the first stream (a server restart would lose the node's
/// software), or sends what it did not detect.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_status_follows_every_welcome() {
    let mut h = Harness::start(
        "node-status",
        Duration::from_secs(60),
        Duration::from_secs(40),
    )
    .await;
    let want = Software::detect().status(&h.report);
    for _ in 0..2 {
        let mut peer = h.session().await;
        // Nothing but the Hello until the server answers.
        let early = peer
            .expect(Duration::from_millis(300), |m| match m {
                daemon_message::Message::Hello(_) => None,
                other => Some(format!("{other:?}")),
            })
            .await;
        assert_eq!(early, None, "a message before Welcome");
        peer.welcome();
        let mut seen = Vec::new();
        loop {
            let (_, m) = peer
                .expect(PROMPT, |m| Some(m.clone()))
                .await
                .expect("a message after Welcome");
            if matches!(m, daemon_message::Message::Heartbeat(_)) {
                break;
            }
            seen.push(m);
        }
        let [daemon_message::Message::NodeStatus(status)] = seen.as_slice() else {
            panic!("expected one NodeStatus before the first Heartbeat: {seen:?}");
        };
        assert_eq!(*status, want);
        assert_eq!(status.daemon_version, kbf_daemon::DAEMON_VERSION);
        assert!(
            status.xcode_builds.is_empty(),
            "the fake driver reports no Xcode"
        );
        peer.close();
    }
}

/// One Xcode as the driver reports it, in `state`.
fn xcode(build: &str, state: XcodeState, reason: &str) -> XcodeStatus {
    XcodeStatus {
        app: format!("/Applications/Xcode_{build}.app"),
        build: build.to_owned(),
        state: state.into(),
        reason: reason.to_owned(),
        fix: String::new(),
    }
}

/// The driver's report with `ready` advertised (an `xcode` entry each) and `xcodes`
/// listed.
fn driver(ready: &[&str], xcodes: Vec<XcodeStatus>) -> DriverReport {
    DriverReport {
        entries: ready
            .iter()
            .map(|b| ("xcode".to_owned(), (*b).to_owned()))
            .collect(),
        xcodes,
    }
}

/// The next message after Welcome that is not a Heartbeat, if one comes promptly.
async fn next(peer: &mut Peer) -> Option<daemon_message::Message> {
    peer.expect(PROMPT, |m| match m {
        daemon_message::Message::Heartbeat(_) => None,
        other => Some(other.clone()),
    })
    .await
    .map(|(_, m)| m)
}

/// The `xcode` entries of a Hello.
fn xcode_entries(m: &daemon_message::Message) -> Vec<String> {
    let daemon_message::Message::Hello(hello) = m else {
        panic!("expected a Hello: {m:?}");
    };
    hello
        .capabilities
        .iter()
        .filter(|c| c.key == "xcode")
        .map(|c| c.value.clone())
        .collect()
}

fn status(m: Option<daemon_message::Message>) -> NodeStatus {
    match m {
        Some(daemon_message::Message::NodeStatus(status)) => status,
        other => panic!("expected a NodeStatus: {other:?}"),
    }
}

/// Catches (issue #164): a not-ready Xcode left out of `NodeStatus`; an Xcode that
/// becomes ready mid-stream not advertised until the daemon restarts (no Hello resent,
/// or one without the new entry, so placement never sees it) or not shown in a new
/// `NodeStatus`; heartbeats still hashing the old report; a Hello resent when only
/// the status changed (the report did not); the driver's entries replacing the report
/// the daemon was started with instead of joining it; a change made while no stream
/// is up lost (the next stream's Hello and status must carry it); and a lease's
/// unacknowledged Result lost across those resent messages and the reconnect.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_driver_report_change_resends_hello_and_status() {
    let licence = xcode("16B40", XcodeState::LicenseNotAccepted, "not agreed");
    let (send, receive) = watch::channel(driver(&[], vec![licence.clone()]));
    let mut h = Harness::with_driver("driver-report", receive).await;
    let mut peer = h.session().await;
    let first = peer.hello().await;
    assert!(
        first.capabilities.iter().all(|c| c.key != "xcode"),
        "a not-ready Xcode advertised: {first:?}"
    );
    let base = first.capabilities.len();
    assert!(base > 0, "the detected report");
    peer.welcome();
    let before = status(next(&mut peer).await);
    assert_eq!(before.xcodes, std::slice::from_ref(&licence));
    assert_eq!(before.xcode_builds, Vec::<String>::new());

    let ready = xcode("16B40", XcodeState::Ready, "");
    send.send_replace(driver(&["16B40"], vec![ready.clone()]));
    let resent = next(&mut peer).await.expect("a resent Hello");
    assert_eq!(xcode_entries(&resent), ["16B40"]);
    let daemon_message::Message::Hello(resent) = resent else {
        unreachable!()
    };
    assert_eq!(
        resent.capabilities.len(),
        base + 1,
        "the started report kept"
    );
    let after = status(next(&mut peer).await);
    assert_eq!(after.xcodes, std::slice::from_ref(&ready));
    assert_eq!(after.xcode_builds, ["16B40"]);
    let beat = peer.heartbeat().await;
    assert_eq!(
        beat.report_hash, resent.report_hash,
        "heartbeats hash the new report"
    );

    // Only the status changes: no Hello.
    let other = xcode("17A1", XcodeState::FirstLaunchNotRun, "first launch");
    send.send_replace(driver(&["16B40"], vec![ready.clone(), other.clone()]));
    let only = status(next(&mut peer).await);
    assert_eq!(only.xcodes, [ready.clone(), other.clone()]);

    // A lease that ends on this stream and is not acknowledged.
    peer.start(1, 1, "action");
    peer.result(PROMPT).await.expect("a Result");

    // While no stream is up.
    peer.close();
    send.send_replace(driver(&[], vec![licence.clone()]));
    let mut peer = h.session().await;
    let hello = peer.hello().await;
    assert!(
        hello.capabilities.iter().all(|c| c.key != "xcode"),
        "{hello:?}"
    );
    assert_eq!(hello.capabilities.len(), base);
    peer.welcome();
    // The Hellos and statuses the driver's changes added did not cost the lease its
    // Result: it is resent, before the new stream's NodeStatus.
    assert!(
        matches!(
            next(&mut peer).await,
            Some(daemon_message::Message::Result(_))
        ),
        "the unacknowledged Result resent"
    );
    assert_eq!(status(next(&mut peer).await).xcodes, [licence]);
    drop(send);
    // A gone driver ends nothing: the stream goes on.
    peer.heartbeat().await;
    peer.heartbeat().await;
}
