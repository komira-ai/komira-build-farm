//! The rebuild of the index: `AdoptSegment`, `RebuildProgress` and `RebuildDone`.

mod common;

use std::num::NonZeroU64;

use common::{ANY_INDEX, digest, meta, object};
use kbf_meta::{
    AdoptRefused, Adopted, Applied, BlobAnswer, BlobWrite, Command, Epoch, LegacyWriter, Location,
    MetaState, NodeWriter, ObjectId, Rebuild, StoreId, UnreachableReason, Writer,
};

fn n(v: u64) -> NonZeroU64 {
    NonZeroU64::new(v).expect("not zero")
}

fn node(member: u64, boot: u64) -> NodeWriter {
    NodeWriter::new(n(member), n(boot))
}

fn node_object(member: u64, boot: u64, seq: u64) -> ObjectId {
    ObjectId::of(Writer::Node(node(member, boot)), seq)
}

fn legacy_object(log: u64, epoch: u64, seq: u64) -> ObjectId {
    let writer = LegacyWriter::new(n(log), Epoch::new(epoch));
    ObjectId::of(Writer::Legacy(writer), seq)
}

fn adopt(m: &mut MetaState, object: ObjectId, entries: &[(u8, u64)]) -> Applied {
    let entries = entries.iter().map(|&(d, o)| (digest(d), o)).collect();
    m.execute(ANY_INDEX, Command::AdoptSegment { object, entries })
}

fn adopted(inserted: u64, kept: u64) -> Applied {
    Applied::Adopted(Ok(Adopted { inserted, kept }))
}

fn present_at(m: &MetaState, d: u8) -> Option<Location> {
    match m.blob(&digest(d)) {
        BlobAnswer::Present { location, .. } => Some(location),
        BlobAnswer::Unavailable | BlobAnswer::Absent => None,
    }
}

/// Catches: an adopted record that does not read back at the object and offset the footer
/// gave, in every kind of writer's object; a digest listed twice in one segment indexed at
/// its second copy.
#[test]
fn adopted_records_are_indexed_at_their_object_and_offset() {
    let mut m = meta();
    let objects = [
        node_object(7, 1, 3),
        legacy_object(9, 4, 1),
        // An epoch this log allocated: the epoch writer of an earlier start.
        object(2),
    ];
    // Epoch 2 must exist for `object(2)`.
    assert_eq!(
        m.execute(ANY_INDEX, Command::AllocEpoch),
        Applied::Epoch(Epoch::new(2))
    );
    for (i, object) in objects.into_iter().enumerate() {
        let d = 10 + i as u8;
        // The digest listed twice keeps its first offset.
        assert_eq!(
            adopt(&mut m, object, &[(d, 100), (d, 200), (d + 10, 300)]),
            adopted(2, 1),
            "{object}"
        );
        assert_eq!(
            present_at(&m, d),
            Some(Location {
                store: StoreId::CONFIGURED,
                object,
                offset: 100
            }),
            "{object}"
        );
        assert_eq!(present_at(&m, d + 10).map(|l| l.offset), Some(300));
    }
}

/// Catches: an adoption that replaces an entry already indexed, which would point a blob
/// written while the rebuild ran (or one a read healed) back at an older copy: a blob held
/// at a reachable object, and one held at an unreachable one, are both left as they are,
/// and counted as kept. Mutant seen red: `adopt_segment` inserting unconditionally.
#[test]
fn adopt_never_replaces_an_entry_already_indexed() {
    let mut m = meta();
    let current = node(7, 5);
    m.execute(ANY_INDEX, Command::SetWriter(current));
    let (mine, lost) = (node_object(7, 5, 1), node_object(7, 5, 2));
    let put = |d, object| Command::PutBlob {
        digest: digest(d),
        location: Location {
            store: StoreId::CONFIGURED,
            object,
            offset: 8,
        },
    };
    for (d, object) in [(1, mine), (2, lost)] {
        assert_eq!(
            m.execute(ANY_INDEX, put(d, object)),
            Applied::Blob(Ok(BlobWrite::Stored))
        );
    }
    // Digest 2's object is lost: held but unavailable.
    m.execute(
        ANY_INDEX,
        Command::ObjectUnreachable {
            object: lost,
            reason: UnreachableReason::Missing,
        },
    );
    let old = node_object(7, 2, 9);
    assert_eq!(
        adopt(&mut m, old, &[(1, 50), (2, 60), (3, 70)]),
        adopted(1, 2)
    );
    assert_eq!(present_at(&m, 1).map(|l| l.object), Some(mine));
    assert_eq!(m.blob(&digest(2)), BlobAnswer::Unavailable);
    assert_eq!(present_at(&m, 3).map(|l| l.object), Some(old));
}

