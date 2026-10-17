//! The format-4 encoding of the rebuild of the index: `AdoptSegment`, `RebuildProgress`
//! and `RebuildDone` (MetaCommand fields 17 to 19), and an `ObjectId` that names an epoch
//! of another log (field 5, `log`).
//!
//! Catches:
//! - an encoding that changes under a committed log (golden bytes);
//! - a rebuild command written into, or read from, a log before format 4: a reader
//!   without it must refuse the entry;
//! - an object id that names no one writer (a log with no epoch, a log with a member), or
//!   a `RebuildProgress` with no key, accepted;
//! - a conversion that loses an entry, its offset, the log id or the epoch.

use std::num::NonZeroU64;

use kbf_log::{DecodeError, EncodeError, Format, LogCommand, decode, encode};
use kbf_meta::{Command, Epoch, LegacyWriter, NodeWriter, ObjectId, UnreachableReason, Writer};
use kbf_proto::kbf::log::v1 as proto;
use kbf_types::{Digest, DigestFunction};
use prost::Message;

fn enc(format: Format, command: &Command) -> Result<Vec<u8>, EncodeError> {
    encode(format, &LogCommand::Meta(command.clone()))
}

fn dec(format: Format, bytes: &[u8]) -> Result<Command, DecodeError> {
    match decode(format, bytes)? {
        LogCommand::Meta(c) => Ok(c),
        other => panic!("not a metadata command: {other:?}"),
    }
}

fn nz(n: u64) -> NonZeroU64 {
    NonZeroU64::new(n).expect("not zero")
}

fn legacy_object(log: u64, epoch: u64, seq: u64) -> ObjectId {
    ObjectId::of(
        Writer::Legacy(LegacyWriter::new(nz(log), Epoch::new(epoch))),
        seq,
    )
}

fn digest(byte: u8, size: u64) -> Digest {
    Digest::new(DigestFunction::Sha256, [byte; 32], size)
}

/// Values with their format-4 bytes written out from `log.proto`. A `LogCommand` is
/// `0a` around a `MetaCommand`. `RebuildDone` is field 19 (`9a01`, empty); `RebuildProgress`
/// field 18 (`9201`) around { after_key (`0a`) }; the mark is field 7 (`3a`) around
/// { object (`0a`) { epoch (08), seq (10), log (28) }, reason (10) }; `AdoptSegment` field 17
/// (`8a01`) around { object (`0a`) { seq (10), member (18), boot (20) }, entries (`12`),
/// each { digest (`0a`) { function (08), hash (12), size (18) }, offset (10) } }.
fn golden() -> Vec<(&'static str, Command, String)> {
    let hash = "ab".repeat(32);
    vec![
        (
            "rebuild done",
            Command::RebuildDone,
            "0a039a0100".to_owned(),
        ),
        (
            "rebuild progress",
            Command::RebuildProgress {
                after: "a".to_owned(),
            },
            concat!("0a06", "920103", "0a0161").to_owned(),
        ),
        (
            "another log's object marked missing",
            Command::ObjectUnreachable {
                object: legacy_object(9, 5, 3),
                reason: UnreachableReason::Missing,
            },
            concat!("0a0c", "3a0a", "0a06", "0805", "1003", "2809", "1001").to_owned(),
        ),
        (
            "adopt a segment",
            Command::AdoptSegment {
                object: ObjectId::of(Writer::Node(NodeWriter::new(nz(7), nz(2))), 3),
                entries: vec![(digest(0xab, 5), 7)],
            },
            [
                "0a37",
                "8a0134",
                "0a06100318072002",
                "122a",
                "0a26",
                "0801",
                "1220",
                &hash,
                "1805",
                "1007",
            ]
            .concat(),
        ),
    ]
}

/// Catches: fields 17 to 19 or the object's field 5 renumbered, a count or an offset
/// lost, and a decoder that disagrees with the bytes.
#[test]
fn rebuild_commands_have_their_golden_bytes() {
    for (name, command, hex) in golden() {
        let bytes = hex::decode(&hex).unwrap_or_else(|e| panic!("{name}: bad hex: {e}"));
        assert_eq!(
            enc(Format::V4, &command).map(hex::encode),
            Ok(hex.clone()),
            "{name}: encoding changed"
        );
        assert_eq!(dec(Format::V4, &bytes), Ok(command), "{name}");
    }
}

