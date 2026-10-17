//! The format-4 encoding of node writers: `SetWriter` (MetaCommand field 16) and an
//! `ObjectId` that names a member and a boot (fields 3 and 4) instead of an epoch.
//!
//! Catches:
//! - an encoding that changes under a committed log (golden bytes);
//! - a node writer written into, or read from, a log before format 4: a reader without
//!   it must refuse the entry, never read the object as an epoch writer's;
//! - an object id that names both kinds, or half a node, accepted (it would have no
//!   one writer, or two encodings);
//! - a conversion that loses or swaps the member, the boot or the seq.

use std::num::NonZeroU64;

use kbf_log::{DecodeError, EncodeError, Format, LogCommand, MachineVersion, decode, encode};
use kbf_meta::{
    Command, Epoch, Location, NodeWriter, ObjectId, StoreId, UnreachableReason, Writer,
};
use kbf_proto::kbf::log::v1 as proto;
use prost::Message;

fn node(member: u64, boot: u64) -> NodeWriter {
    NodeWriter::new(
        NonZeroU64::new(member).expect("member"),
        NonZeroU64::new(boot).expect("boot"),
    )
}

fn node_object(member: u64, boot: u64, seq: u64) -> ObjectId {
    ObjectId::of(Writer::Node(node(member, boot)), seq)
}

fn enc(format: Format, command: &Command) -> Result<Vec<u8>, EncodeError> {
    encode(format, &LogCommand::Meta(command.clone()))
}

fn dec(format: Format, bytes: &[u8]) -> Result<Command, DecodeError> {
    match decode(format, bytes)? {
        LogCommand::Meta(c) => Ok(c),
        other => panic!("not a metadata command: {other:?}"),
    }
}

/// Values with their format-4 bytes written out from `log.proto`: `SetWriter` is a
/// `LogCommand` (`0a`) around a `MetaCommand` field 16 (`8201`) around { member (08),
/// boot (10) }; the mark is `MetaCommand` field 7 (`3a`) around { object (0a) {
/// seq (10), member (18), boot (20) }, reason (10) }, the epoch (zero) left out.
fn golden() -> Vec<(&'static str, Command, &'static str)> {
    vec![
        (
            "set writer",
            Command::SetWriter(node(7, 2)),
            concat!("0a07", "820104", "0807", "1002"),
        ),
        (
            "a node object marked missing",
            Command::ObjectUnreachable {
                object: node_object(7, 2, 3),
                reason: UnreachableReason::Missing,
            },
            concat!("0a0c", "3a0a", "0a06", "1003", "1807", "2002", "1001"),
        ),
    ]
}

/// Catches: field 16 or the object's fields 3 and 4 renumbered, the epoch written as an
/// explicit zero, and a decoder that disagrees with the bytes.
#[test]
fn node_writers_have_their_golden_bytes() {
    for (name, command, hex) in golden() {
        let bytes = hex::decode(hex).unwrap_or_else(|e| panic!("{name}: bad hex: {e}"));
        assert_eq!(
            enc(Format::V4, &command).map(hex::encode).as_deref(),
            Ok(hex),
            "{name}: encoding changed"
        );
        assert_eq!(dec(Format::V4, &bytes), Ok(command), "{name}");
    }
}

/// Catches: a node writer written into a log before format 4, and a reader of one that
/// takes it. Mutant seen red: `since_v4` answering false for a command that only names
/// a node object (format 3 then encodes the mark, and an older replica reads it as
/// unknown fields).
#[test]
fn node_writers_are_refused_below_v4() {
    for (name, command, hex) in golden() {
        let bytes = hex::decode(hex).expect("hex");
        for format in [Format::V1, Format::V2, Format::V3] {
            assert_eq!(
                enc(format, &command),
                Err(EncodeError::Command(format)),
                "{name} under {format:?}"
            );
            assert_eq!(
                dec(format, &bytes),
                Err(DecodeError::UnknownCommand("MetaCommand")),
                "{name} under {format:?}"
            );
        }
    }
    // An epoch writer's object keeps its bytes under every format, format 4 included.
    let epoch = Command::PutBlob {
        digest: kbf_types::Digest::new(kbf_types::DigestFunction::Sha256, [1; 32], 1),
        location: Location {
            store: StoreId::CONFIGURED,
            object: ObjectId::new(Epoch::new(3), 4),
            offset: 5,
        },
    };
    let v1 = enc(Format::V1, &epoch).expect("format 1");
    assert_eq!(enc(Format::V4, &epoch).as_ref(), Ok(&v1));
    assert_eq!(dec(Format::V4, &v1), Ok(epoch));
    assert_eq!(MachineVersion::V4.format(), Some(Format::V4));
}

