//! The node's software status: what the daemon reads, and when it sends `NodeStatus`.

mod support;

use std::time::Duration;

use kbf_daemon::status::{Software, linux_software};
use kbf_proto::worker::daemon_message;
use support::{Harness, PROMPT, scratch};

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
        assert_eq!(status.daemon_version, env!("CARGO_PKG_VERSION"));
        assert!(
            status.xcode_builds.is_empty(),
            "the fake driver reports no Xcode"
        );
        peer.close();
    }
}
