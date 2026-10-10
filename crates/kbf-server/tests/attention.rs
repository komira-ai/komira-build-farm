//! The server's log of a node's attention items (issue #164), in a test binary of its
//! own: the log is read through a global subscriber, which no other test's events can
//! reach or race (a subscriber set for one thread misses an event whose callsite
//! another thread registered first).

use std::io::Write;
use std::sync::{Arc, Mutex};

use kbf_caps::NodeCaps;
use kbf_front::Cache;
use kbf_proto::worker::{NodeStatus, ServerMessage, XcodeState, XcodeStatus};
use kbf_server::Farm;
use kbf_types::{Resources, WorkerId};
use tokio::sync::mpsc;

/// Where the global subscriber writes: every line, without colour.
#[derive(Clone, Default)]
struct Log(Arc<Mutex<Vec<u8>>>);

impl Write for Log {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("log").extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Log {
    /// The lines written since the last call, trimmed.
    fn take(&self) -> Vec<String> {
        let bytes = std::mem::take(&mut *self.0.lock().expect("log"));
        let text = String::from_utf8(bytes).expect("utf-8");
        text.lines().map(|l| l.trim().to_owned()).collect()
    }
}

fn status(licence: XcodeState, reason: &str) -> NodeStatus {
    NodeStatus {
        os_name: "macOS".to_owned(),
        xcodes: vec![XcodeStatus {
            app: "/Applications/Xcode_16.1.app".to_owned(),
            build: "16B40".to_owned(),
            state: licence.into(),
            reason: reason.to_owned(),
            fix: "sudo x -license accept".to_owned(),
        }],
        ..NodeStatus::default()
    }
}

/// What `xcodebuild -license check` prints, as an NSLog line: its time and pid differ
/// each time it is asked.
const FIRST: &str = "2026-10-09 12:00:01.123 xcodebuild[4321:9876] not agreed";
const AGAIN: &str = "2026-10-09 12:03:01.456 xcodebuild[5555:1234] not agreed";

fn not_ready(reason: &str) -> String {
    format!(
        "Xcode 16B40 (/Applications/Xcode_16.1.app) installed but not ready: {reason}; \
         fix: sudo x -license accept"
    )
}

/// Catches: the server's alert not written to its log (kbf has no alert delivery yet,
/// issue #189, so the log line is the alert), written below `WARN` or under another
/// target, without the node, the problem or the fix; a status that repeats it logged
/// again, or one whose only difference is the reason's time and pid (each survey's);
/// the newest reason not the one `GET /v1/nodes` shows; and its resolution not logged.
#[tokio::test]
async fn an_attention_item_is_a_warning_that_names_the_node_and_the_fix() {
    let log = Log::default();
    let writer = log.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_max_level(tracing::Level::INFO)
        .with_writer(move || writer.clone())
        .finish();
    tracing::subscriber::set_global_default(subscriber).expect("the only subscriber");

    let cache = Arc::new(Cache::memory());
    let farm = Farm::new(
        cache,
        kbf_sched::UNSERVABLE_WAIT,
        kbf_sched::FINISHED_RETENTION,
    );
    let node = WorkerId::new("mac-1");
    let (outbound, _responses) = mpsc::unbounded_channel();
    let stream = farm.register(
        &node,
        kbf_sched::DaemonInstance::new("mac-1"),
        Resources::new(8_000, 16 << 30),
        NodeCaps::from_report([("arch", "arm64")]).expect("caps"),
        outbound,
        ServerMessage::default(),
    );
    let attention = |lines: Vec<String>| -> Vec<String> {
        lines
            .into_iter()
            .filter(|l| l.contains("kbf_server::attention"))
            .collect()
    };
    log.take();

    let licence = XcodeState::LicenseNotAccepted;
    let _ = farm.node_status(&node, stream, status(licence, FIRST));
    assert_eq!(
        attention(log.take()),
        [format!(
            "WARN kbf_server::attention: node mac-1: {}",
            not_ready(FIRST)
        )]
    );
    let _ = farm.node_status(&node, stream, status(licence, FIRST));
    assert_eq!(attention(log.take()), Vec::<String>::new());
    let _ = farm.node_status(&node, stream, status(licence, AGAIN));
    assert_eq!(attention(log.take()), Vec::<String>::new());
    assert_eq!(farm.nodes().nodes[0].needs_attention, [not_ready(AGAIN)]);
    let _ = farm.node_status(&node, stream, status(XcodeState::Ready, ""));
    assert_eq!(
        attention(log.take()),
        [format!(
            "INFO kbf_server::attention: node mac-1: resolved: {}",
            not_ready(AGAIN)
        )]
    );
}
