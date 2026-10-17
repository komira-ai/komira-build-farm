//! The format-3 encoding of `EvictObjects` (MetaCommand field 15).
//!
//! Catches:
//! - an encoding that changes under a committed log (golden bytes);
//! - a conversion that loses, reorders or repeats an object (a round trip over
//!   generated sets);
//! - `EvictObjects` written into, or read from, a format-1 or format-2 log: a reader
//!   without the command must refuse the entry, never skip it;
//! - the command numbered onto a field a format-2 reader knows, which it would then
//!   apply as another command;
//! - a set accepted out of order or with a repeat, which would give one value two
//!   encodings.

use std::collections::BTreeSet;

use kbf_log::{DecodeError, EncodeError, Format, LogCommand, decode, encode};
use kbf_meta::{Command, Epoch, ObjectId, ServerId, TierId};
use kbf_proto::kbf::log::v1 as proto;
use prost::Message;

fn object(epoch: u64, seq: u64) -> ObjectId {
    ObjectId::new(Epoch::new(epoch), seq)
}

fn evict(objects: &[(u64, u64)]) -> Command {
    Command::EvictObjects {
        objects: objects.iter().map(|&(e, s)| object(e, s)).collect(),
    }
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

/// `EvictObjects` values with their format-3 bytes written out from `log.proto`: a
/// `LogCommand` (`0a`) around a `MetaCommand` whose field is 15 (`7a`), around
/// `EvictObjects`, whose field 1 (`0a`) repeats an `ObjectId` { epoch (08), seq (10) }.
fn golden() -> Vec<(&'static str, Command, &'static str)> {
    vec![
        (
            "two objects",
            // Given out of order: the set encodes in id order, epoch first.
            evict(&[(3, 1), (2, 9)]),
            concat!(
                "0a0e", "7a0c", "0a04", "0802", "1009", "0a04", "0803", "1001"
            ),
        ),
        ("no object", evict(&[]), concat!("0a02", "7a00")),
    ]
}

/// Catches: the field renumbered, a type changed, the set written in another order;
/// and a decoder that disagrees with the bytes an older binary wrote.
#[test]
fn evict_objects_has_its_golden_bytes() {
    for (name, command, hex) in golden() {
        let bytes = hex::decode(hex).unwrap_or_else(|e| panic!("{name}: bad hex: {e}"));
        assert_eq!(
            enc(Format::V3, &command).map(hex::encode).as_deref(),
            Ok(hex),
            "{name}: encoding changed"
        );
        assert_eq!(
            dec(Format::V3, &bytes),
            Ok(command),
            "{name}: golden bytes decode differently"
        );
    }
}

/// xorshift64*, so a failing value is reproducible from its index.
struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d) % n
    }

    /// Small ids, so sets share epochs and differ only in seq; zero and `u64::MAX`
    /// now and then.
    fn id(&mut self) -> u64 {
        match self.below(10) {
            0 => 0,
            1 => u64::MAX,
            _ => self.below(5),
        }
    }
}

/// Catches: a conversion that drops an object, swaps epoch and seq, or truncates a
/// number; and an encoding that is not a function of the value.
#[test]
fn generated_evict_objects_round_trip_and_reencode_to_the_same_bytes() {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    for n in 0..20_000 {
        let objects: BTreeSet<ObjectId> = (0..rng.below(6))
            .map(|_| object(rng.id(), rng.id()))
            .collect();
        let command = Command::EvictObjects { objects };
        let bytes = enc(Format::V3, &command).expect("encode");
        let back =
            dec(Format::V3, &bytes).unwrap_or_else(|e| panic!("value {n}: {command:?}: {e}"));
        assert_eq!(back, command, "value {n}");
        assert_eq!(
            enc(Format::V3, &back).as_ref(),
            Ok(&bytes),
            "value {n}: {command:?} re-encoded differently"
        );
    }
}

/// Catches: `EvictObjects` written into a format-1 or format-2 log, and a reader of
/// either that skips it or reads it as something else, which would let a replica
/// without the command keep entries the others dropped. Mutant: `since_v3` answering
/// false (format 2 then takes the command).
#[test]
fn evict_refused_below_v3() {
    for (name, command, hex) in golden() {
        let bytes = hex::decode(hex).expect("hex");
        for format in [Format::V1, Format::V2] {
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
    // Format 3 still reads and writes the format-1 and format-2 commands, with their
    // bytes.
    let add_tier = Command::AddTier {
        server: ServerId::new(3),
        tier: TierId::new(7),
    };
    for (command, since) in [(Command::Collect, Format::V1), (add_tier, Format::V2)] {
        let bytes = enc(since, &command).expect("its own format");
        assert_eq!(enc(Format::V3, &command).as_ref(), Ok(&bytes));
        assert_eq!(dec(Format::V3, &bytes), Ok(command));
    }
}

/// `MetaCommand` as format 2 has it: fields 1 to 14 only, so prost on a binary built
/// before format 3 drops field 15 and leaves the `oneof` unset.
#[derive(Clone, PartialEq, Message)]
struct Format2MetaCommand {
    #[prost(
        oneof = "Format2Command",
        tags = "1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14"
    )]
    command: Option<Format2Command>,
}

