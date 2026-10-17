//! The format-2 encoding of the write tier's metadata commands (docs/design/write-path.md,
//! section 6.5): `AddTier`, `PutStaged`, `SealStaged`, `ObjectFlushed`, `FlushConflict`.
//!
//! Catches:
//! - an encoding that changes under a committed log (golden bytes per variant);
//! - a conversion that loses or mixes up a field (a round trip over generated values);
//! - a tier command written into, or read from, a format-1 log: a reader without the
//!   commands must refuse the entry, never skip it (section 9, "a log holding each new
//!   variant, opened by a reader without it, is refused");
//! - a tier command numbered onto a field an older reader knows, which it would then
//!   apply as another command;
//! - a required field read as a default when absent.

use kbf_log::{DecodeError, EncodeError, Format, LogCommand, decode, encode};
use kbf_meta::{Command, Epoch, ObjectId, ServerId, StagedPut, TierId};
use kbf_proto::kbf::log::v1 as proto;
use kbf_types::{Digest, DigestFunction};
use prost::Message;

fn d(n: u8) -> Digest {
    Digest::new(DigestFunction::Sha256, [n; 32], u64::from(n))
}

fn object(epoch: u64, seq: u64) -> ObjectId {
    ObjectId::new(Epoch::new(epoch), seq)
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

/// The bytes of a `Digest` message for `d(n)`, n < 128, as in `meta_codec.rs`.
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
}

/// Every tier command, with its format-2 bytes written out from `log.proto`: a
/// `LogCommand` (`0a`) around a `MetaCommand` whose field is the variant's (10 to 14:
/// `52`, `5a`, `62`, `6a`, `72`), around the variant's message.
fn golden() -> Vec<(&'static str, Command, &'static str)> {
    vec![
        (
            "AddTier",
            Command::AddTier {
                server: ServerId::new(3),
                tier: TierId::new(7),
            },
            // add_tier (52) { server 3, tier 7 }
            concat!("0a06", "5204", "0803", "1007"),
        ),
        (
            "PutStaged",
            Command::PutStaged {
                tier: TierId::new(7),
                objects: vec![StagedPut {
                    object: object(2, 9),
                    end: 300,
                    seal: true,
                    blobs: vec![(d(3), 0), (d(4), 100)],
                }],
            },
            // put_staged (5a) { tier 7, objects (12) { object (0a) { 2, 9 }, end 300,
            // seal (18 01), blobs (22) { digest d(3) } (offset 0 left out), blobs (22)
            // { digest d(4), offset 100 (10 64) } } }
            concat!(
                "0a67",
                "5a65",
                "0807",
                "1261",
                "0a04",
                "0802",
                "1009",
                "10ac02",
                "1801",
                "2228",
                "0a26",
                digest_hex!(3),
                "222a",
                "0a26",
                digest_hex!(4),
                "1064"
            ),
        ),
        (
            "SealStaged",
            Command::SealStaged {
                object: object(2, 9),
            },
            // seal_staged (62) { object (0a) { epoch 2, seq 9 } }
            concat!("0a08", "6206", "0a04", "0802", "1009"),
        ),
        (
            "ObjectFlushed",
            Command::ObjectFlushed {
                object: object(2, 9),
            },
            // object_flushed (6a)
            concat!("0a08", "6a06", "0a04", "0802", "1009"),
        ),
        (
            "FlushConflict",
            Command::FlushConflict {
                object: object(2, 9),
            },
            // flush_conflict (72)
            concat!("0a08", "7206", "0a04", "0802", "1009"),
        ),
    ]
}

/// Catches: a field tag swapped or renumbered, a type changed, a variant moved; and a
/// decoder that disagrees with the bytes an older binary wrote.
#[test]
fn every_tier_command_has_its_golden_bytes() {
    for (name, command, hex) in golden() {
        let bytes = hex::decode(hex).unwrap_or_else(|e| panic!("{name}: bad hex: {e}"));
        assert_eq!(
            enc(Format::V2, &command).map(hex::encode).as_deref(),
            Ok(hex),
            "{name}: encoding changed"
        );
        assert_eq!(
            dec(Format::V2, &bytes),
            Ok(command),
            "{name}: golden bytes decode differently"
        );
    }
}

