//! The `kbf.log.v1` encoding of every `kbf_meta::Command`.
//!
//! Catches:
//! - an encoding that changes under a committed log: a field tag swapped or renumbered,
//!   a field's type changed, a command moved to another `oneof` number. The golden bytes
//!   below are the format-1 encoding of every variant; a log written by an older binary
//!   must decode to the same command on a newer one;
//! - a conversion that loses or mixes up a field (a round trip over generated values,
//!   both `decode(encode(x)) == x` and `encode(decode(b)) == b`);
//! - a decoder that reads an unknown or unspecified enum value as a default, or a
//!   command it does not know as nothing at all, instead of refusing it;
//! - a decoder that accepts an absent required field, a short hash, a store id past 16
//!   bits, or a set out of order (which would give one value two encodings);
//! - a decoder that accepts bytes `encode` would not write (an explicit zero, an
//!   unknown field, a varint that is not minimal, a repeated or reordered field);
//! - a format other than 1 written or read.

use kbf_log::{DecodeError, EncodeError, Format, LogCommand, decode, encode};
use kbf_meta::{
    ActionRecord, Closure, Command, Epoch, Generation, Location, ObjectId, Role, StoreId, Touch,
    UnreachableReason,
};
use kbf_proto::kbf::log::v1 as proto;
use kbf_types::{Digest, DigestFunction, FarmTime};
use prost::Message;

fn d(n: u8) -> Digest {
    Digest::new(DigestFunction::Sha256, [n; 32], u64::from(n))
}

fn object(epoch: u64, seq: u64) -> ObjectId {
    ObjectId::new(Epoch::new(epoch), seq)
}

fn location(store: u16, epoch: u64, seq: u64, offset: u64) -> Location {
    Location {
        store: StoreId::new(store),
        object: object(epoch, seq),
        offset,
    }
}

fn enc(command: &Command) -> Vec<u8> {
    encode(Format::V1, &LogCommand::Meta(command.clone())).expect("encode")
}

fn dec(bytes: &[u8]) -> Result<Command, DecodeError> {
    decode(Format::V1, bytes).map(|LogCommand::Meta(c)| c)
}

/// The bytes of a `Digest` message for `d(n)`, n < 128: function 1 (`08 01`), the
/// 32-byte hash (`12 20 ..`), the size (`18 n`); 38 (0x26) bytes in all.
macro_rules! digest_hex {
    (3) => {
        concat!(
            "0801",
            "1220",
            "0303030303030303030303030303030303030303030303030303030303030303",
            "1803"
        )
    };
    (4) => {
        concat!(
            "0801",
            "1220",
            "0404040404040404040404040404040404040404040404040404040404040404",
            "1804"
        )
    };
    (5) => {
        concat!(
            "0801",
            "1220",
            "0505050505050505050505050505050505050505050505050505050505050505",
            "1805"
        )
    };
}

