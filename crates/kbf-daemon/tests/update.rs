//! The update client on the session: the node's status goes out after each Welcome and on
//! every change; offers are fetched and orders written through the client; and while an
//! accepted activation holds the update gate the node offers no capacity and refuses every
//! Start. The client and the helper's status file are real; the release bytes come from
//! memory and the helper is the test.

mod support;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;
use kbf_daemon::{BUILD_COMMIT, UpdateLink};
use kbf_proto::google::rpc::Code;
use kbf_proto::update::update_order::Verb;
use kbf_proto::update::update_status::State;
use kbf_proto::update::{Component, UpdateOffer, UpdateOrder};
use kbf_proto::worker::{Phase, daemon_message::Message};
use kbf_update::{Chunks, Config, FetchError, Source, start};
use sha2::{Digest as _, Sha256};
use support::{Harness, PROMPT, Peer};
use tokio::task::JoinHandle;

const PLATFORM: &str = "linux-x86_64";
const LONG: Duration = Duration::from_secs(60);

/// Release bytes in memory, by digest.
#[derive(Default)]
struct Blobs(Mutex<BTreeMap<String, Vec<u8>>>);

impl Source for Blobs {
    fn fetch(&self, sha256: &str) -> BoxFuture<'_, Result<Chunks, FetchError>> {
        let data = self.0.lock().expect("lock")[sha256].clone();
        let chunks: Vec<Result<Vec<u8>, FetchError>> =
            data.chunks(100).map(|c| Ok(c.to_vec())).collect();
        Box::pin(async move { Ok(Box::pin(futures::stream::iter(chunks)) as Chunks) })
    }
}

/// A daemon with the update client over a scratch directory.
struct Node {
    h: Harness,
    dir: PathBuf,
    blobs: Arc<Blobs>,
    worker: JoinHandle<()>,
}

impl Drop for Node {
    fn drop(&mut self) {
        self.worker.abort();
    }
}

fn commit(digit: char) -> String {
    digit.to_string().repeat(40)
}

/// What the helper writes: `state`, with `installed` and `staged` as the commits it
/// holds, having acted on order `acked`.
fn helper_says(dir: &Path, state: &str, installed: &str, staged: &str, acked: u64) {
    let quote = |c: &str| format!("\"{c}\"").replace("\"\"", "null");
    let text = format!(
        "{{\"schema\":1,\"installed_commit\":{},\"previous_commit\":null,\"staged_commit\":{},\
         \"state\":\"{state}\",\"refusal\":\"\",\"acked_order_id\":{acked},\"staged\":null}}",
        quote(installed),
        quote(staged)
    );
    fs::write(dir.join("status.json"), text).expect("write status.json");
}

impl Node {
    /// A node whose helper says it is idle, its lease taking `run_for`.
    async fn new(name: &str, run_for: Duration) -> Self {
        let dir = support::scratch(name);
        helper_says(&dir, "idle", "", "", 0);
        let mut config = Config::new(dir.clone(), PLATFORM.to_owned(), BUILD_COMMIT.to_owned());
        config.poll_every = Duration::from_millis(20);
        let blobs = Arc::new(Blobs::default());
        let (handle, worker) = start(config, Arc::clone(&blobs) as Arc<dyn Source>).expect("start");
        let link = UpdateLink::new(handle);
        Self {
            h: Harness::with_update(name, run_for, link).await,
            dir,
            blobs,
            worker: tokio::spawn(worker.run()),
        }
    }

    /// The offer of `generation` for a tarball of `size` bytes, which the blobs hold.
    fn offer(&self, generation: u64, size: usize) -> UpdateOffer {
        let data: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
        let sha256 = hex::encode(Sha256::digest(&data));
        self.blobs
            .0
            .lock()
            .expect("lock")
            .insert(sha256.clone(), data);
        UpdateOffer {
            generation,
            commit: commit('1'),
            platform: PLATFORM.to_owned(),
            components: vec![Component {
                name: "kbf".to_owned(),
                sha256,
                size: size as u64,
                attestation: b"{}\n".to_vec(),
            }],
        }
    }