/// Every tier command is in the golden set, and only tier commands are: a variant added
/// to `kbf_meta::Command` fails to compile here until it is placed (the match is
/// exhaustive), and a format-1 command in this set would hide that format 1 refuses it.
#[test]
fn the_golden_set_names_every_tier_command() {
    let mut seen = [false; 5];
    for (name, command, _) in golden() {
        let i = match command {
            Command::AddTier { .. } => 0,
            Command::PutStaged { .. } => 1,
            Command::SealStaged { .. } => 2,
            Command::ObjectFlushed { .. } => 3,
            Command::FlushConflict { .. } => 4,
            Command::Tick(_)
            | Command::AllocEpoch
            | Command::PutBlob { .. }
            | Command::PutBlobs(_)
            | Command::PutAction { .. }
            | Command::Touch(_)
            | Command::ObjectUnreachable { .. }
            | Command::ObjectReachable { .. }
            | Command::Collect => panic!("{name} is a format-1 command"),
            Command::EvictObjects { .. } => panic!("{name} is a format-3 command"),
            Command::SetWriter(_)
            | Command::AdoptSegment { .. }
            | Command::RebuildProgress { .. }
            | Command::RebuildDone => panic!("{name} is a format-4 command"),
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
        let n = u8::try_from(self.below(4)).expect("small");
        Digest::new(DigestFunction::Sha256, [n; 32], self.number())
    }

    fn object(&mut self) -> ObjectId {
        ObjectId::new(Epoch::new(self.number()), self.number())
    }

    fn command(&mut self) -> Command {
        match self.below(5) {
            0 => Command::AddTier {
                server: ServerId::new(self.number()),
                tier: TierId::new(self.number()),
            },
            1 => Command::PutStaged {
                tier: TierId::new(self.number()),
                objects: (0..self.below(4))
                    .map(|_| StagedPut {
                        object: self.object(),
                        end: self.number(),
                        seal: self.below(2) == 0,
                        // Unordered, digests repeated: a list, not a set.
                        blobs: (0..self.below(5))
                            .map(|_| (self.digest(), self.number()))
                            .collect(),
                    })
                    .collect(),
            },
            2 => Command::SealStaged {
                object: self.object(),
            },
            3 => Command::ObjectFlushed {
                object: self.object(),
            },
            _ => Command::FlushConflict {
                object: self.object(),
            },
        }
    }
}

/// Catches: a conversion that drops, truncates or mixes up a field (server and tier
/// swapped, `seal` lost, a blob's offset dropped, objects or blobs reordered), and an
/// encoding that is not a function of the value.
#[test]
fn generated_tier_commands_round_trip_and_reencode_to_the_same_bytes() {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    for n in 0..20_000 {
        let command = rng.command();
        let bytes = enc(Format::V2, &command).expect("encode");
        let back =
            dec(Format::V2, &bytes).unwrap_or_else(|e| panic!("value {n}: {command:?}: {e}"));
        assert_eq!(back, command, "value {n}");
        assert_eq!(
            enc(Format::V2, &back).as_ref(),
            Ok(&bytes),
            "value {n}: {command:?} re-encoded differently"
        );
    }
}

/// Catches: a tier command written into a format-1 log, and a format-1 reader that
/// skips one or reads it as something else, which would let a replica without the tier
/// diverge silently. Mutant: drop the format check in `decode`.
#[test]
fn a_format_1_log_refuses_every_tier_command() {
    for (name, command, hex) in golden() {
        assert_eq!(
            enc(Format::V1, &command),
            Err(EncodeError::Command(Format::V1)),
            "{name}"
        );
        let bytes = hex::decode(hex).expect("hex");
        assert_eq!(
            dec(Format::V1, &bytes),
            Err(DecodeError::UnknownCommand("MetaCommand")),
            "{name}"
        );
    }
    // Format 2 still reads and writes every format-1 command, with format 1's bytes.
    let collect = Command::Collect;
    let bytes = enc(Format::V1, &collect).expect("format 1");
    assert_eq!(enc(Format::V2, &collect).as_ref(), Ok(&bytes));
    assert_eq!(dec(Format::V2, &bytes), Ok(collect));
}

/// `MetaCommand` as format 1 has it: fields 1 to 9 only, so prost on a binary built
/// before format 2 drops any other field and leaves the `oneof` unset.
#[derive(Clone, PartialEq, Message)]
struct Format1MetaCommand {
    #[prost(oneof = "Format1Command", tags = "1, 2, 3, 4, 5, 6, 7, 8, 9")]
    command: Option<Format1Command>,
}

#[derive(Clone, PartialEq, prost::Oneof)]
enum Format1Command {
    #[prost(message, tag = "1")]
    Tick(proto::Tick),
    #[prost(message, tag = "2")]
    AllocEpoch(proto::AllocEpoch),
    #[prost(message, tag = "3")]
    PutBlob(proto::PutBlob),
    #[prost(message, tag = "4")]
    PutBlobs(proto::PutBlobs),
    #[prost(message, tag = "5")]
    PutAction(proto::PutAction),
    #[prost(message, tag = "6")]
    Touch(proto::Touch),
    #[prost(message, tag = "7")]
    ObjectUnreachable(proto::ObjectUnreachable),
    #[prost(message, tag = "8")]
    ObjectReachable(proto::ObjectReachable),
    #[prost(message, tag = "9")]
    Collect(proto::Collect),
}

/// What an older binary sees in a tier command: nothing at all (the `oneof` unset),
/// which its decoder refuses as an unknown command
/// (`meta_codec.rs::a_command_this_binary_does_not_know_is_refused`). Catches: a tier
/// command numbered onto a format-1 field, which an older binary would apply as that
/// command. The format-1 commands still decode under the old shape, so the check is
/// not vacuous.
#[test]
fn an_older_binary_sees_no_command_in_a_tier_entry() {
    for (name, command, _) in golden() {
        let bytes = enc(Format::V2, &command).expect("encode");
        let entry = proto::LogCommand::decode(bytes.as_slice()).expect("decode");
        let Some(proto::log_command::Command::Meta(meta)) = entry.command else {
            panic!("{name}: not a metadata command");
        };
        let old = Format1MetaCommand::decode(meta.encode_to_vec().as_slice()).expect("decode");
        assert_eq!(old.command, None, "{name}");
    }
    let collect = proto::MetaCommand {
        command: Some(proto::meta_command::Command::Collect(proto::Collect {})),
    };
    let old = Format1MetaCommand::decode(collect.encode_to_vec().as_slice()).expect("decode");
    assert_eq!(
        old.command,
        Some(Format1Command::Collect(proto::Collect {}))
    );
}

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
    Some(proto::ObjectId {
        epoch: 1,
        seq: 2,
        ..proto::ObjectId::default()
    })
}