fn mark(object: proto::ObjectId) -> Vec<u8> {
    proto::LogCommand {
        command: Some(proto::log_command::Command::Meta(proto::MetaCommand {
            command: Some(proto::meta_command::Command::ObjectUnreachable(
                proto::ObjectUnreachable {
                    object: Some(object),
                    reason: proto::UnreachableReason::Missing.into(),
                },
            )),
        })),
    }
    .encode_to_vec()
}

/// Catches: an object id that names an epoch and a node, or a node with a zero member
/// or boot, read as one or the other (one value, two encodings; or an object no writer
/// owns), and a `SetWriter` of a zero member or boot. The well-formed node id decodes,
/// so the cases are not vacuous.
#[test]
fn an_object_id_names_one_writer_or_is_refused() {
    let id = |epoch, member, boot| proto::ObjectId {
        epoch,
        seq: 1,
        member,
        boot,
        log: 0,
    };
    assert_eq!(
        dec(Format::V4, &mark(id(0, 7, 2))),
        Ok(Command::ObjectUnreachable {
            object: node_object(7, 2, 1),
            reason: UnreachableReason::Missing,
        })
    );
    for (name, object) in [
        ("an epoch and a node", id(3, 7, 2)),
        ("an epoch and a member", id(3, 7, 0)),
        ("a member without a boot", id(0, 7, 0)),
        ("a boot without a member", id(0, 0, 2)),
    ] {
        assert_eq!(
            dec(Format::V4, &mark(object)),
            Err(DecodeError::Writer("ObjectUnreachable.object")),
            "{name}"
        );
    }
    let evict = proto::LogCommand {
        command: Some(proto::log_command::Command::Meta(proto::MetaCommand {
            command: Some(proto::meta_command::Command::EvictObjects(
                proto::EvictObjects {
                    objects: vec![id(0, 7, 2), id(3, 7, 2)],
                },
            )),
        })),
    }
    .encode_to_vec();
    assert_eq!(
        dec(Format::V4, &evict),
        Err(DecodeError::Writer("EvictObjects.objects")),
        "a set with an object of no one writer"
    );
    for (name, member, boot, field) in [
        ("zero member", 0, 2, "SetWriter.member"),
        ("zero boot", 7, 0, "SetWriter.boot"),
    ] {
        let bytes = proto::LogCommand {
            command: Some(proto::log_command::Command::Meta(proto::MetaCommand {
                command: Some(proto::meta_command::Command::SetWriter(proto::SetWriter {
                    member,
                    boot,
                })),
            })),
        }
        .encode_to_vec();
        assert_eq!(
            dec(Format::V4, &bytes),
            Err(DecodeError::Missing(field)),
            "{name}"
        );
    }
}

/// Catches: a conversion that swaps member and boot, drops the seq, or reads a node
/// object in a set or a put differently from a mark; over every command that names an
/// object.
#[test]
fn node_objects_round_trip_in_every_command_that_names_one() {
    let a = node_object(u64::MAX, 1, 0);
    let b = node_object(1, u64::MAX, u64::MAX);
    let digest = kbf_types::Digest::new(kbf_types::DigestFunction::Sha256, [9; 32], 9);
    let location = |object| Location {
        store: StoreId::CONFIGURED,
        object,
        offset: 7,
    };
    for command in [
        Command::PutBlob {
            digest,
            location: location(a),
        },
        Command::PutBlobs(vec![
            (digest, location(ObjectId::new(Epoch::new(1), 1))),
            (digest, location(b)),
        ]),
        Command::PutStaged {
            tier: kbf_meta::TierId::new(1),
            objects: vec![kbf_meta::StagedPut {
                object: b,
                end: 20,
                seal: true,
                blobs: vec![(digest, 0)],
            }],
        },
        Command::ObjectReachable {
            object: a,
            generation: kbf_meta::Generation::new(4),
        },
        Command::SealStaged { object: a },
        Command::ObjectFlushed { object: b },
        Command::FlushConflict { object: a },
        Command::EvictObjects {
            objects: [ObjectId::new(Epoch::new(u64::MAX), 1), a, b]
                .into_iter()
                .collect(),
        },
    ] {
        let bytes = enc(Format::V4, &command).expect("encode");
        assert_eq!(
            dec(Format::V4, &bytes).as_ref(),
            Ok(&command),
            "{command:?}"
        );
        assert_eq!(
            enc(Format::V3, &command),
            Err(EncodeError::Command(Format::V3)),
            "{command:?}"
        );
    }
}
