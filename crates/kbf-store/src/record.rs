//! The bytes on disk.
//!
//! A file is a sequence of records. A record is the payload's length (`u32`, little
//! endian), the CRC-32C of those four length bytes followed by the payload (`u32`,
//! little endian), then the payload. Checking the length under the CRC means a damaged
//! length is caught, not followed.
//!
//! An entry's payload is its index and term (`u64` each, little endian), a kind byte
//! (0 blank, 1 command) and, for a command, its bytes. The hard state's payload is the
//! term (`u64`), a vote byte (0 none, 1 some) and, with a vote, the server (`u64`).

use kbf_raft::{Entry, HardState, LogId, LogIndex, Payload, ServerId, Term};

use crate::crc::crc32c;

/// The bytes before a record's payload.
pub const HEADER: usize = 8;

/// The largest payload a record may carry: 64 MiB.
pub const MAX_PAYLOAD: usize = 64 << 20;

/// The fixed part of an entry's payload: index, term, kind.
const ENTRY_FIXED: usize = 17;

/// Appends one record carrying `payload` to `out`.
///
/// # Panics
///
/// If `payload` is longer than [`MAX_PAYLOAD`]; callers check first.
pub fn put_record(out: &mut Vec<u8>, payload: &[u8]) {
    assert!(payload.len() <= MAX_PAYLOAD, "payload over MAX_PAYLOAD");
    let len = u32::try_from(payload.len()).expect("MAX_PAYLOAD fits in u32");
    let len = len.to_le_bytes();
    out.extend_from_slice(&len);
    out.extend_from_slice(&crc32c(&[&len, payload]).to_le_bytes());
    out.extend_from_slice(payload);
}

/// The record at the start of `bytes`: its payload and its length on disk, or `None`
/// if the bytes there are not a whole record with a matching CRC.
pub fn get_record(bytes: &[u8]) -> Option<(&[u8], usize)> {
    let len_bytes: [u8; 4] = bytes.get(..4)?.try_into().ok()?;
    let crc_bytes: [u8; 4] = bytes.get(4..HEADER)?.try_into().ok()?;
    let len = usize::try_from(u32::from_le_bytes(len_bytes)).ok()?;
    if len > MAX_PAYLOAD {
        return None;
    }
    let payload = bytes.get(HEADER..HEADER + len)?;
    (crc32c(&[&len_bytes, payload]) == u32::from_le_bytes(crc_bytes))
        .then_some((payload, HEADER + len))
}

/// Whether a whole record with a matching CRC starts at any offset of `bytes`.
pub fn any_record(bytes: &[u8]) -> bool {
    (0..bytes.len()).any(|o| get_record(&bytes[o..]).is_some())
}

/// The payload of `entry`.
pub fn encode_entry(entry: &Entry) -> Vec<u8> {
    let data: &[u8] = match &entry.payload {
        Payload::Blank => &[],
        Payload::Command(c) => c,
    };
    let mut out = Vec::with_capacity(ENTRY_FIXED + data.len());
    out.extend_from_slice(&entry.id.index.0.to_le_bytes());
    out.extend_from_slice(&entry.id.term.0.to_le_bytes());
    out.push(match entry.payload {
        Payload::Blank => 0,
        Payload::Command(_) => 1,
    });
    out.extend_from_slice(data);
    out
}

/// The entry `payload` encodes, or why it does not encode one.
pub fn decode_entry(payload: &[u8]) -> Result<Entry, &'static str> {
    if payload.len() < ENTRY_FIXED {
        return Err("entry shorter than its fixed part");
    }
    let id = LogId::new(Term(u64_at(payload, 8)), LogIndex(u64_at(payload, 0)));
    let data = &payload[ENTRY_FIXED..];
    let payload = match payload[16] {
        0 if data.is_empty() => Payload::Blank,
        0 => return Err("blank entry with bytes"),
        1 => Payload::Command(data.to_vec()),
        _ => return Err("unknown entry kind"),
    };
    Ok(Entry { id, payload })
}