/// Every variant, with its format-1 bytes, written out from `log.proto` (and checked
/// against an independent encoder when they were committed): a `LogCommand` (`0a`, field
/// 1, `meta`) around a `MetaCommand` whose field number is the variant's, around the
/// variant's message. Proto3 leaves zero values out.
fn golden() -> Vec<(&'static str, Command, &'static str)> {
    vec![
        (
            "Tick",
            Command::Tick(FarmTime::from_millis(1500)),
            // Tick (0a, field 1) { farm_time_ms: 1500 }
            concat!("0a05", "0a03", "08dc0b"),
        ),
        // alloc_epoch (12, field 2), empty
        ("AllocEpoch", Command::AllocEpoch, concat!("0a02", "1200")),
        (
            "PutBlob",
            Command::PutBlob {
                digest: d(3),
                location: location(1, 2, 7, 300),
            },
            // put_blob (1a) { digest (0a), location (12) { store 1, object (12)
            // { epoch 2, seq 7 }, offset 300 } }
            concat!(
                "0a37",
                "1a35",
                "0a26",
                digest_hex!(3),
                "120b",
                "0801",
                "1204",
                "0802",
                "1007",
                "18ac02"
            ),
        ),
        (
            "PutBlobs",
            Command::PutBlobs(vec![
                (d(3), location(0, 1, 0, 0)),
                (d(4), location(0, 1, 0, 100)),
            ]),
            // put_blobs (22) { blobs (0a) in order; store 0, seq 0 and offset 0 left out }
            concat!(
                "0a64",
                "2262",
                "0a2e",
                "0a26",
                digest_hex!(3),
                "1204",
                "1202",
                "0801",
                "0a30",
                "0a26",
                digest_hex!(4),
                "1206",
                "1202",
                "0801",
                "1864"
            ),
        ),
        (
            "PutAction",
            Command::PutAction {
                role: Role::Daemon,
                action: d(3),
                record: ActionRecord {
                    result: d(4),
                    closure: Closure::from_iter([d(5)]),
                },
            },
            // put_action (2a) { role DAEMON (08 01), action (12), record (1a)
            // { result (0a), closure (12) } }
            concat!(
                "0a7e",
                "2a7c",
                "0801",
                "1226",
                digest_hex!(3),
                "1a50",
                "0a26",
                digest_hex!(4),
                "1226",
                digest_hex!(5)
            ),
        ),
        (
            "Touch",
            Command::Touch(Touch {
                blobs: [d(3)].into(),
                actions: [d(4)].into(),
            }),
            // touch (32) { blobs (0a), actions (12) }
            concat!(
                "0a52",
                "3250",
                "0a26",
                digest_hex!(3),
                "1226",
                digest_hex!(4)
            ),
        ),
        (
            "ObjectUnreachable",
            Command::ObjectUnreachable {
                object: object(2, 9),
                reason: UnreachableReason::Corrupt,
            },
            // object_unreachable (3a) { object (0a) { 2, 9 }, reason CORRUPT (10 02) }
            concat!("0a0a", "3a08", "0a04", "0802", "1009", "1002"),
        ),
        (
            "ObjectReachable",
            Command::ObjectReachable {
                object: object(2, 9),
                generation: Generation::new(300),
            },
            // object_reachable (42) { object (0a) { 2, 9 }, generation 300 (10 ac 02) }
            concat!("0a0b", "4209", "0a04", "0802", "1009", "10ac02"),
        ),
        // collect (4a), empty
        ("Collect", Command::Collect, concat!("0a02", "4a00")),
    ]
}

/// Catches: a field tag swapped or renumbered, a type changed, a variant moved; and a
/// decoder that disagrees with the bytes an older binary wrote.
#[test]
fn every_variant_has_its_golden_bytes() {
    for (name, command, hex) in golden() {
        let bytes = hex::decode(hex).unwrap_or_else(|e| panic!("{name}: bad hex: {e}"));
        assert_eq!(hex::encode(enc(&command)), hex, "{name}: encoding changed");
        assert_eq!(
            dec(&bytes),
            Ok(command),
            "{name}: golden bytes decode differently"
        );
    }
}

/// Every `Command` variant is listed in the golden set, so a variant added to
/// `kbf_meta::Command` without golden bytes fails here (the match is exhaustive).
#[test]
fn the_golden_set_names_every_variant() {
    let mut seen = [false; 9];
    for (_, command, _) in golden() {
        let i = match command {
            Command::Tick(_) => 0,
            Command::AllocEpoch => 1,
            Command::PutBlob { .. } => 2,
            Command::PutBlobs(_) => 3,
            Command::PutAction { .. } => 4,
            Command::Touch(_) => 5,
            Command::ObjectUnreachable { .. } => 6,
            Command::ObjectReachable { .. } => 7,
            Command::Collect => 8,
        };
        seen[i] = true;
    }
    assert!(seen.iter().all(|s| *s), "{seen:?}");
}

