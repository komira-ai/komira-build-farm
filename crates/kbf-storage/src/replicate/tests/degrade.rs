//! A write whose replica cannot be reached: answered with one copy, and only with the copy
//! owed on the primary's disk.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use bytes::Bytes;
use kbf_map::MemberState;
use kbf_store::Fault;
use kbf_types::Digest;

use super::chain::{MIB, held, within};
use super::farm::Farm;
use crate::repair::{NoLedger, Options, RepairRecord, Repairs};
use crate::replicate::ChainConfig;
use crate::service::tests::fake::{StreamFault, sha};
use crate::{Storage, StorageError};

fn owed(farm: &Farm, node: usize) -> Vec<RepairRecord> {
    farm.nodes[node].repairs.open_records()
}

/// What node `node`'s journal says after a power cut: only what was synced.
fn after_a_power_cut(farm: &Farm, node: usize) -> Vec<RepairRecord> {
    Repairs::open(
        farm.nodes[node].fs.crash(0),
        Options::default(),
        Arc::new(NoLedger),
    )
    .expect("reopen the journal")
    .open_records()
}

/// Catches a one-copy write answered without a record, and a record that is not on the
/// disk when the writer is told: with the replica cut off, the write is answered, the
/// primary holds the blob, the replica does not, the primary owes exactly this copy
/// (naming the replica and the generation), and a power cut right after the answer
/// leaves the record. Mutants: the record not made; the journal's sync skipped.
#[tokio::test]
async fn an_answer_with_one_copy_comes_with_a_durable_record() {
    let farm = Farm::start(2, ChainConfig::default()).await;
    let map = farm.map(1, 2, &[]);
    let bytes = farm.blob(&map, 0, 1, 4096, 30);
    let digest = sha(&bytes);
    farm.nodes[1].switch.cut();
    let answered = within(farm.client(0, 1).put(Bytes::from(bytes)))
        .await
        .expect("answered");
    assert_eq!(answered, digest);
    assert!(held(&farm, 0, &digest) && !held(&farm, 1, &digest));
    let record = RepairRecord {
        digest,
        replica: farm.nodes[1].id,
        generation: 1,
    };
    assert_eq!(owed(&farm, 0), [record]);
    assert_eq!(after_a_power_cut(&farm, 0), [record]);
}

/// Catches an answer given when the record could not be made: the journal's sync fails, so
/// the write is not answered (UNAVAILABLE naming the journal) and the primary does not
/// hold the blob; the journal has stopped, so another one-copy write is refused too; and a
/// write whose replica is back still lands on both. Mutants: the failure of `degrade`
/// ignored; the commit before the record.
#[tokio::test]
async fn no_answer_is_given_when_the_record_cannot_be_made() {
    let farm = Farm::start(2, ChainConfig::default()).await;
    let map = farm.map(1, 2, &[]);
    farm.nodes[1].switch.cut();
    let first = farm.blob(&map, 0, 1, 4096, 31);
    // The journal appends (one operation) and then syncs (the next).
    let at = farm.nodes[0].fs.ops();
    farm.nodes[0].fs.fail_at(at + 1, Fault::Error);
    let answer = within(farm.client(0, 1).put(Bytes::from(first.clone()))).await;
    let Err(StorageError::Unavailable(why)) = answer else {
        panic!("{answer:?}");
    };
    assert!(why.to_string().contains("repair journal"), "{why}");
    assert!(!held(&farm, 0, &sha(&first)));
    assert!(owed(&farm, 0).is_empty());

    let second = farm.blob(&map, 0, 1, 4096, 32);
    let answer = within(farm.client(0, 1).put(Bytes::from(second.clone()))).await;
    let Err(StorageError::Unavailable(why)) = answer else {
        panic!("{answer:?}");
    };
    assert!(why.to_string().contains("stopped"), "{why}");
    assert!(!held(&farm, 0, &sha(&second)));

    farm.nodes[1].switch.restore();
    let third = farm.blob(&map, 0, 1, 4096, 33);
    farm.client(0, 1)
        .put(Bytes::from(third.clone()))
        .await
        .expect("both hold it");
    assert!(held(&farm, 0, &sha(&third)) && held(&farm, 1, &sha(&third)));
}