/// Catches: a rebuild that indexes the objects its own node is writing (a race with its
/// own puts, whose commit then reads `Duplicate` or fights the adoption): an object of the
/// current node writer is refused and changes nothing; an object of an earlier boot of the
/// same member, or of another member, is taken. Mutant seen red: no current-writer check.
#[test]
fn adopt_is_refused_for_the_current_writer() {
    let mut m = meta();
    m.execute(ANY_INDEX, Command::SetWriter(node(7, 2)));
    let before = m.clone();
    let own = node_object(7, 2, 4);
    assert_eq!(
        adopt(&mut m, own, &[(1, 0)]),
        Applied::Adopted(Err(AdoptRefused::CurrentWriter(own)))
    );
    assert_eq!(m, before, "a refused adoption changes nothing");
    assert_eq!(
        adopt(&mut m, node_object(7, 1, 4), &[(1, 0)]),
        adopted(1, 0)
    );
    assert_eq!(
        adopt(&mut m, node_object(8, 2, 4), &[(2, 0)]),
        adopted(1, 0)
    );
    // Once a later boot is current, the earlier one's objects are adoptable.
    m.execute(ANY_INDEX, Command::SetWriter(node(7, 3)));
    assert_eq!(adopt(&mut m, own, &[(3, 0)]), adopted(1, 0));
}

/// Catches: an adoption of an object of an epoch this log never allocated (epoch zero, or
/// one past the last): a later `AllocEpoch` would hand its writer out again, and two keys
/// would be named alike. Refused, and changes nothing. Mutant seen red: the epoch check
/// removed.
#[test]
fn adopt_is_refused_for_an_unallocated_epoch() {
    let mut m = meta();
    let before = m.clone();
    for epoch in [0, 2, u64::MAX] {
        let object = ObjectId::new(Epoch::new(epoch), 1);
        assert_eq!(
            adopt(&mut m, object, &[(1, 0)]),
            Applied::Adopted(Err(AdoptRefused::UnallocatedEpoch(object))),
            "epoch {epoch}"
        );
    }
    assert_eq!(m, before);
}

/// Catches: an adopted object whose writer the log then refuses to hear about: a read
/// that fails on it cannot be marked unreachable (the blob would be served as present for
/// ever), and its loss cannot be forgotten. Both are accepted after an adoption, and
/// refused before one. Mutant seen red: `adopted_writers` never filled.
#[test]
fn an_adopted_objects_writer_is_known_afterwards() {
    let mut m = meta();
    let lost = |object| Command::ObjectUnreachable {
        object,
        reason: UnreachableReason::Missing,
    };
    for (i, object) in [node_object(8, 1, 1), legacy_object(5, 1, 1)]
        .into_iter()
        .enumerate()
    {
        let d = i as u8 + 1;
        assert!(
            matches!(m.execute(ANY_INDEX, lost(object)), Applied::Marked(Err(_))),
            "{object} before"
        );
        assert_eq!(adopt(&mut m, object, &[(d, 0)]), adopted(1, 0));
        assert_eq!(m.execute(ANY_INDEX, lost(object)), Applied::Marked(Ok(())));
        assert_eq!(m.unreachable(object), Some(UnreachableReason::Missing));
    }
    for d in [1, 2] {
        assert_eq!(m.blob(&digest(d)), BlobAnswer::Unavailable);
    }
}

/// Catches: a put of another log's or another node's object accepted because an adoption
/// made the writer known (adoption is the only way in): the put stays refused.
#[test]
fn a_put_of_an_adopted_writer_is_still_refused() {
    let mut m = meta();
    m.execute(ANY_INDEX, Command::SetWriter(node(7, 2)));
    let object = legacy_object(5, 1, 1);
    adopt(&mut m, object, &[(1, 0)]);
    let put = m.execute(
        ANY_INDEX,
        Command::PutBlob {
            digest: digest(9),
            location: Location {
                store: StoreId::CONFIGURED,
                object,
                offset: 0,
            },
        },
    );
    assert!(matches!(put, Applied::Blob(Err(_))), "{put:?}");
}