    fn order_file(&self, id: u64) -> PathBuf {
        self.dir.join(format!("inbox/orders/{id:020}.json"))
    }
}

fn order(id: u64, epoch: u64, verb: Verb, commit: &str) -> UpdateOrder {
    UpdateOrder {
        order_id: id,
        epoch,
        verb: verb as i32,
        commit: commit.to_owned(),
        window_s: 600,
    }
}

/// The name of a daemon message: `Hello`, `Heartbeat`, `UpdateStatus`, ...
fn kind(message: &Message) -> String {
    let text = format!("{message:?}");
    text.split('(').next().unwrap_or_default().to_owned()
}

/// The next `Hello` the daemon sends whose phase is `phase` (`None`: serving), and its
/// reason.
async fn hello_in(peer: &mut Peer, phase: Option<Phase>) -> String {
    let want = phase.map(|p| p as i32);
    let (_, reason) = peer
        .expect(PROMPT, |m| match m {
            Message::Hello(h) => {
                let got = h.phase.as_ref().map(|p| p.phase);
                (got == want).then(|| h.phase.clone().unwrap_or_default().reason)
            }
            _ => None,
        })
        .await
        .expect("a Hello in that phase");
    reason
}

/// The result of lease `lease`, with its status code and message.
async fn result_of(peer: &mut Peer) -> (i32, String) {
    let (_, result) = peer.result(PROMPT).await.expect("a Result");
    let status = result.status.expect("a status");
    (status.code, status.message)
}

/// Catches a status that is not sent after each Welcome (a restarted frontend would not
/// know the node's state), one that is not sent when it changes (an offer fetched, an
/// order accepted), and one whose running commit is not the build's own. (Mutant: no
/// status after Welcome; no status on change.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_status_is_sent_after_each_welcome_and_on_every_change() {
    let mut node = Node::new("update-status", LONG).await;
    let mut peer = node.h.welcomed().await;
    let first = peer.update_status(|_| true).await;
    assert_eq!(first.state, State::Idle as i32);
    assert_eq!(first.running_commit, BUILD_COMMIT);
    assert!(first.helper_present);
    assert_eq!(first.offered_generation, 0);

    peer.update_offer(node.offer(3, 450));
    let fetched = peer
        .update_status(|s| s.offered_generation == 3 && s.state == State::Idle as i32)
        .await;
    assert_eq!(fetched.refusal, "");
    let dir = node.dir.join(format!("inbox/{:020}-{}", 3, commit('1')));
    assert!(
        dir.join("offer.json").is_file(),
        "the offer is in the inbox"
    );

    // A new stream: the same status again, after Welcome.
    peer.close();
    let mut next = node.h.session().await;
    next.hello().await;
    next.welcome();
    let again = next.update_status(|_| true).await;
    assert_eq!(again, fetched);
}

/// Catches an offer the client refuses staying silent: the refusal and its reason reach
/// the frontend in the status.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_offer_is_reported_with_its_reason() {
    let mut node = Node::new("update-refused", LONG).await;
    let mut peer = node.h.welcomed().await;
    let mut offer = node.offer(1, 100);
    offer.platform = "darwin-arm64".to_owned();
    peer.update_offer(offer);
    let refused = peer
        .update_status(|s| s.state == State::Refused as i32)
        .await;
    assert!(refused.refusal.contains("platform"), "{}", refused.refusal);
    assert_eq!(refused.offered_generation, 1);
}

