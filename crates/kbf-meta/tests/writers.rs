//! Node writers: `SetWriter`, and which objects a put and the other commands may name.

mod common;

use std::num::NonZeroU64;

use common::{ANY_INDEX, digest, meta};
use kbf_meta::{
    Applied, BlobWrite, Command, ForeignWriter, Location, MetaState, NodeWriter, ObjectId,
    StagedPut, StaleWriter, StoreId, TierError, TierId, UnreachableReason, Writer,
};

fn n(v: u64) -> NonZeroU64 {
    NonZeroU64::new(v).expect("not zero")
}

fn node(member: u64, boot: u64) -> NodeWriter {
    NodeWriter::new(n(member), n(boot))
}

fn object_of(writer: NodeWriter, seq: u64) -> ObjectId {
    ObjectId::of(Writer::Node(writer), seq)
}

fn at(object: ObjectId) -> Location {
    Location {
        store: StoreId::CONFIGURED,
        object,
        offset: 0,
    }
}

fn set(m: &mut MetaState, writer: NodeWriter) -> Applied {
    m.execute(ANY_INDEX, Command::SetWriter(writer))
}

fn put(m: &mut MetaState, d: u8, object: ObjectId) -> Applied {
    m.execute(
        ANY_INDEX,
        Command::PutBlob {
            digest: digest(d),
            location: at(object),
        },
    )
}

/// Catches: a put accepted for an object of another writer: another member, an earlier
/// boot of this member, a later boot nobody set, or a member never set at all, in each
/// of the three put commands, and a refused put that changes the state anyway. A node
/// that took another writer's object by a put could index keys it never wrote, and a
/// stale process (an earlier boot) could write keys the current one also numbers.
/// Mutant seen red: `writable` accepting any known node writer (an earlier boot then
/// puts).
#[test]
fn a_foreign_writer_put_is_refused() {
    let mut m = meta();
    let (old, current) = (node(7, 1), node(7, 2));
    assert_eq!(set(&mut m, old), Applied::Writer(Ok(())));
    assert_eq!(set(&mut m, current), Applied::Writer(Ok(())));
    assert_eq!(m.current_writer(), Some(current));
    assert_eq!(
        put(&mut m, 1, object_of(current, 1)),
        Applied::Blob(Ok(BlobWrite::Stored))
    );
    m.execute(
        ANY_INDEX,
        Command::AddTier {
            server: kbf_meta::ServerId::new(1),
            tier: TierId::new(1),
        },
    );
    let before = m.clone();
    for foreign in [
        object_of(old, 5),
        object_of(node(7, 3), 1),
        object_of(node(8, 1), 1),
        object_of(node(8, 2), 1),
    ] {
        assert_eq!(
            put(&mut m, 2, foreign),
            Applied::Blob(Err(ForeignWriter(foreign))),
            "{foreign}"
        );
        let batch = vec![
            (digest(3), at(object_of(current, 2))),
            (digest(4), at(foreign)),
        ];
        assert_eq!(
            m.execute(ANY_INDEX, Command::PutBlobs(batch)),
            Applied::Blobs(Err(ForeignWriter(foreign))),
            "{foreign}"
        );
        let staged = Command::PutStaged {
            tier: TierId::new(1),
            objects: vec![StagedPut {
                object: foreign,
                end: 10,
                seal: false,
                blobs: vec![(digest(5), 0)],
            }],
        };
        assert_eq!(
            m.execute(ANY_INDEX, staged),
            Applied::Staged(Err(TierError::Foreign(ForeignWriter(foreign)))),
            "{foreign}"
        );
        assert_eq!(m, before, "a refused put changed the state: {foreign}");
    }
}

/// Catches: an earlier boot's objects treated as unknown once a later boot is current
/// (its staged blocks could then never be flushed, nor its lost objects marked), and a
/// writer never set accepted by those commands. Mutant seen red: `known` comparing the
/// boot with `==` (the earlier boot's mark is refused).
#[test]
fn other_commands_name_any_boot_set_before_and_nothing_else() {
    let mut m = meta();
    assert_eq!(set(&mut m, node(7, 1)), Applied::Writer(Ok(())));
    assert_eq!(set(&mut m, node(7, 4)), Applied::Writer(Ok(())));
    for known in [object_of(node(7, 1), 1), object_of(node(7, 3), 1)] {
        let mark = Command::ObjectUnreachable {
            object: known,
            reason: UnreachableReason::Missing,
        };
        assert_eq!(
            m.execute(ANY_INDEX, mark),
            Applied::Marked(Ok(())),
            "{known}"
        );
        // Past the writer check: refused only because nothing staged it.
        assert_eq!(
            m.execute(ANY_INDEX, Command::SealStaged { object: known }),
            Applied::Tier(Err(TierError::NotStaged(known))),
            "{known}"
        );
    }
    for unknown in [object_of(node(7, 5), 1), object_of(node(9, 1), 1)] {
        let mark = Command::ObjectUnreachable {
            object: unknown,
            reason: UnreachableReason::Missing,
        };
        assert_eq!(
            m.execute(ANY_INDEX, mark),
            Applied::Marked(Err(ForeignWriter(unknown))),
            "{unknown}"
        );
        assert_eq!(
            m.execute(ANY_INDEX, Command::ObjectFlushed { object: unknown }),
            Applied::Tier(Err(TierError::Foreign(ForeignWriter(unknown)))),
            "{unknown}"
        );
    }
}