/// Catches: progress that goes backwards (a retried older batch would send the resume
/// point back and a restart would re-read what it had done), that is recorded after the
/// rebuild is done (a resume of a finished rebuild), or an empty key accepted as a resume
/// point. And that a log that never started is `Pending` from the beginning, that
/// `RebuildDone` is final, and that a done rebuild still takes adoptions.
#[test]
fn progress_moves_forward_and_done_is_final() {
    let mut m = meta();
    assert_eq!(m.rebuild(), Rebuild::Pending { after: None });
    let progress = |m: &mut MetaState, key: &str| {
        m.execute(
            ANY_INDEX,
            Command::RebuildProgress {
                after: key.to_owned(),
            },
        )
    };
    let pending = |key: &str| {
        Applied::Rebuild(Rebuild::Pending {
            after: Some(key.to_owned()),
        })
    };
    assert_eq!(progress(&mut m, "kbf/cas/b"), pending("kbf/cas/b"));
    assert_eq!(
        progress(&mut m, "kbf/cas/a"),
        pending("kbf/cas/b"),
        "backwards"
    );
    assert_eq!(
        progress(&mut m, "kbf/cas/b"),
        pending("kbf/cas/b"),
        "the same"
    );
    assert_eq!(progress(&mut m, ""), pending("kbf/cas/b"), "empty");
    assert_eq!(progress(&mut m, "kbf/cas/c"), pending("kbf/cas/c"));
    assert_eq!(
        m.execute(ANY_INDEX, Command::RebuildDone),
        Applied::Rebuild(Rebuild::Done)
    );
    assert_eq!(
        progress(&mut m, "kbf/cas/z"),
        Applied::Rebuild(Rebuild::Done)
    );
    assert_eq!(m.rebuild(), Rebuild::Done);
    assert_eq!(
        adopt(&mut m, node_object(8, 1, 1), &[(1, 0)]),
        adopted(1, 0)
    );
    assert!(m.rebuild().is_done());
}

/// Catches: a rebuild state that a copy of the state machine (a snapshot, a replica)
/// does not carry: two replicas that applied the same commands are equal, and differ from
/// one that did not apply them.
#[test]
fn replicas_agree_on_the_rebuild() {
    let (mut a, mut b) = (meta(), meta());
    let commands = [
        Command::AdoptSegment {
            object: legacy_object(5, 1, 1),
            entries: vec![(digest(1), 0)],
        },
        Command::RebuildProgress {
            after: "k".to_owned(),
        },
        Command::RebuildDone,
    ];
    for command in commands {
        a.execute(ANY_INDEX, command.clone());
        assert_ne!(a, b, "{command:?}");
        b.execute(ANY_INDEX, command.clone());
        assert_eq!(a, b, "{command:?}");
    }
    assert_eq!(a, b);
}

/// Catches: an object of another log's epoch that reads as an epoch writer's or a node's
/// (the same epoch number in two logs would be one object), a display that drops the log,
/// an ordering that moves it among the other kinds (an `EvictObjects` set is encoded in
/// this order), and a writer of another log that "starts after" a current writer (a tier
/// would refuse to start over its blocks).
#[test]
fn another_logs_writer_is_named_by_its_log_and_never_starts_after_anyone() {
    let id = legacy_object(9, 4, 1);
    assert_eq!(id.to_string(), "log 0000000000000009 epoch 4/1");
    assert_eq!(id.epoch(), None, "it has no epoch of this log");
    assert_ne!(id, legacy_object(10, 4, 1));
    assert_ne!(id, ObjectId::new(Epoch::new(4), 1));
    let (epoch, node, other) = (
        Writer::Epoch(Epoch::new(u64::MAX)),
        Writer::Node(node(1, 1)),
        id.writer(),
    );
    assert!(epoch < node && node < other);
    for current in [epoch, node, other] {
        assert!(!other.at_or_after(current), "{current}");
    }
    assert!(!epoch.at_or_after(other));
    assert!(!node.at_or_after(other));
}
