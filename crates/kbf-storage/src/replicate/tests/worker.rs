//! The repair worker against a farm: the copies owed are made when their owners can take
//! them, and not before.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use bytes::Bytes;
use kbf_map::MemberState;
use kbf_store::Fault;
use kbf_types::Digest;

use super::chain::{MIB, held, within};
use super::degrade::a_build_with_a_killed_owner;
use super::farm::Farm;
use crate::repair::{Options, Pass, RepairConfig, RepairWorker, Repairs};
use crate::replicate::{Chain, ChainConfig, MapSource, Peers};
use crate::service::tests::fake::{Held, Mem, StreamFault, sha};
use crate::{Answer, ReadStream, Storage, StorageError};

fn config() -> RepairConfig {
    RepairConfig {
        interval: Duration::from_millis(50),
        concurrency: 2,
        ..RepairConfig::default()
    }
}

fn worker(farm: &Farm, node: usize) -> RepairWorker {
    RepairWorker::new(
        Arc::clone(&farm.nodes[node].mem) as Arc<dyn Storage>,
        Arc::clone(&farm.nodes[node].chain),
        Arc::clone(&farm.nodes[node].repairs),
        config(),
    )
}

fn owed_digests(farm: &Farm, node: usize) -> Vec<Digest> {
    farm.nodes[node]
        .repairs
        .open_records()
        .iter()
        .map(|r| r.digest)
        .collect()
}

/// A blob written to node 0 with node 1 cut off: held by node 0 alone, owed to node 1.
async fn owed_blob(farm: &Farm, len: usize, salt: u8) -> (Vec<u8>, Digest) {
    let map = farm.map(1, 2, &[]);
    let bytes = farm.blob(&map, 0, 1, len, salt);
    farm.nodes[1].switch.cut();
    let digest = farm
        .client(0, 1)
        .put(Bytes::from(bytes.clone()))
        .await
        .expect("answered with one copy");
    (bytes, digest)
}