/// Catches: a rebuild command, or an object of another log, written into a log before
/// format 4, and a reader of one that takes it. Mutant seen red: `since_v4` leaving
/// `AdoptSegment` out (format 3 then encodes it, and an older replica reads it as
/// unknown fields).
#[test]
fn rebuild_commands_are_refused_below_v4() {
    for (name, command, hex) in golden() {
        let bytes = hex::decode(&hex).expect("hex");
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
}

fn wrap(command: proto::meta_command::Command) -> Vec<u8> {
    proto::LogCommand {
        command: Some(proto::log_command::Command::Meta(proto::MetaCommand {
            command: Some(command),
        })),
    }
    .encode_to_vec()
}

/// Catches: an object id naming a log without an epoch, a log and a member, or a log
/// and a boot, read as an epoch or a node (one value, two encodings; or an object no
/// writer owns), and a progress mark with no key (which would never move the resume
/// point). Each case is refused by name; the well-formed other-log id decodes, so the
/// cases are not vacuous.
#[test]
fn an_other_logs_object_id_names_one_writer_or_is_refused() {
    let id = |epoch, member, boot, log| proto::ObjectId {
        epoch,
        seq: 1,
        member,
        boot,
        log,
    };
    let mark = |object| {
        wrap(proto::meta_command::Command::ObjectUnreachable(
            proto::ObjectUnreachable {
                object: Some(object),
                reason: proto::UnreachableReason::Missing.into(),
            },
        ))
    };
    assert_eq!(
        dec(Format::V4, &mark(id(5, 0, 0, 9))),
        Ok(Command::ObjectUnreachable {
            object: legacy_object(9, 5, 1),
            reason: UnreachableReason::Missing,
        })
    );
    for (name, object) in [
        ("a log without an epoch", id(0, 0, 0, 9)),
        ("a log and a member", id(5, 7, 2, 9)),
        ("a log and a node", id(0, 7, 2, 9)),
        ("a log and a boot", id(5, 0, 2, 9)),
    ] {
        assert_eq!(
            dec(Format::V4, &mark(object)),
            Err(DecodeError::Writer("ObjectUnreachable.object")),
            "{name}"
        );
    }
    let none = wrap(proto::meta_command::Command::RebuildProgress(
        proto::RebuildProgress {
            after_key: String::new(),
        },
    ));
    assert_eq!(
        dec(Format::V4, &none),
        Err(DecodeError::Missing("RebuildProgress.after_key"))
    );
    let no_digest = wrap(proto::meta_command::Command::AdoptSegment(
        proto::AdoptSegment {
            object: Some(id(0, 7, 2, 0)),
            entries: vec![proto::AdoptedBlob {
                digest: None,
                offset: 1,
            }],
        },
    ));
    assert_eq!(
        dec(Format::V4, &no_digest),
        Err(DecodeError::Missing("AdoptedBlob.digest"))
    );
    let no_object = wrap(proto::meta_command::Command::AdoptSegment(
        proto::AdoptSegment {
            object: None,
            entries: Vec::new(),
        },
    ));
    assert_eq!(
        dec(Format::V4, &no_object),
        Err(DecodeError::Missing("AdoptSegment.object"))
    );
}

/// Catches: a conversion that reorders entries, drops one, swaps an offset, or reads a
/// segment of an epoch writer, a node and another log differently; the epoch and log at
/// their extremes, a digest listed twice kept.
#[test]
fn adopted_segments_round_trip_for_every_kind_of_writer() {
    let entries = vec![
        (digest(3, 10), 0),
        (digest(1, 20), u64::MAX),
        (digest(3, 10), 7),
        (digest(2, 0), 1),
    ];
    for object in [
        ObjectId::new(Epoch::new(u64::MAX), u64::MAX),
        ObjectId::of(Writer::Node(NodeWriter::new(nz(u64::MAX), nz(1))), 0),
        legacy_object(u64::MAX, u64::MAX, 4),
    ] {
        let command = Command::AdoptSegment {
            object,
            entries: entries.clone(),
        };
        let bytes = enc(Format::V4, &command).expect("encode");
        assert_eq!(dec(Format::V4, &bytes), Ok(command), "{object}");
    }
    let empty = Command::AdoptSegment {
        object: legacy_object(1, 1, 1),
        entries: Vec::new(),
    };
    let bytes = enc(Format::V4, &empty).expect("encode");
    assert_eq!(
        dec(Format::V4, &bytes),
        Ok(empty),
        "a segment of no records"
    );
}