/// xorshift64*, so a failing value is reproducible from its seed.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    /// Small values often (the zero values proto3 leaves out included), any u64 else.
    fn number(&mut self) -> u64 {
        match self.below(3) {
            0 => self.below(3),
            1 => self.below(1 << 14),
            _ => self.next(),
        }
    }

    fn digest(&mut self) -> Digest {
        let mut hash = [0u8; 32];
        for chunk in hash.chunks_mut(8) {
            chunk.copy_from_slice(&self.next().to_le_bytes()[..chunk.len()]);
        }
        // A few hashes only, so sets and batches repeat digests.
        if self.below(2) == 0 {
            hash = [u8::try_from(self.below(4)).expect("small"); 32];
        }
        Digest::new(DigestFunction::Sha256, hash, self.number())
    }

    fn object(&mut self) -> ObjectId {
        ObjectId::new(Epoch::new(self.number()), self.number())
    }

    fn location(&mut self) -> Location {
        Location {
            store: StoreId::new(u16::try_from(self.below(1 << 16)).expect("16 bits")),
            object: self.object(),
            offset: self.number(),
        }
    }

    fn command(&mut self) -> Command {
        match self.below(9) {
            0 => Command::Tick(FarmTime::from_millis(self.number())),
            1 => Command::AllocEpoch,
            2 => Command::PutBlob {
                digest: self.digest(),
                location: self.location(),
            },
            3 => Command::PutBlobs(
                (0..self.below(5))
                    .map(|_| (self.digest(), self.location()))
                    .collect(),
            ),
            4 => Command::PutAction {
                role: if self.below(2) == 0 {
                    Role::Daemon
                } else {
                    Role::Client
                },
                action: self.digest(),
                record: ActionRecord {
                    result: self.digest(),
                    closure: (0..self.below(5)).map(|_| self.digest()).collect(),
                },
            },
            5 => Command::Touch(Touch {
                blobs: (0..self.below(5)).map(|_| self.digest()).collect(),
                actions: (0..self.below(5)).map(|_| self.digest()).collect(),
            }),
            6 => Command::ObjectUnreachable {
                object: self.object(),
                reason: if self.below(2) == 0 {
                    UnreachableReason::Missing
                } else {
                    UnreachableReason::Corrupt
                },
            },
            7 => Command::ObjectReachable {
                object: self.object(),
                generation: Generation::new(self.number()),
            },
            _ => Command::Collect,
        }
    }
}

/// Catches: a conversion that drops, truncates or mixes up a field (a store id cut to 8
/// bits, epoch and seq swapped, a closure or touch set losing an element, a batch
/// reordered), and an encoding that is not a function of the value.
#[test]
fn generated_commands_round_trip_and_reencode_to_the_same_bytes() {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    for n in 0..20_000 {
        let command = rng.command();
        let bytes = enc(&command);
        let back = dec(&bytes).unwrap_or_else(|e| panic!("value {n}: {command:?}: {e}"));
        assert_eq!(back, command, "value {n}");
        assert_eq!(
            enc(&back),
            bytes,
            "value {n}: {command:?} re-encoded differently"
        );
    }
}

/// A `LogCommand` around `meta`, encoded.
fn wrap(meta: proto::meta_command::Command) -> Vec<u8> {
    proto::LogCommand {
        command: Some(proto::log_command::Command::Meta(proto::MetaCommand {
            command: Some(meta),
        })),
    }
    .encode_to_vec()
}

fn pd(n: u8) -> proto::Digest {
    proto::Digest {
        function: proto::DigestFunction::Sha256.into(),
        hash: vec![n; 32],
        size_bytes: u64::from(n),
    }
}

fn pobject() -> Option<proto::ObjectId> {
    Some(proto::ObjectId { epoch: 1, seq: 2 })
}

fn put_action(role: i32, function: i32) -> proto::meta_command::Command {
    proto::meta_command::Command::PutAction(proto::PutAction {
        role,
        action: Some(proto::Digest { function, ..pd(1) }),
        record: Some(proto::ActionRecord {
            result: Some(pd(2)),
            closure: Vec::new(),
        }),
    })
}

fn unreachable(reason: i32) -> proto::meta_command::Command {
    proto::meta_command::Command::ObjectUnreachable(proto::ObjectUnreachable {
        object: pobject(),
        reason,
    })
}

/// Catches: a decoder that reads an unspecified (zero) or unknown enum value as some
/// default (an unknown reason as `Missing`, an unknown role as `Daemon`) instead of
/// refusing the entry. Every enum field, at zero and at a value `log.proto` does not
/// name; and the named values still decode, so the check is not vacuous.
#[test]
fn an_unspecified_or_unknown_enum_value_is_refused() {
    for value in [0, 3, 99, -1] {
        assert_eq!(
            dec(&wrap(unreachable(value))),
            Err(DecodeError::UnknownValue {
                field: "ObjectUnreachable.reason",
                value,
            })
        );
        assert_eq!(
            dec(&wrap(put_action(value, 1))),
            Err(DecodeError::UnknownValue {
                field: "PutAction.role",
                value,
            })
        );
    }
    for value in [0, 2, 99, -1] {
        assert_eq!(
            dec(&wrap(put_action(1, value))),
            Err(DecodeError::UnknownValue {
                field: "PutAction.action",
                value,
            })
        );
    }
    assert!(dec(&wrap(unreachable(1))).is_ok());
    assert!(dec(&wrap(unreachable(2))).is_ok());
    assert!(dec(&wrap(put_action(1, 1))).is_ok());
    assert!(dec(&wrap(put_action(2, 1))).is_ok());
}