async fn settled_told(farm: &Farm, node: usize, digest: &Digest) -> bool {
    for _ in 0..500 {
        if farm.nodes[node]
            .ledger
            .settled
            .lock()
            .expect("lock")
            .contains(digest)
        {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    false
}

/// Catches a debt settled before its copy is made, or a copy never made: with the replica
/// unreachable, the pass leaves the debt and nothing is sent; once it is back, the pass
/// makes the copy (the same bytes), settles the debt, and tells the frontend; the next pass
/// has nothing to do. Mutants: the debt settled whatever happened; the copy not sent.
#[tokio::test]
async fn the_copy_is_made_when_the_replica_returns() {
    let farm = Farm::start(2, ChainConfig::default()).await;
    let (bytes, digest) = owed_blob(&farm, 5 * MIB + 3, 1).await;
    let repairs = worker(&farm, 0);
    assert_eq!(
        repairs.pass().await,
        Pass {
            settled: 0,
            waiting: 1
        }
    );
    assert_eq!(owed_digests(&farm, 0), [digest]);
    assert!(!held(&farm, 1, &digest));

    farm.nodes[1].switch.restore();
    assert_eq!(
        repairs.pass().await,
        Pass {
            settled: 1,
            waiting: 0
        }
    );
    assert!(owed_digests(&farm, 0).is_empty());
    let copy = farm.nodes[1]
        .mem
        .blobs
        .lock()
        .expect("lock")
        .get(&digest)
        .map(|b| b.to_vec());
    assert_eq!(copy, Some(bytes));
    assert!(settled_told(&farm, 0, &digest).await);
    assert_eq!(repairs.pass().await, Pass::default());
}

/// Catches a blob sent to an owner that already has it, and a read of the store for it:
/// the replica gets the blob by other means (a copy sent to it directly) while the debt
/// stands; the pass asks it, finds it there, and settles without reading the blob.
/// Mutant: the question skipped.
#[tokio::test]
async fn an_owner_that_already_holds_the_blob_is_asked_not_sent_it() {
    let farm = Farm::start(2, ChainConfig::default()).await;
    let (bytes, _) = owed_blob(&farm, 4096, 2).await;
    farm.nodes[1].switch.restore();
    farm.nodes[1]
        .mem
        .put(Bytes::from(bytes))
        .await
        .expect("already there");
    let reads = farm.nodes[0].mem.reads.load(Ordering::SeqCst);
    assert_eq!(
        worker(&farm, 0).pass().await,
        Pass {
            settled: 1,
            waiting: 0
        }
    );
    assert_eq!(farm.nodes[0].mem.reads.load(Ordering::SeqCst), reads);
    assert!(owed_digests(&farm, 0).is_empty());
}

/// Catches a down owner called, or a debt settled against it: named down in the map, the
/// owner is not contacted (no stream opened on it), the debt stays, and when the map
/// names it up again the copy is made.
#[tokio::test]
async fn a_down_owner_is_waited_for() {
    let farm = Farm::start(2, ChainConfig::default()).await;
    let (_, digest) = owed_blob(&farm, 4096, 3).await;
    farm.nodes[1].switch.restore();
    farm.publish(&farm.map(2, 2, &[(1, MemberState::Down)]));
    let repairs = worker(&farm, 0);
    assert_eq!(
        repairs.pass().await,
        Pass {
            settled: 0,
            waiting: 1
        }
    );
    assert_eq!(farm.nodes[1].mem.knobs.begun.load(Ordering::SeqCst), 0);
    farm.publish(&farm.map(3, 2, &[]));
    assert_eq!(
        repairs.pass().await,
        Pass {
            settled: 1,
            waiting: 0
        }
    );
    assert!(held(&farm, 1, &digest));
}

/// Catches a repair aimed at the owner the record names instead of the owners now: the
/// replica of a debt is drained while it stands, so its slot goes to the third node, which
/// gets the copy and the debt is settled; and when this node is itself no longer an
/// owner, both owners get the copy before the debt is settled.
#[tokio::test]
async fn the_copy_goes_to_the_owners_the_map_names_now() {
    let farm = Farm::start(3, ChainConfig::default()).await;
    let first = farm.map(1, 2, &[]);
    let bytes = farm.blob(&first, 0, 1, 4096, 4);
    let digest = sha(&bytes);
    farm.nodes[1].switch.cut();
    farm.client(0, 1)
        .put(Bytes::from(bytes))
        .await
        .expect("answered");

    farm.publish(&farm.map(2, 2, &[(1, MemberState::Draining)]));
    let repairs = worker(&farm, 0);
    assert_eq!(
        repairs.pass().await,
        Pass {
            settled: 1,
            waiting: 0
        }
    );
    assert!(held(&farm, 2, &digest) && !held(&farm, 1, &digest));

    // Owed again, with this node out of the map's owners.
    let bytes = farm.blob(&first, 0, 1, 4096, 5);
    let digest = sha(&bytes);
    farm.publish(&first);
    farm.client(0, 1)
        .put(Bytes::from(bytes))
        .await
        .expect("answered");
    assert_eq!(owed_digests(&farm, 0), [digest]);
    farm.nodes[1].switch.restore();
    farm.publish(&farm.map(3, 2, &[(0, MemberState::Draining)]));
    assert_eq!(
        repairs.pass().await,
        Pass {
            settled: 1,
            waiting: 0
        }
    );
    assert!(held(&farm, 1, &digest) && held(&farm, 2, &digest));
}

/// Catches a debt kept for a blob that is gone, or a copy invented for it: the blob
/// is no longer in the store, so the debt is dropped and nothing is sent.
#[tokio::test]
async fn a_blob_no_longer_held_is_dropped_not_sent() {
    let farm = Farm::start(2, ChainConfig::default()).await;
    let (_, digest) = owed_blob(&farm, 4096, 6).await;
    farm.nodes[1].switch.restore();
    farm.nodes[0]
        .mem
        .blobs
        .lock()
        .expect("lock")
        .remove(&digest);
    assert_eq!(
        worker(&farm, 0).pass().await,
        Pass {
            settled: 1,
            waiting: 0
        }
    );
    assert!(!held(&farm, 1, &digest));
    assert!(owed_digests(&farm, 0).is_empty());
}

/// Catches a failure taken for a copy made: the node's own store failing to read, an owner
/// that refuses the copy part way (its store fails; a map newer than this node's), and a
/// node with no map: the debt stays owed.
#[tokio::test]
async fn a_failure_leaves_the_debt_owed() {
    let farm = Farm::start(2, ChainConfig::default()).await;
    let (_, digest) = owed_blob(&farm, 6 * MIB, 7).await;
    farm.nodes[1].switch.restore();
    let repairs = worker(&farm, 0);

    *farm.nodes[1].mem.knobs.fault.lock().expect("lock") = StreamFault::AppendFails;
    assert_eq!(
        repairs.pass().await,
        Pass {
            settled: 0,
            waiting: 1
        }
    );
    *farm.nodes[1].mem.knobs.fault.lock().expect("lock") = StreamFault::None;

    *farm.nodes[0].mem.fail.lock().expect("lock") = Some(StorageError::Unreachable(digest));
    assert_eq!(
        repairs.pass().await,
        Pass {
            settled: 0,
            waiting: 1
        }
    );
    *farm.nodes[0].mem.fail.lock().expect("lock") = None;

    farm.publish_to(1, &farm.map(2, 2, &[]));
    assert_eq!(
        repairs.pass().await,
        Pass {
            settled: 0,
            waiting: 1
        }
    );
    farm.publish_to(0, &farm.map(2, 2, &[]));

    let none = Arc::new(Chain::new(
        Held::none() as Arc<dyn MapSource>,
        Peers::plain(),
        Arc::clone(&farm.nodes[0].repairs),
        ChainConfig::default(),
    ));
    let lost = RepairWorker::new(
        Arc::clone(&farm.nodes[0].mem) as Arc<dyn Storage>,
        none,
        Arc::clone(&farm.nodes[0].repairs),
        config(),
    );
    assert_eq!(
        lost.pass().await,
        Pass {
            settled: 0,
            waiting: 1
        }
    );
    assert_eq!(owed_digests(&farm, 0), [digest]);

    // None of that was the copy: with everything well, it is made.
    assert_eq!(
        repairs.pass().await,
        Pass {
            settled: 1,
            waiting: 0
        }
    );
    assert!(held(&farm, 1, &digest));
}

/// Catches an owner at an address that cannot be dialled settled for: the debt waits.
#[tokio::test]
async fn an_owner_that_cannot_be_dialled_is_waited_for() {
    let farm = Farm::start(2, ChainConfig::default()).await;
    let (_, digest) = owed_blob(&farm, 4096, 8).await;
    let members = farm
        .nodes
        .iter()
        .enumerate()
        .map(|(i, node)| kbf_map::Member {
            id: node.id,
            addr: if i == 1 {
                "not an address".to_owned()
            } else {
                node.switch.addr.to_string()
            },
            zone: String::new(),
            weight: 1,
            state: MemberState::Active,
        })
        .collect();
    let map = Arc::new(
        kbf_map::Map::new(2, members, kbf_map::Rule::Rendezvous, 2, 1, Vec::new()).expect("map"),
    );
    farm.publish(&map);
    assert_eq!(
        worker(&farm, 0).pass().await,
        Pass {
            settled: 0,
            waiting: 1
        }
    );
    assert_eq!(owed_digests(&farm, 0), [digest]);
}

/// Catches a settle that fails taken for success: the copy is made but the journal cannot
/// write the settled frame, so the pass reports the debt as still owed (and so does the
/// journal); the debt is not forgotten.
#[tokio::test]
async fn a_debt_whose_settling_cannot_be_written_stays_owed() {
    let farm = Farm::start(2, ChainConfig::default()).await;
    let (_, digest) = owed_blob(&farm, 4096, 9).await;
    farm.nodes[1].switch.restore();
    let at = farm.nodes[0].fs.ops();
    farm.nodes[0].fs.fail_at(at, Fault::Error);
    assert_eq!(
        worker(&farm, 0).pass().await,
        Pass {
            settled: 0,
            waiting: 1
        }
    );
    assert_eq!(owed_digests(&farm, 0), [digest]);
    assert!(held(&farm, 1, &digest));
}

/// Catches debts lost to a restart: the primary's process dies and the machine loses power
/// with a debt owed; a new node over the same disk and store reads the journal, and its
/// worker makes the copy.
#[tokio::test]
async fn a_restarted_primary_repairs_what_it_owed() {
    let farm = Farm::start(2, ChainConfig::default()).await;
    let (_, digest) = owed_blob(&farm, 4096, 10).await;
    farm.nodes[1].switch.restore();
    let repairs = Arc::new(
        Repairs::open(
            farm.nodes[0].fs.restart().crash(0),
            Options::default(),
            Arc::new(crate::repair::NoLedger),
        )
        .expect("reopen"),
    );
    assert_eq!(repairs.open_records().len(), 1);
    let chain = Arc::new(Chain::new(
        Arc::clone(&farm.nodes[0].held) as Arc<dyn MapSource>,
        farm.peers(),
        Arc::clone(&repairs),
        ChainConfig::default(),
    ));
    let restarted = RepairWorker::new(
        Arc::clone(&farm.nodes[0].mem) as Arc<dyn Storage>,
        chain,
        Arc::clone(&repairs),
        config(),
    );
    assert_eq!(
        restarted.pass().await,
        Pass {
            settled: 1,
            waiting: 0
        }
    );
    assert!(held(&farm, 1, &digest));
}

/// Catches repairs without a bound: six debts and a replica that holds every append; with a
/// concurrency of two, no more than two copies are begun, and when it is released all six
/// are made.
#[tokio::test]
async fn at_most_the_concurrency_is_repaired_at_once() {
    let farm = Farm::start(2, ChainConfig::default()).await;
    let map = farm.map(1, 2, &[]);
    farm.nodes[1].switch.cut();
    let mut digests = Vec::new();
    for salt in 0..6 {
        let bytes = farm.blob(&map, 0, 1, 4096, 100 + salt);
        digests.push(sha(&bytes));
        farm.client(0, 1)
            .put(Bytes::from(bytes))
            .await
            .expect("answered");
    }
    farm.nodes[1].switch.restore();
    farm.nodes[1].mem.knobs.hold_appends(true);
    let repairs = Arc::new(worker(&farm, 0));
    let pass = {
        let repairs = Arc::clone(&repairs);
        tokio::spawn(async move { repairs.pass().await })
    };
    // Two copies begin; a third does not, however long it is given.
    while farm.nodes[1].mem.knobs.begun.load(Ordering::SeqCst) < 2 {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(farm.nodes[1].mem.knobs.begun.load(Ordering::SeqCst), 2);
    farm.nodes[1].mem.knobs.hold_appends(false);
    let done = within(pass).await.expect("joined");
    assert_eq!(
        done,
        Pass {
            settled: 6,
            waiting: 0
        }
    );
    for digest in &digests {
        assert!(held(&farm, 1, digest));
    }
}

/// Catches a default that would repair nothing, or everything at once: a pass every few
/// seconds, a few blobs at a time.
#[test]
fn the_default_pace_is_a_few_blobs_every_few_seconds() {
    let pace = RepairConfig::default();
    assert_eq!(pace.concurrency, 4);
    assert_eq!(pace.interval, Duration::from_secs(5));
    assert_eq!(pace.probe, Duration::from_secs(10));
}

/// Catches a worker that never runs on its own, or that does not tell the frontend what is
/// owed when it starts: `run` announces the debts (the frontend is told of the record
/// again, besides the first time), repairs on its ticks once the replica is back, and
/// returns when told to stop.
#[tokio::test]
async fn run_announces_repairs_on_every_tick_and_stops() {
    let farm = Farm::start(2, ChainConfig::default()).await;
    let (_, digest) = owed_blob(&farm, 4096, 11).await;
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let repairs = Arc::new(worker(&farm, 0));
    let running = {
        let repairs = Arc::clone(&repairs);
        tokio::spawn(async move {
            repairs
                .run(async {
                    let _ = stopped.await;
                })
                .await;
        })
    };
    for _ in 0..500 {
        if farm.nodes[0].ledger.owed.lock().expect("lock").len() >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(farm.nodes[0].ledger.owed.lock().expect("lock").len(), 2);
    farm.nodes[1].switch.restore();
    for _ in 0..500 {
        if owed_digests(&farm, 0).is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(owed_digests(&farm, 0).is_empty());
    assert!(held(&farm, 1, &digest));
    let _ = stop.send(());
    within(running).await.expect("run returns");
}

/// "Kill an owner mid-build and it finishes", to the end: after the build of the previous
/// test, the dead node returns and the map names every node up; the workers of the nodes
/// that took the writes make the copies, and then every blob is on both its owners and
/// nothing is owed anywhere.
#[tokio::test]
async fn an_owner_killed_mid_build_is_repaired_when_it_returns() {
    let (farm, written, _) = a_build_with_a_killed_owner().await;
    farm.nodes[1].switch.restore();
    let map = farm.map(3, 2, &[]);
    farm.publish(&map);
    for node in [0, 2] {
        let pass = worker(&farm, node).pass().await;
        assert_eq!(pass.waiting, 0, "node {node}");
    }
    for bytes in &written {
        let digest = sha(bytes);
        for id in map.write_owners(&digest) {
            let node = farm.nodes.iter().position(|n| n.id == id).expect("a node");
            assert!(
                held(&farm, node, &digest),
                "blob {digest} is not on node {node}"
            );
        }
    }
    for node in 0..3 {
        assert!(
            owed_digests(&farm, node).is_empty(),
            "node {node} still owes"
        );
    }
}

/// Catches a debt settled as "evicted" while its write is still committing: the replica is
/// cut, the primary's commit is held, so the debt is journalled (the write is not yet
/// answered) and the blob is not yet present here. A pass in that window reads NotFound, and
/// must leave the debt owed; once the commit is released the write is answered, the blob is
/// present with one copy, the debt is still owed, and the next pass (the replica back) makes
/// the second copy. Mutant: NotFound settles whatever is in flight.
#[tokio::test]
async fn a_debt_is_not_settled_while_its_write_is_committing() {
    let farm = Farm::start(2, ChainConfig::default()).await;
    let map = farm.map(1, 2, &[]);
    let bytes = farm.blob(&map, 0, 1, 4096, 20);
    let digest = sha(&bytes);
    farm.nodes[1].switch.cut();
    farm.nodes[0].mem.knobs.hold_commits();
    let write = tokio::spawn({
        let client = farm.client(0, 1);
        async move { client.put(Bytes::from(bytes)).await }
    });
    // The debt is journalled before the commit: wait for it, the commit is held.
    within(async {
        while owed_digests(&farm, 0).is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(!held(&farm, 0, &digest), "the commit is held");
    // The replica is back, so the pass gets as far as reading the blob here.
    farm.nodes[1].switch.restore();
    let repairs = worker(&farm, 0);
    assert_eq!(
        repairs.pass().await,
        Pass {
            settled: 0,
            waiting: 1
        }
    );
    assert_eq!(owed_digests(&farm, 0), [digest], "settled mid-commit");

    farm.nodes[0].mem.knobs.release_commits();
    assert_eq!(
        within(write).await.expect("joined").expect("answered"),
        digest
    );
    assert!(held(&farm, 0, &digest) && !held(&farm, 1, &digest));
    assert_eq!(owed_digests(&farm, 0), [digest]);
    assert_eq!(
        repairs.pass().await,
        Pass {
            settled: 1,
            waiting: 0
        }
    );
    assert!(held(&farm, 1, &digest));
}

/// A store whose first `read_range` of a blob answers as the store did when it was asked,
/// and only then lets the write that was committing finish: the worker read NotFound, and
/// the commit lands before the worker acts on it.
struct CommitsAfterTheRead {
    inner: Arc<Mem>,
    farm_repairs: Arc<Repairs>,
    first: std::sync::atomic::AtomicBool,
}

impl Storage for CommitsAfterTheRead {
    fn find_missing<'a>(&'a self, digests: &'a [Digest]) -> Answer<'a, Vec<Digest>> {
        self.inner.find_missing(digests)
    }

    fn read_blob<'a>(&'a self, digest: &'a Digest) -> Answer<'a, Bytes> {
        self.inner.read_blob(digest)
    }

    fn read_range<'a>(
        &'a self,
        digest: &'a Digest,
        offset: u64,
        limit: u64,
    ) -> Answer<'a, ReadStream> {
        Box::pin(async move {
            let answer = self.inner.read_range(digest, offset, limit).await;
            if self.first.swap(false, Ordering::SeqCst) {
                assert!(
                    matches!(answer, Err(StorageError::NotFound(_))),
                    "the commit is held, so the blob is not here yet"
                );
                // Step 3: the write commits and drops its Pending, after the read.
                self.inner.knobs.release_commits();
                within(async {
                    while self.farm_repairs.is_in_flight(digest) {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                })
                .await;
            }
            answer
        })
    }

    fn put(&self, bytes: Bytes) -> Answer<'_, Digest> {
        self.inner.put(bytes)
    }

    fn stream(&self, size: u64) -> Option<Box<dyn crate::PutStream>> {
        self.inner.stream(size)
    }

    fn segments(&self) -> Arc<dyn kbf_proto::worker::segments_server::Segments> {
        self.inner.segments()
    }

    fn action_result<'a>(
        &'a self,
        action: &'a Digest,
    ) -> Answer<'a, Option<(Digest, kbf_proto::reapi::ActionResult)>> {
        self.inner.action_result(action)
    }

    fn prepare_action_result<'a>(
        &'a self,
        result: &'a kbf_proto::reapi::ActionResult,
    ) -> Answer<'a, kbf_meta::ActionRecord> {
        self.inner.prepare_action_result(result)
    }

    fn commit_action_record(
        &self,
        role: kbf_meta::Role,
        action: Digest,
        record: kbf_meta::ActionRecord,
    ) -> Answer<'_, ()> {
        self.inner.commit_action_record(role, action, record)
    }
}