/// The payload of `hard`.
pub fn encode_hard(hard: HardState) -> Vec<u8> {
    let mut out = hard.term.0.to_le_bytes().to_vec();
    match hard.voted_for {
        None => out.push(0),
        Some(ServerId(s)) => {
            out.push(1);
            out.extend_from_slice(&s.to_le_bytes());
        }
    }
    out
}

/// The hard state `payload` encodes, or why it does not encode one.
pub fn decode_hard(payload: &[u8]) -> Result<HardState, &'static str> {
    let voted_for = match (payload.len(), payload.get(8)) {
        (9, Some(0)) => None,
        (17, Some(1)) => Some(ServerId(u64_at(payload, 9))),
        _ => return Err("malformed hard state"),
    };
    Ok(HardState {
        term: Term(u64_at(payload, 0)),
        voted_for,
    })
}

fn u64_at(bytes: &[u8], at: usize) -> u64 {
    let mut b = [0u8; 8];
    b.copy_from_slice(&bytes[at..at + 8]);
    u64::from_le_bytes(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(index: u64, term: u64, payload: Payload) -> Entry {
        Entry {
            id: LogId::new(Term(term), LogIndex(index)),
            payload,
        }
    }

    /// Catches: a change to the on-disk layout (the golden bytes pin field order,
    /// widths and endianness, so an old log stays readable), and a codec that does
    /// not round-trip either kind or the vote.
    #[test]
    fn golden_bytes_and_round_trip() {
        let e = entry(2, 3, Payload::Command(vec![0xAB]));
        let mut rec = Vec::new();
        put_record(&mut rec, &encode_entry(&e));
        assert_eq!(
            hex(&rec),
            "120000009a95fc370200000000000000030000000000000001ab"
        );
        let (payload, used) = get_record(&rec).unwrap();
        assert_eq!(used, rec.len());
        assert_eq!(decode_entry(payload).unwrap(), e);
        let blank = entry(1, 1, Payload::Blank);
        assert_eq!(decode_entry(&encode_entry(&blank)).unwrap(), blank);

        let hard = HardState {
            term: Term(7),
            voted_for: Some(ServerId(2)),
        };
        assert_eq!(
            hex(&encode_hard(hard)),
            "0700000000000000010200000000000000"
        );
        assert_eq!(decode_hard(&encode_hard(hard)).unwrap(), hard);
        let none = HardState::default();
        assert_eq!(decode_hard(&encode_hard(none)).unwrap(), none);
    }

    /// Catches: a record accepted with a bad CRC, a damaged length, a short body or a
    /// length over the cap, and payloads that decode though malformed (fail-stop needs
    /// every one refused).
    #[test]
    fn damaged_records_and_payloads_are_refused() {
        let mut rec = Vec::new();
        put_record(&mut rec, b"hello");
        for i in 0..rec.len() {
            let mut bad = rec.clone();
            bad[i] ^= 0x01;
            assert!(get_record(&bad).is_none(), "flip at {i}");
        }
        for n in 0..rec.len() {
            assert!(get_record(&rec[..n]).is_none(), "cut at {n}");
        }
        let mut huge = (u32::try_from(MAX_PAYLOAD).unwrap() + 1)
            .to_le_bytes()
            .to_vec();
        huge.extend_from_slice(&[0; 4]);
        assert!(get_record(&huge).is_none());
        assert!(any_record(&[&[0u8; 3][..], &rec].concat()));
        assert!(!any_record(&rec[1..]));

        let mut blank = encode_entry(&entry(1, 1, Payload::Blank));
        assert!(decode_entry(&blank[..16]).is_err());
        blank.push(9);
        assert_eq!(decode_entry(&blank), Err("blank entry with bytes"));
        blank[16] = 2;
        assert_eq!(decode_entry(&blank), Err("unknown entry kind"));
        assert!(decode_hard(&[0; 8]).is_err());
        assert!(decode_hard(&[0; 9].map(|_| 1)).is_err());
        assert!(decode_hard(&[0; 17]).is_err());
    }

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }
}