/// Catches a daemon with no link that sends a status (a server that predates updates
/// reads it as an unknown message), and one that fails on an offer or an order it has no
/// use for; and a node that updates by hand saying so once and ignoring both.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_node_without_a_client_sends_nothing_and_a_manual_one_says_so() {
    let mut bare = Harness::start("update-none", Duration::from_millis(10), LONG).await;
    let mut peer = bare.welcomed().await;
    peer.update_messages();
    let first = peer
        .expect(PROMPT, |m| {
            let name = kind(m);
            (name == "UpdateStatus" || name == "Heartbeat").then_some(name)
        })
        .await
        .expect("a message")
        .1;
    assert_eq!(first, "Heartbeat");

    let link = UpdateLink::manual(BUILD_COMMIT);
    let mut manual = Harness::with_update("update-manual", Duration::from_millis(10), link).await;
    let mut peer = manual.welcomed().await;
    let status = peer.update_status(|_| true).await;
    assert_eq!(status.state, State::Manual as i32);
    assert!(!status.helper_present);
    assert_eq!(status.running_commit, BUILD_COMMIT);
    peer.update_messages();
    // Both are ignored; the next thing the node sends is a heartbeat, not a status.
    let next = peer
        .expect(PROMPT, |m| {
            let name = kind(m);
            (name == "UpdateStatus" || name == "Heartbeat").then_some(name)
        })
        .await
        .expect("a message")
        .1;
    assert_eq!(next, "Heartbeat");
}

/// Catches an activation that closes no gate, a gate that offers capacity (the Hello must
/// say the node takes no lease) or lets a Start run, and a gate that never opens: the
/// helper refusing the order while this process stays up opens it. (Mutants: no gate in
/// the Start path; no phase in the Hello.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_accepted_activation_closes_the_gate_until_the_helper_settles_it() {
    let mut node = Node::new("update-gate", Duration::from_millis(10)).await;
    let target = commit('9');
    helper_says(&node.dir, "staged", &commit('8'), &target, 0);
    let mut peer = node.h.welcomed().await;
    peer.update_status(|s| s.helper_present).await;

    peer.update_order(order(1, 0, Verb::Activate, &target));
    let accepted = peer.update_status(|s| s.acked_order_id == 1).await;
    assert_eq!(accepted.refusal, "");
    assert!(node.order_file(1).is_file());
    let reason = hello_in(&mut peer, Some(Phase::Quarantined)).await;
    assert!(reason.contains("update to 999999999999"), "{reason}");

    // A Start in the gate is refused with a Result, and runs nothing.
    peer.start(1, 1, "action");
    let (code, message) = result_of(&mut peer).await;
    assert_eq!(code, Code::Unavailable as i32);
    assert!(message.contains("update to 999999999999"), "{message}");
    assert!(node.h.runtime.started().is_empty());

    // The helper refused the order and this process stays up: the node takes leases again.
    helper_says(&node.dir, "refused", &commit('8'), &target, 1);
    hello_in(&mut peer, None).await;
    peer.start(1, 2, "action");
    let (code, _) = result_of(&mut peer).await;
    assert_eq!(code, Code::Ok as i32);
    assert_eq!(node.h.runtime.started().len(), 1);
}

/// Catches an activation accepted with a lease running, and a lease that finished with a
/// Result the server has not acknowledged not counted: the order is refused with the
/// reason, no file is written and no gate closes; once the Result is acknowledged it is
/// accepted. (Mutant: count only running leases.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_activation_waits_for_every_lease_and_every_unacknowledged_result() {
    let mut node = Node::new("update-leases", LONG).await;
    let target = commit('9');
    helper_says(&node.dir, "staged", &commit('8'), &target, 0);
    let mut peer = node.h.welcomed().await;
    peer.update_status(|s| s.helper_present).await;

    // One lease running.
    peer.start(1, 1, "action");
    node.h.started(1).await;
    peer.update_order(order(1, 0, Verb::Activate, &target));
    let refused = peer
        .update_status(|s| s.state == State::Refused as i32)
        .await;
    assert_eq!(refused.refusal, "1 leases are live; not activating");
    assert_eq!(refused.acked_order_id, 0);
    assert!(!node.order_file(1).exists());
    // No gate: a second lease still starts.
    peer.start(1, 2, "action");
    node.h.started(2).await;
}