/// Catches: a decoder that skips a command it does not know (an unset `oneof`, which is
/// what prost leaves for a field number the file does not name) and applies nothing, or
/// some default, in its place. A replica that skipped an entry the others applied would
/// diverge silently.
#[test]
fn a_command_this_binary_does_not_know_is_refused() {
    // Field 15, length-delimited, empty: a command a later format might add.
    let unknown = [0x7a, 0x00];
    assert_eq!(
        dec(&unknown),
        Err(DecodeError::UnknownCommand("LogCommand"))
    );
    let mut meta = vec![0x0a, 0x02];
    meta.extend(unknown);
    assert_eq!(dec(&meta), Err(DecodeError::UnknownCommand("MetaCommand")));
    assert_eq!(dec(&[]), Err(DecodeError::UnknownCommand("LogCommand")));
    assert!(matches!(dec(&[0x0a, 0x05]), Err(DecodeError::Malformed(_))));
}

/// Catches: a required field read as a default when absent (an object id of 0/0, an
/// empty digest), a short or long hash padded or cut, a store id past 16 bits
/// truncated, and a set accepted out of order or with a repeat.
#[test]
fn malformed_values_are_refused() {
    use proto::meta_command::Command as P;
    let cases: Vec<(&str, P, DecodeError)> = vec![
        (
            "no location",
            P::PutBlob(proto::PutBlob {
                digest: Some(pd(1)),
                location: None,
            }),
            DecodeError::Missing("PutBlob.location"),
        ),
        (
            "no digest",
            P::PutBlob(proto::PutBlob {
                digest: None,
                location: Some(proto::Location {
                    store: 0,
                    object: pobject(),
                    offset: 0,
                }),
            }),
            DecodeError::Missing("PutBlob.digest"),
        ),
        (
            "no object in a location",
            P::PutBlobs(proto::PutBlobs {
                blobs: vec![proto::PutBlob {
                    digest: Some(pd(1)),
                    location: Some(proto::Location {
                        store: 0,
                        object: None,
                        offset: 0,
                    }),
                }],
            }),
            DecodeError::Missing("Location.object"),
        ),
        (
            "store past 16 bits",
            P::PutBlob(proto::PutBlob {
                digest: Some(pd(1)),
                location: Some(proto::Location {
                    store: 1 << 16,
                    object: pobject(),
                    offset: 0,
                }),
            }),
            DecodeError::Store(1 << 16),
        ),
        (
            "short hash",
            P::Touch(proto::Touch {
                blobs: vec![proto::Digest {
                    hash: vec![1; 31],
                    ..pd(1)
                }],
                actions: Vec::new(),
            }),
            DecodeError::HashLength {
                field: "Touch.blobs",
                len: 31,
            },
        ),
        (
            "long hash",
            P::Touch(proto::Touch {
                blobs: Vec::new(),
                actions: vec![proto::Digest {
                    hash: vec![1; 33],
                    ..pd(1)
                }],
            }),
            DecodeError::HashLength {
                field: "Touch.actions",
                len: 33,
            },
        ),
        (
            "touch out of order",
            P::Touch(proto::Touch {
                blobs: vec![pd(2), pd(1)],
                actions: Vec::new(),
            }),
            DecodeError::SetOrder("Touch.blobs"),
        ),
        (
            "touch repeats",
            P::Touch(proto::Touch {
                blobs: Vec::new(),
                actions: vec![pd(1), pd(1)],
            }),
            DecodeError::SetOrder("Touch.actions"),
        ),
        (
            "closure out of order",
            P::PutAction(proto::PutAction {
                role: proto::Role::Daemon.into(),
                action: Some(pd(1)),
                record: Some(proto::ActionRecord {
                    result: Some(pd(2)),
                    closure: vec![pd(4), pd(3)],
                }),
            }),
            DecodeError::SetOrder("ActionRecord.closure"),
        ),
        (
            "no record",
            P::PutAction(proto::PutAction {
                role: proto::Role::Daemon.into(),
                action: Some(pd(1)),
                record: None,
            }),
            DecodeError::Missing("PutAction.record"),
        ),
        (
            "no action",
            P::PutAction(proto::PutAction {
                role: proto::Role::Daemon.into(),
                action: None,
                record: Some(proto::ActionRecord {
                    result: Some(pd(2)),
                    closure: Vec::new(),
                }),
            }),
            DecodeError::Missing("PutAction.action"),
        ),
        (
            "no result",
            P::PutAction(proto::PutAction {
                role: proto::Role::Daemon.into(),
                action: Some(pd(1)),
                record: Some(proto::ActionRecord {
                    result: None,
                    closure: Vec::new(),
                }),
            }),
            DecodeError::Missing("ActionRecord.result"),
        ),
        (
            "no unreachable object",
            P::ObjectUnreachable(proto::ObjectUnreachable {
                object: None,
                reason: proto::UnreachableReason::Missing.into(),
            }),
            DecodeError::Missing("ObjectUnreachable.object"),
        ),
        (
            "no reachable object",
            P::ObjectReachable(proto::ObjectReachable {
                object: None,
                generation: 4,
            }),
            DecodeError::Missing("ObjectReachable.object"),
        ),
    ];
    for (name, command, error) in cases {
        assert_eq!(dec(&wrap(command)), Err(error), "{name}");
    }
}