#[derive(Clone, PartialEq, prost::Oneof)]
enum Format2Command {
    #[prost(message, tag = "1")]
    F1(proto::Tick),
    #[prost(message, tag = "2")]
    F2(proto::AllocEpoch),
    #[prost(message, tag = "3")]
    F3(proto::PutBlob),
    #[prost(message, tag = "4")]
    F4(proto::PutBlobs),
    #[prost(message, tag = "5")]
    F5(proto::PutAction),
    #[prost(message, tag = "6")]
    F6(proto::Touch),
    #[prost(message, tag = "7")]
    F7(proto::ObjectUnreachable),
    #[prost(message, tag = "8")]
    F8(proto::ObjectReachable),
    #[prost(message, tag = "9")]
    F9(proto::Collect),
    #[prost(message, tag = "10")]
    F10(proto::AddTier),
    #[prost(message, tag = "11")]
    F11(proto::PutStaged),
    #[prost(message, tag = "12")]
    F12(proto::SealStaged),
    #[prost(message, tag = "13")]
    F13(proto::ObjectFlushed),
    #[prost(message, tag = "14")]
    F14(proto::FlushConflict),
}

fn meta_bytes(command: &Command) -> Vec<u8> {
    let bytes = enc(Format::V3, command).expect("encode");
    let entry = proto::LogCommand::decode(bytes.as_slice()).expect("decode");
    let Some(proto::log_command::Command::Meta(meta)) = entry.command else {
        panic!("not a metadata command");
    };
    meta.encode_to_vec()
}

/// What a format-2 binary sees in an `EvictObjects` entry: no command (the `oneof`
/// unset). Catches the command numbered onto a field 1 to 14. The last format-2
/// command still decodes under the old shape, so the check is not vacuous. That the
/// decoder then refuses the entry is
/// `an_unset_meta_command_is_refused_under_every_format`.
#[test]
fn a_format_2_binary_sees_no_command_in_an_evict_entry() {
    for (name, command, _) in golden() {
        let old = Format2MetaCommand::decode(meta_bytes(&command).as_slice()).expect("decode");
        assert_eq!(old.command, None, "{name}");
    }
    let conflict = Command::FlushConflict {
        object: object(2, 9),
    };
    let old = Format2MetaCommand::decode(meta_bytes(&conflict).as_slice()).expect("decode");
    assert!(matches!(old.command, Some(Format2Command::F14(_))));
}

/// What a format-2 binary decodes from an `EvictObjects` entry, `kbf_log::decode`
/// refuses: the entry as that binary's prost leaves it (field 15 dropped, the
/// `MetaCommand` empty) is refused with `UnknownCommand("MetaCommand")` under every
/// format, by the unset-`oneof` check `meta::from_proto` has had since format 1. So a
/// format-2 node stops at the entry instead of skipping it. Catches: an unset command
/// decoded as a no-op (mutant: `from_proto` reads `None` as `Tick(0)`; the canonical
/// re-encode check then refuses it as `NotCanonical`, a second barrier, so the
/// assertion names the error). The raw entry under formats 1 and 2 is
/// `evict_refused_below_v3`.
#[test]
fn an_unset_meta_command_is_refused_under_every_format() {
    for (name, command, _) in golden() {
        let old = Format2MetaCommand::decode(meta_bytes(&command).as_slice()).expect("decode");
        let seen = proto::LogCommand {
            command: Some(proto::log_command::Command::Meta(
                proto::MetaCommand::decode(old.encode_to_vec().as_slice()).expect("decode"),
            )),
        }
        .encode_to_vec();
        assert_eq!(hex::encode(&seen), "0a00", "{name}");
        for format in [Format::V1, Format::V2, Format::V3] {
            assert_eq!(
                decode(format, &seen),
                Err(DecodeError::UnknownCommand("MetaCommand")),
                "{name} under {format:?}"
            );
        }
    }
}

fn wrap(objects: &[(u64, u64)]) -> Vec<u8> {
    let objects = objects
        .iter()
        .map(|&(epoch, seq)| proto::ObjectId {
            epoch,
            seq,
            ..proto::ObjectId::default()
        })
        .collect();
    proto::LogCommand {
        command: Some(proto::log_command::Command::Meta(proto::MetaCommand {
            command: Some(proto::meta_command::Command::EvictObjects(
                proto::EvictObjects { objects },
            )),
        })),
    }
    .encode_to_vec()
}

/// Catches a decoder that sorts or dedupes instead of refusing: the same set would then
/// have several encodings. Ordered by epoch first, so (1, 9) comes before (2, 1). The
/// ordered list decodes, so the cases are not vacuous.
#[test]
fn an_evict_set_out_of_order_or_repeated_is_refused() {
    assert_eq!(
        dec(Format::V3, &wrap(&[(1, 9), (2, 1)])),
        Ok(evict(&[(1, 9), (2, 1)]))
    );
    for (name, objects) in [
        ("seq out of order", [(1, 2), (1, 1)]),
        ("epoch out of order", [(2, 1), (1, 9)]),
        ("repeated", [(1, 1), (1, 1)]),
    ] {
        assert_eq!(
            dec(Format::V3, &wrap(&objects)),
            Err(DecodeError::SetOrder("EvictObjects.objects")),
            "{name}"
        );
    }
}