/// Catches the same with the lease finished: its Result is out and unacknowledged, which
/// the scheduler still counts as the node's lease.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unacknowledged_result_counts_as_a_live_lease() {
    let mut node = Node::new("update-unacked", Duration::from_millis(10)).await;
    let target = commit('9');
    helper_says(&node.dir, "staged", &commit('8'), &target, 0);
    let mut peer = node.h.welcomed().await;
    peer.update_status(|s| s.helper_present).await;
    peer.start(1, 1, "action");
    peer.result(PROMPT).await.expect("a Result");

    peer.update_order(order(1, 0, Verb::Activate, &target));
    let refused = peer
        .update_status(|s| s.state == State::Refused as i32)
        .await;
    assert_eq!(refused.refusal, "1 leases are live; not activating");
    assert!(!node.order_file(1).exists());

    peer.ack_result(Some((1, 1)), true);
    // The acknowledgement reaches the daemon before the order does (one stream, in order).
    peer.update_order(order(2, 0, Verb::Activate, &target));
    let accepted = peer.update_status(|s| s.acked_order_id == 2).await;
    assert_eq!(accepted.refusal, "");
    assert!(node.order_file(2).is_file());
}

/// Catches an order of another epoch than the session's Welcome, taken from the frontend
/// before a restart: refused with the reason, and the next session's epoch is the one
/// asked for. A replay of an accepted order writes nothing more. (Mutant: the epoch of the
/// first Welcome kept for the daemon's life; no epoch check.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_order_must_carry_the_epoch_of_the_session() {
    let mut node = Node::new("update-epoch", LONG).await;
    let target = commit('9');
    helper_says(&node.dir, "confirmed", &target, "", 0);
    let mut first = node.h.session().await;
    first.hello().await;
    first.welcome_epoch(7);
    first.update_status(|s| s.helper_present).await;

    first.update_order(order(1, 6, Verb::Confirm, &target));
    let stale = first
        .update_status(|s| s.state == State::Refused as i32)
        .await;
    assert_eq!(stale.refusal, "order 1 is of epoch 6; this session's is 7");
    assert!(!node.order_file(1).exists());

    first.update_order(order(1, 7, Verb::Confirm, &target));
    let accepted = first.update_status(|s| s.acked_order_id == 1).await;
    assert_eq!(accepted.refusal, "");
    // A replay: nothing is written again.
    fs::write(node.order_file(1), "marker").expect("mark");
    first.update_order(order(1, 7, Verb::Confirm, &target));
    first.update_order(order(2, 7, Verb::Confirm, &target));
    first.update_status(|s| s.acked_order_id == 2).await;
    assert_eq!(
        fs::read_to_string(node.order_file(1)).expect("file"),
        "marker"
    );

    // The frontend restarted: epoch 8. An order of epoch 7 is a replay from before.
    first.close();
    let mut second = node.h.session().await;
    second.hello().await;
    second.welcome_epoch(8);
    second.update_order(order(3, 7, Verb::Confirm, &target));
    let old = second
        .update_status(|s| s.state == State::Refused as i32)
        .await;
    assert_eq!(old.refusal, "order 3 is of epoch 7; this session's is 8");
    second.update_order(order(3, 8, Verb::Confirm, &target));
    second.update_status(|s| s.acked_order_id == 3).await;
}

/// Catches a revert that leaves the gate closed: it is written without the lease check
/// and opens the gate at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_revert_opens_the_gate() {
    let mut node = Node::new("update-revert", Duration::from_millis(10)).await;
    let target = commit('9');
    helper_says(&node.dir, "staged", &commit('8'), &target, 0);
    let mut peer = node.h.welcomed().await;
    peer.update_status(|s| s.helper_present).await;
    peer.update_order(order(1, 0, Verb::Activate, &target));
    hello_in(&mut peer, Some(Phase::Quarantined)).await;
    peer.update_order(order(2, 0, Verb::Revert, &commit('8')));
    let status = peer.update_status(|s| s.acked_order_id == 2).await;
    assert_eq!(status.refusal, "");
    hello_in(&mut peer, None).await;
    peer.start(1, 1, "action");
    let (code, _) = result_of(&mut peer).await;
    assert_eq!(code, Code::Ok as i32);
}