/// Catches: a `SetWriter` that takes a boot the log has set already, or an earlier
/// one, for the same member (two processes would then number objects under one
/// writer: the second overwrites the first's keys from seq 1), or that refuses a new
/// member. A refused one leaves the current writer as it was. Mutant seen red: the
/// check `<=` weakened to `<` (the same boot set twice is accepted).
#[test]
fn a_writer_is_set_once_and_only_forward() {
    let mut m = meta();
    assert_eq!(set(&mut m, node(7, 2)), Applied::Writer(Ok(())));
    let before = m.clone();
    for stale in [node(7, 2), node(7, 1)] {
        assert_eq!(
            set(&mut m, stale),
            Applied::Writer(Err(StaleWriter {
                writer: stale,
                current: n(2),
            }))
        );
        assert_eq!(m, before);
    }
    assert_eq!(set(&mut m, node(3, 1)), Applied::Writer(Ok(())));
    assert_eq!(m.current_writer(), Some(node(3, 1)));
    assert_eq!(
        put(&mut m, 1, object_of(node(7, 2), 1)),
        Applied::Blob(Err(ForeignWriter(object_of(node(7, 2), 1)))),
        "only the writer set last puts"
    );
    assert_eq!(
        set(&mut m, node(7, 2)),
        Applied::Writer(Err(StaleWriter {
            writer: node(7, 2),
            current: n(2),
        })),
        "a member's boots are remembered after another member is set"
    );
}

/// Catches: a node writer that disturbs the epoch writers: an allocated epoch's put is
/// still taken after `SetWriter`, and `AllocEpoch` still counts on.
#[test]
fn epoch_writers_are_unaffected_by_a_node_writer() {
    let mut m = meta();
    assert_eq!(set(&mut m, node(1, 1)), Applied::Writer(Ok(())));
    assert_eq!(
        put(&mut m, 1, ObjectId::new(common::EPOCH, 1)),
        Applied::Blob(Ok(BlobWrite::Stored))
    );
    assert_eq!(
        m.execute(ANY_INDEX, Command::AllocEpoch),
        Applied::Epoch(kbf_meta::Epoch::new(2))
    );
}

/// Catches: an ordering or "at or after" rule that mixes the kinds, or a node writer of
/// another member read as later (a tier would refuse to start over a block it should
/// treat as an orphan), and a display that drops the boot.
#[test]
fn writers_order_and_display() {
    let e = |v| Writer::Epoch(kbf_meta::Epoch::new(v));
    let w = |m, b| Writer::Node(node(m, b));
    assert!(e(u64::MAX) < w(1, 1));
    assert!(w(1, 9) < w(2, 1));
    assert!(e(3).at_or_after(e(3)) && e(4).at_or_after(e(3)) && !e(2).at_or_after(e(3)));
    assert!(w(7, 2).at_or_after(w(7, 2)) && w(7, 3).at_or_after(w(7, 2)));
    assert!(!w(7, 1).at_or_after(w(7, 2)) && !w(8, 5).at_or_after(w(7, 2)));
    assert!(!e(9).at_or_after(w(7, 2)) && !w(7, 2).at_or_after(e(1)));
    let id = ObjectId::of(w(7, 2), 3);
    assert_eq!((id.epoch(), id.seq(), id.writer()), (None, 3, w(7, 2)));
    assert_eq!(id.to_string(), "member 7 boot 2/3");
    assert_eq!(ObjectId::new(kbf_meta::Epoch::new(4), 5).to_string(), "4/5");
    let stale = StaleWriter {
        writer: node(7, 1),
        current: n(2),
    };
    assert_eq!(
        stale.to_string(),
        "member 7 boot 1 is not after boot 2, the latest set"
    );
    assert_eq!(
        ForeignWriter(id).to_string(),
        "object member 7 boot 2/3 is not of a writer this log takes for the command"
    );
}