/// Catches debts that do not survive the primary restarting: three one-copy writes, then
/// the primary's process dies and the machine loses power; the reopened journal owes all
/// three, and a repair worker could start from it. Mutant: the journal's sync skipped.
#[tokio::test]
async fn the_records_survive_a_restart_of_the_primary() {
    let farm = Farm::start(2, ChainConfig::default()).await;
    let map = farm.map(1, 2, &[]);
    farm.nodes[1].switch.cut();
    let mut digests: Vec<Digest> = Vec::new();
    for salt in 40..43 {
        let bytes = farm.blob(&map, 0, 1, 2048, salt);
        digests.push(
            farm.client(0, 1)
                .put(Bytes::from(bytes))
                .await
                .expect("answered"),
        );
    }
    digests.sort();
    let restarted = Repairs::open(
        farm.nodes[0].fs.restart().crash(0),
        Options::default(),
        Arc::new(NoLedger),
    )
    .expect("reopen");
    let owing: Vec<Digest> = restarted.open_records().iter().map(|r| r.digest).collect();
    assert_eq!(owing, digests);
}

/// Catches a write abandoned because the replica died part way through the blob: the
/// replica's appends are held and its node is then cut off with the blob half sent; the
/// primary goes on receiving, holds the whole blob and owes the copy. Mutant: the loss
/// of the leg ends the write.
#[tokio::test]
async fn a_replica_that_dies_part_way_does_not_stop_the_write() {
    let farm = Farm::start(2, ChainConfig::default()).await;
    let map = farm.map(1, 2, &[]);
    let bytes = farm.blob(&map, 0, 1, 12 * MIB, 34);
    let digest = sha(&bytes);
    farm.nodes[1].mem.knobs.hold_appends(true);
    let client = farm.client(0, 1);
    let sent = bytes.clone();
    let write = tokio::spawn(async move { client.put(Bytes::from(sent)).await });
    while farm.nodes[0].mem.knobs.chunks.load(Ordering::SeqCst) < 3 {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    farm.nodes[1].switch.cut();
    farm.nodes[1].mem.knobs.hold_appends(false);
    let answered = within(write).await.expect("joined").expect("answered");
    assert_eq!(answered, digest);
    let blobs = farm.nodes[0].mem.blobs.lock().expect("lock");
    assert_eq!(blobs.get(&digest).map(|b| b.to_vec()), Some(bytes));
    drop(blobs);
    assert_eq!(owed(&farm, 0).len(), 1);
    assert!(!held(&farm, 1, &digest));
}

/// Catches a replica the map names down being called (a connection attempt that costs the
/// write the connect timeout) or its copy not being owed: with the replica down in the
/// map, the write is answered, no byte reaches it, and the record names the generation
/// that said so. Mutant: the check of the replica's state removed.
#[tokio::test]
async fn a_replica_the_map_names_down_is_owed_at_once() {
    let farm = Farm::start(2, ChainConfig::default()).await;
    let down = farm.map(2, 2, &[(1, MemberState::Down)]);
    farm.publish(&down);
    let bytes = farm.blob(&down, 0, 1, 4096, 35);
    let digest = farm
        .client(0, 2)
        .put(Bytes::from(bytes))
        .await
        .expect("answered");
    assert_eq!(farm.nodes[1].mem.knobs.chunks.load(Ordering::SeqCst), 0);
    assert_eq!(
        owed(&farm, 0),
        [RepairRecord {
            digest,
            replica: farm.nodes[1].id,
            generation: 2
        }]
    );
}

/// Catches a refusal treated as a replica that could not be reached: a replica holding a
/// newer map (STALE_MAP) and a replica whose store fails are the writer's answer, and
/// nothing is owed. Mutant: every replica error counts as unreachable.
#[tokio::test]
async fn a_refusal_is_not_a_copy_owed() {
    let farm = Farm::start(2, ChainConfig::default()).await;
    let map = farm.map(1, 2, &[]);
    farm.publish_to(1, &farm.map(2, 2, &[]));
    let bytes = farm.blob(&map, 0, 1, 4096, 36);
    let answer = farm.client(0, 1).put(Bytes::from(bytes)).await;
    assert!(
        matches!(answer, Err(StorageError::StaleMap { current: 2 })),
        "{answer:?}"
    );
    farm.publish(&map);
    *farm.nodes[1].mem.knobs.fault.lock().expect("lock") = StreamFault::CommitFails;
    let bytes = farm.blob(&map, 0, 1, 4096, 37);
    let answer = farm.client(0, 1).put(Bytes::from(bytes.clone())).await;
    assert!(
        matches!(answer, Err(StorageError::Internal(_))),
        "{answer:?}"
    );
    assert!(!held(&farm, 0, &sha(&bytes)));
    assert!(owed(&farm, 0).is_empty());
}

/// Catches a primary that waits for ever on a replica that accepts nothing or commits
/// nothing, and one that fails the write for it: after the bounds, the write is answered
/// with the copy owed (the replica may still commit later; the repair finds it there).
/// Mutants: the stall or the commit bound removed.
#[tokio::test]
async fn a_replica_that_stops_answering_is_owed_after_its_bounds() {
    let config = ChainConfig {
        stall: Duration::from_millis(300),
        commit: Duration::from_millis(300),
    };
    let farm = Farm::start(2, config).await;
    let map = farm.map(1, 2, &[]);
    let bytes = farm.blob(&map, 0, 1, 12 * MIB, 38);
    farm.nodes[1].mem.knobs.hold_appends(true);
    let digest = within(farm.client(0, 1).put(Bytes::from(bytes)))
        .await
        .expect("answered");
    assert!(held(&farm, 0, &digest) && !held(&farm, 1, &digest));
    farm.nodes[1].mem.knobs.hold_appends(false);

    let bytes = farm.blob(&map, 0, 1, MIB, 39);
    farm.nodes[1].mem.knobs.hold_commits();
    let second = within(farm.client(0, 1).put(Bytes::from(bytes)))
        .await
        .expect("answered");
    let digests: Vec<Digest> = owed(&farm, 0).iter().map(|r| r.digest).collect();
    assert!(
        digests.contains(&digest) && digests.contains(&second),
        "{digests:?}"
    );
}

/// Catches the frontend's ledger being waited for, or not told: a ledger that never
/// returns does not delay the answer, and it was told of the record.
/// Mutant: the report awaited before the answer.
#[tokio::test]
async fn the_frontend_is_told_but_never_waited_for() {
    let farm = Farm::start(2, ChainConfig::default()).await;
    let map = farm.map(1, 2, &[]);
    farm.nodes[0].ledger.block.store(true, Ordering::SeqCst);
    farm.nodes[1].switch.cut();
    let bytes = farm.blob(&map, 0, 1, 4096, 44);
    let digest = within(farm.client(0, 1).put(Bytes::from(bytes)))
        .await
        .expect("answered");
    for _ in 0..500 {
        if !farm.nodes[0].ledger.owed.lock().expect("lock").is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let told = farm.nodes[0].ledger.owed.lock().expect("lock").clone();
    assert_eq!(told.len(), 1);
    assert_eq!(told[0].digest, digest);
}

/// Catches the first owner being down leaving the blob unwritable or unowed: with the
/// primary down in the map, the next owner takes the write, holds it, and owes the primary
/// its copy; its own connection to the dead node is never made.
#[tokio::test]
async fn with_the_primary_down_the_next_owner_takes_the_write() {
    let farm = Farm::start(2, ChainConfig::default()).await;
    let map = farm.map(2, 2, &[(0, MemberState::Down)]);
    farm.publish(&map);
    let bytes = farm.blob(&map, 0, 1, 4096, 45);
    let digest = farm
        .client(1, 2)
        .put(Bytes::from(bytes))
        .await
        .expect("answered");
    assert!(held(&farm, 1, &digest) && !held(&farm, 0, &digest));
    assert_eq!(
        owed(&farm, 1),
        [RepairRecord {
            digest,
            replica: farm.nodes[0].id,
            generation: 2
        }]
    );
    assert_eq!(farm.nodes[0].mem.knobs.chunks.load(Ordering::SeqCst), 0);
}

/// Catches the second owner refusing a client write because the map has not marked the
/// first down yet (a frontend whose write to a dead owner failed goes on to the next
/// owner, `kbf_route::Router::put`, and gets INVALID_ARGUMENT "not the primary"): with the
/// first owner's connections cut and the map still naming it active, the write to the
/// second is answered, the second holds the blob, and it owes the first its copy.
/// Mutant: the plan's `(None, Some(1), ..)` arm guarded by the first owner being down.
#[tokio::test]
async fn the_second_owner_takes_a_write_whose_primary_is_dead_but_not_yet_down() {
    let farm = Farm::start(2, ChainConfig::default()).await;
    let map = farm.map(1, 2, &[]);
    let bytes = farm.blob(&map, 0, 1, 4096, 46);
    let digest = sha(&bytes);
    farm.nodes[0].switch.cut();
    let answered = within(farm.client(1, 1).put(Bytes::from(bytes)))
        .await
        .expect("answered");
    assert_eq!(answered, digest);
    assert!(held(&farm, 1, &digest) && !held(&farm, 0, &digest));
    assert_eq!(
        owed(&farm, 1),
        [RepairRecord {
            digest,
            replica: farm.nodes[0].id,
            generation: 1
        }]
    );
}

/// Catches the second owner taking a client write and keeping it to itself: with the
/// first owner up and the map naming it active, the write to the second lands on both and
/// nothing is owed. Mutant: the second owner's plan having no copy to send.
#[tokio::test]
async fn the_second_owner_takes_a_write_while_the_map_has_the_first_up() {
    let farm = Farm::start(2, ChainConfig::default()).await;
    let map = farm.map(1, 2, &[]);
    let bytes = farm.blob(&map, 0, 1, 4096, 47);
    let digest = sha(&bytes);
    let answered = within(farm.client(1, 1).put(Bytes::from(bytes)))
        .await
        .expect("answered");
    assert_eq!(answered, digest);
    assert!(held(&farm, 0, &digest) && held(&farm, 1, &digest));
    assert!(owed(&farm, 1).is_empty() && owed(&farm, 0).is_empty());
}

/// The build of [`an_owner_killed_mid_build_does_not_stop_it`]: 30 blobs written through
/// the owners the map names, the first live one as primary; a third of the way in node 1
/// dies (the map has not noticed), and two thirds in the map turns and names it down.
/// Writes whose primary was the dead node wait for the map and then go to the next owner.
/// Returns the farm, the blobs, and the final map.
pub(super) async fn a_build_with_a_killed_owner() -> (Farm, Vec<Vec<u8>>, Arc<kbf_map::Map>) {
    let farm = Farm::start(3, ChainConfig::default()).await;
    let mut map = farm.map(1, 2, &[]);
    let mut generation = 1;
    let mut pending: Vec<Vec<u8>> = Vec::new();
    let mut written: Vec<Vec<u8>> = Vec::new();
    let write = |map: &kbf_map::Map, generation: u64, bytes: Vec<u8>| {
        let owners = map.write_owners(&sha(&bytes));
        let live = owners
            .iter()
            .find(|id| map.member(**id).expect("a member").state != MemberState::Down)
            .copied()
            .expect("a live owner");
        let node = farm
            .nodes
            .iter()
            .position(|n| n.id == live)
            .expect("a node");
        let client = farm.client(node, generation);
        async move { client.put(Bytes::from(bytes)).await }
    };
    for i in 0..30u32 {
        if i == 10 {
            farm.nodes[1].switch.cut();
        }
        if i == 20 {
            generation = 2;
            map = farm.map(2, 2, &[(1, MemberState::Down)]);
            farm.publish(&map);
            for bytes in std::mem::take(&mut pending) {
                within(write(&map, generation, bytes.clone()))
                    .await
                    .expect("waited for the map, then written");
                written.push(bytes);
            }
        }
        let mut bytes = crate::service::tests::fake::patterned(3000 + i as usize);
        bytes[..4].copy_from_slice(&i.to_le_bytes());
        match within(write(&map, generation, bytes.clone())).await {
            Ok(_) => written.push(bytes),
            Err(StorageError::Unavailable(_)) if generation == 1 => pending.push(bytes),
            Err(other) => panic!("blob {i}: {other:?}"),
        }
    }
    (farm, written, map)
}

/// "Kill an owner mid-build and it finishes": every write of the build is answered in the
/// end, every blob is held by a live node, and every blob not on both its owners is owed
/// by a node that holds it.
#[tokio::test]
async fn an_owner_killed_mid_build_does_not_stop_it() {
    let (farm, written, map) = a_build_with_a_killed_owner().await;
    assert_eq!(written.len(), 30, "every write answered in the end");
    for bytes in &written {
        let digest = sha(bytes);
        let holders: Vec<usize> = (0..3).filter(|n| held(&farm, *n, &digest)).collect();
        assert!(
            holders.iter().any(|n| *n != 1),
            "blob {digest} is on no live node"
        );
        let both_owners = map
            .write_owners(&digest)
            .iter()
            .all(|id| holders.iter().any(|n| farm.nodes[*n].id == *id));
        let owed_by_a_holder = holders
            .iter()
            .any(|n| owed(&farm, *n).iter().any(|r| r.digest == digest));
        assert!(
            both_owners || owed_by_a_holder,
            "blob {digest} is not on both its owners and nobody owes the other copy"
        );
    }
}