fn staged(
    object: Option<proto::ObjectId>,
    blob: proto::StagedBlob,
) -> proto::meta_command::Command {
    proto::meta_command::Command::PutStaged(proto::PutStaged {
        tier: 1,
        objects: vec![proto::StagedObject {
            object,
            end: 100,
            seal: false,
            blobs: vec![blob],
        }],
    })
}

/// Catches: a required field read as a default when absent (an object id of 0/0, an
/// empty digest), and a short hash padded. The well-formed value decodes, so the cases
/// are not vacuous.
#[test]
fn malformed_tier_commands_are_refused() {
    use proto::meta_command::Command as P;
    let blob = proto::StagedBlob {
        digest: Some(pd(1)),
        offset: 0,
    };
    assert!(dec(Format::V2, &wrap(staged(pobject(), blob.clone()))).is_ok());
    let cases: Vec<(&str, P, DecodeError)> = vec![
        (
            "no object",
            staged(None, blob.clone()),
            DecodeError::Missing("StagedObject.object"),
        ),
        (
            "no digest",
            staged(
                pobject(),
                proto::StagedBlob {
                    digest: None,
                    offset: 0,
                },
            ),
            DecodeError::Missing("StagedBlob.digest"),
        ),
        (
            "short hash",
            staged(
                pobject(),
                proto::StagedBlob {
                    digest: Some(proto::Digest {
                        hash: vec![1; 31],
                        ..pd(1)
                    }),
                    offset: 0,
                },
            ),
            DecodeError::HashLength {
                field: "StagedBlob.digest",
                len: 31,
            },
        ),
        (
            "seal without object",
            P::SealStaged(proto::SealStaged { object: None }),
            DecodeError::Missing("SealStaged.object"),
        ),
        (
            "flushed without object",
            P::ObjectFlushed(proto::ObjectFlushed { object: None }),
            DecodeError::Missing("ObjectFlushed.object"),
        ),
        (
            "conflict without object",
            P::FlushConflict(proto::FlushConflict { object: None }),
            DecodeError::Missing("FlushConflict.object"),
        ),
    ];
    for (name, command, error) in cases {
        assert_eq!(dec(Format::V2, &wrap(command)), Err(error), "{name}");
    }
}