/// Catches a debt dropped for an absent blob that is present by the time it is dropped: the
/// write journals its debt (the replica is cut) with its commit held; the worker's pass has
/// begun counting the write (it began before the pass read `begun`, so the count is
/// unchanged), asked the replica, and read NotFound locally; then the write commits and
/// drops its in-flight mark; then the pass settles. The blob is held with one copy, so the
/// debt must stay, and the next pass makes the copy. Mutant: `settle_evicted` without the
/// look at the store after the in-flight test.
#[tokio::test]
async fn a_debt_is_not_dropped_for_a_blob_committed_after_the_worker_read_it_absent() {
    let farm = Farm::start(2, ChainConfig::default()).await;
    let map = farm.map(1, 2, &[]);
    let bytes = farm.blob(&map, 0, 1, 4096, 23);
    let digest = sha(&bytes);
    farm.nodes[1].switch.cut();
    farm.nodes[0].mem.knobs.hold_commits();
    let write = tokio::spawn({
        let client = farm.client(0, 1);
        async move { client.put(Bytes::from(bytes)).await }
    });
    // Step 1: the debt is journalled, the write is in flight, the commit is held.
    within(async {
        while owed_digests(&farm, 0).is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    farm.nodes[1].switch.restore();
    let storage = Arc::new(CommitsAfterTheRead {
        inner: Arc::clone(&farm.nodes[0].mem),
        farm_repairs: Arc::clone(&farm.nodes[0].repairs),
        first: std::sync::atomic::AtomicBool::new(true),
    });
    let repairs = RepairWorker::new(
        storage,
        Arc::clone(&farm.nodes[0].chain),
        Arc::clone(&farm.nodes[0].repairs),
        config(),
    );
    // Step 2 to 4: the pass reads begun, asks the replica, reads NotFound, the write
    // commits, the pass settles.
    let pass = within(repairs.pass()).await;
    assert_eq!(
        within(write).await.expect("joined").expect("answered"),
        digest
    );
    assert!(held(&farm, 0, &digest) && !held(&farm, 1, &digest));
    assert_eq!(
        owed_digests(&farm, 0),
        [digest],
        "a single copy and no debt (pass {pass:?})"
    );
    assert_eq!(
        within(repairs.pass()).await,
        Pass {
            settled: 1,
            waiting: 0
        }
    );
    assert!(held(&farm, 1, &digest));
}

/// Catches a debt kept for a blob this node never came to hold: the replica is cut and the
/// primary's commit fails, so the write fails after its debt was journalled. The blob is
/// not here and not in flight any more, so the next pass drops the debt (there is nothing
/// to copy) and sends nothing.
#[tokio::test]
async fn a_write_whose_commit_fails_leaves_no_debt() {
    let farm = Farm::start(2, ChainConfig::default()).await;
    let map = farm.map(1, 2, &[]);
    let bytes = farm.blob(&map, 0, 1, 4096, 21);
    let digest = sha(&bytes);
    farm.nodes[1].switch.cut();
    *farm.nodes[0].mem.knobs.fault.lock().expect("lock") = StreamFault::CommitFails;
    let answer = within(farm.client(0, 1).put(Bytes::from(bytes))).await;
    assert!(
        matches!(answer, Err(StorageError::Internal(_))),
        "{answer:?}"
    );
    assert!(!held(&farm, 0, &digest));
    assert_eq!(
        owed_digests(&farm, 0),
        [digest],
        "journalled before the commit"
    );
    farm.nodes[1].switch.restore();
    assert_eq!(
        worker(&farm, 0).pass().await,
        Pass {
            settled: 1,
            waiting: 0
        }
    );
    assert!(owed_digests(&farm, 0).is_empty());
    assert!(settled_told(&farm, 0, &digest).await);
    assert!(!held(&farm, 1, &digest));
    assert_eq!(farm.nodes[1].mem.knobs.begun.load(Ordering::SeqCst), 0);
}

/// Catches a pass that waits for ever on an owner that is up and does not answer: the
/// owner's `FindMissing` hangs, so the pass ends at the probe bound with the debt owed, and
/// a pass over the owner answering again settles it. (A hang deadline, not a speed.)
/// Mutant: the question asked without the bound.
#[tokio::test]
async fn an_owner_that_does_not_answer_is_waited_for_not_hung_on() {
    let farm = Farm::start(2, ChainConfig::default()).await;
    let (_, digest) = owed_blob(&farm, 4096, 22).await;
    farm.nodes[1].switch.restore();
    farm.nodes[1]
        .mem
        .knobs
        .hang_find_missing
        .store(true, Ordering::SeqCst);
    let repairs = RepairWorker::new(
        Arc::clone(&farm.nodes[0].mem) as Arc<dyn Storage>,
        Arc::clone(&farm.nodes[0].chain),
        Arc::clone(&farm.nodes[0].repairs),
        RepairConfig {
            probe: Duration::from_millis(300),
            ..config()
        },
    );
    let pass = within(repairs.pass()).await;
    assert_eq!(
        pass,
        Pass {
            settled: 0,
            waiting: 1
        }
    );
    assert!(farm.nodes[1].mem.knobs.asked.load(Ordering::SeqCst) >= 1);
    assert_eq!(owed_digests(&farm, 0), [digest]);
    farm.nodes[1]
        .mem
        .knobs
        .hang_find_missing
        .store(false, Ordering::SeqCst);
    let pass = within(repairs.pass()).await;
    assert_eq!(
        pass,
        Pass {
            settled: 1,
            waiting: 0
        }
    );
    assert!(held(&farm, 1, &digest));
}

/// Catches a worker that cannot be stopped while a pass is stuck: the owner's `FindMissing`
/// hangs and the probe bound is an hour, so only the stop can end `run`, and it does.
/// Mutant: `stop` polled only between passes.
#[tokio::test]
async fn run_stops_in_the_middle_of_a_stuck_pass() {
    let farm = Farm::start(2, ChainConfig::default()).await;
    let (_, digest) = owed_blob(&farm, 4096, 23).await;
    farm.nodes[1].switch.restore();
    farm.nodes[1]
        .mem
        .knobs
        .hang_find_missing
        .store(true, Ordering::SeqCst);
    let repairs = Arc::new(RepairWorker::new(
        Arc::clone(&farm.nodes[0].mem) as Arc<dyn Storage>,
        Arc::clone(&farm.nodes[0].chain),
        Arc::clone(&farm.nodes[0].repairs),
        RepairConfig {
            probe: Duration::from_secs(3600),
            ..config()
        },
    ));
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let running = tokio::spawn({
        let repairs = Arc::clone(&repairs);
        async move {
            repairs
                .run(async {
                    let _ = stopped.await;
                })
                .await;
        }
    });
    within(async {
        while farm.nodes[1].mem.knobs.asked.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    let _ = stop.send(());
    within(running).await.expect("run returns");
    assert_eq!(owed_digests(&farm, 0), [digest]);
}