/// Catches: an encoder or decoder that ignores the committed format, so a binary would
/// write or read an encoding the log does not allow.
#[test]
fn only_format_1_is_written_or_read() {
    let entry = LogCommand::Meta(Command::Collect);
    let bytes = encode(Format::V1, &entry).expect("format 1");
    for format in [Format::new(0), Format::new(2), Format::new(u32::MAX)] {
        assert_eq!(encode(format, &entry), Err(EncodeError::Format(format)));
        assert_eq!(decode(format, &bytes), Err(DecodeError::Format(format)));
    }
    assert_eq!(Format::V1.get(), 1);
    assert_eq!(decode(Format::new(1), &bytes), Ok(entry));
}

/// Catches: a decoder that accepts bytes `encode` would not write, so one value would
/// have two encodings and an entry would carry fields this binary ignores. prost on its
/// own accepts every case below and drops the unknown fields; each decodes to a value
/// whose canonical bytes are `canonical`, and each must be refused.
#[test]
fn bytes_encode_would_not_write_are_refused() {
    let tick = Command::Tick(FarmTime::from_millis(1500));
    let reachable = Command::ObjectReachable {
        object: object(2, 9),
        generation: Generation::new(300),
    };
    let cases: [(&str, &str, &Command, &str); 7] = [
        // Tick (0a) { farm_time_ms 0 written out (08 00) }
        (
            "an explicit zero",
            "0a040a020800",
            &Command::Tick(FarmTime::from_millis(0)),
            "0a020a00",
        ),
        // Tick { 1500, field 15 varint 1 (78 01) }
        (
            "an unknown field",
            "0a070a0508dc0b7801",
            &tick,
            "0a050a0308dc0b",
        ),
        // LogCommand { meta { tick { 1500 } }, field 15 varint 1 }
        (
            "an unknown top-level field",
            "0a050a0308dc0b7801",
            &tick,
            "0a050a0308dc0b",
        ),
        // Tick { 1500 as a four-byte varint (dc 8b 80 00) }
        (
            "a varint that is not minimal",
            "0a070a0508dc8b8000",
            &tick,
            "0a050a0308dc0b",
        ),
        // Tick { 1, then 1500 }: prost keeps the last
        (
            "a repeated field",
            "0a070a05080108dc0b",
            &tick,
            "0a050a0308dc0b",
        ),
        // ObjectReachable { generation, then object }
        (
            "fields out of order",
            "0a0b420910ac020a0408021009",
            &reachable,
            "0a0b42090a040802100910ac02",
        ),
        // ObjectId { seq, then epoch }
        (
            "nested fields out of order",
            "0a0b42090a041009080210ac02",
            &reachable,
            "0a0b42090a040802100910ac02",
        ),
    ];
    for (name, hex, value, canonical) in cases {
        let bytes = hex::decode(hex).expect("hex");
        assert_eq!(dec(&bytes), Err(DecodeError::NotCanonical), "{name}");
        // The canonical bytes of the same value still decode: the refusal is the
        // encoding's, not the value's.
        assert_eq!(hex::encode(enc(value)), canonical, "{name}");
        assert_eq!(
            dec(&hex::decode(canonical).expect("hex")).as_ref(),
            Ok(value),
            "{name}"
        );
    }
}
