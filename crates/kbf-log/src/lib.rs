//! The encoding of the replicated log's command entries (docs/design/ha.md, section
//! 4.6): conversions between the domain values and the `kbf.log.v1` messages of
//! `kbf-proto`, so the domain crates (`kbf-meta`) stay free of codec dependencies.
//!
//! [`encode`] and [`decode`] take the log's committed [`Format`]. Only [`Format::V1`]
//! exists; any other is refused on both sides, so a binary never writes or reads an
//! encoding it does not know.
//!
//! Decoding is strict, because every replica must apply the same value or none:
//! - an enum value that is unspecified (zero) or unnamed is an error, never a default;
//! - a command whose `oneof` is unset (one this binary does not know) is an error;
//! - a required message field that is absent is an error;
//! - a set (a closure, a touch) must be in strictly increasing order;
//! - the bytes must be the ones [`encode`] writes for the decoded value. prost on its own
//!   accepts an explicit zero, a varint that is not minimal, a field repeated or out of
//!   order, and drops a field it does not know; [`decode`] re-encodes the value and
//!   refuses the entry unless the bytes match. So a value has exactly one encoding, and
//!   `encode(decode(bytes)) == bytes` for every accepted entry.
//!
//! An error is fail-stop for the replica that meets it.

mod meta;

use kbf_meta::Command;
use kbf_proto::kbf::log::v1 as proto;
use prost::Message;

/// Which entry encodings a log may hold. Committed in the log, raised only by a
/// committed entry, so every replica decodes an entry under the same format.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Format(u32);

impl Format {
    /// `kbf.log.v1` as `log.proto` has it at format 1.
    pub const V1: Self = Self(1);

    /// Format number `n`, which may not be one this binary supports.
    #[must_use]
    pub const fn new(n: u32) -> Self {
        Self(n)
    }

    /// The raw number.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

/// The payload of a command entry. Later formats add the log's other commands
/// (docs/design/ha.md, section 4.1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LogCommand {
    /// A command of the metadata state machine.
    Meta(Command),
}

/// Why an entry could not be encoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum EncodeError {
    /// This binary does not write the format.
    #[error("log format {} is not supported", .0.get())]
    Format(Format),
    /// A digest names a hash function the format has no value for.
    #[error("a digest names a hash function log format 1 cannot encode")]
    DigestFunction,
}

/// Why an entry could not be decoded. Every case stops the replica.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DecodeError {
    /// This binary does not read the format.
    #[error("log format {} is not supported", .0.get())]
    Format(Format),
    /// The bytes are not a well-formed message.
    #[error("malformed entry: {0}")]
    Malformed(#[from] prost::DecodeError),
    /// A `oneof` is unset: a command this binary does not know.
    #[error("{0} names no command this binary knows")]
    UnknownCommand(&'static str),
    /// An enum value that is unspecified or that this binary does not know.
    #[error("{field} has value {value}, which is unspecified or unknown")]
    UnknownValue {
        /// The field, as `Message.field`.
        field: &'static str,
        /// The value found.
        value: i32,
    },
    /// A required message field is absent.
    #[error("{0} is absent")]
    Missing(&'static str),
    /// A digest's hash is not 32 bytes.
    #[error("{field} has a {len}-byte hash; 32 bytes expected")]
    HashLength {
        /// The field.
        field: &'static str,
        /// The length found.
        len: usize,
    },
    /// A store id does not fit in 16 bits.
    #[error("Location.store {0} does not fit in 16 bits")]
    Store(u32),
    /// A set is out of order or repeats an element.
    #[error("{0} is not in strictly increasing order")]
    SetOrder(&'static str),
    /// The bytes decode to a value, but are not the bytes [`encode`] writes for it.
    #[error("entry is not the canonical encoding of its value")]
    NotCanonical,
}

/// Encodes `command` under `format`.
pub fn encode(format: Format, command: &LogCommand) -> Result<Vec<u8>, EncodeError> {
    if format != Format::V1 {
        return Err(EncodeError::Format(format));
    }
    let message = match command {
        LogCommand::Meta(c) => proto::LogCommand {
            command: Some(proto::log_command::Command::Meta(meta::to_proto(c)?)),
        },
    };
    Ok(message.encode_to_vec())
}

/// Decodes an entry's payload under `format`.
pub fn decode(format: Format, bytes: &[u8]) -> Result<LogCommand, DecodeError> {
    if format != Format::V1 {
        return Err(DecodeError::Format(format));
    }
    let command = match proto::LogCommand::decode(bytes)?.command {
        Some(proto::log_command::Command::Meta(c)) => LogCommand::Meta(meta::from_proto(c)?),
        None => return Err(DecodeError::UnknownCommand("LogCommand")),
    };
    // One value, one encoding: refuse anything prost accepted that `encode` would not
    // have written (an unknown field, a non-minimal varint, a reordered field).
    if encode(format, &command).as_deref() != Ok(bytes) {
        return Err(DecodeError::NotCanonical);
    }
    Ok(command)
}
