//! The key and value of a `floor/<instance>/<action>` row: one memory floor
//! ([`kbf_sched::Floor`]), written with the requeue that raises it.
//!
//! The row's key is `floor/`, the instance name in hexadecimal (the name may hold any
//! character, a `/` too), `/`, and the action's digest as `<hash>/<size>`: one key per
//! action key, and no two alike. The value is a version byte (1), the ask (8 bytes,
//! big-endian), the stamp (8), the instance name (2-byte length, then the bytes) and the
//! digest's text to the end. Every digest is SHA-256 (the only [`DigestFunction`]). A row
//! of another version is refused, not guessed at.

use kbf_sched::Floor;
use kbf_types::{ActionKey, Digest, DigestFunction};

const VERSION: u8 = 1;

/// The key of `key`'s row.
pub(super) fn row_key(key: &ActionKey) -> String {
    let instance: String = key
        .instance
        .bytes()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("{}{instance}/{}", super::FLOOR, key.action)
}

/// `floor` as a row value, or `None` if its instance name is too long for it.
pub(super) fn encode(floor: &Floor) -> Option<Vec<u8>> {
    let instance = floor.key.instance.as_bytes();
    let mut row = vec![VERSION];
    row.extend(floor.ask.to_be_bytes());
    row.extend(floor.stamp.to_be_bytes());
    row.extend(u16::try_from(instance.len()).ok()?.to_be_bytes());
    row.extend(instance);
    row.extend(floor.key.action.to_string().as_bytes());
    Some(row)
}

/// A row value as a [`Floor`], or what is wrong with it.
pub(super) fn decode(row: &[u8]) -> Result<Floor, &'static str> {
    let (version, rest) = row.split_first().ok_or("the row ends early")?;
    if *version != VERSION {
        return Err("not a version 1 floor");
    }
    let (ask, rest) = rest.split_first_chunk::<8>().ok_or("the row ends early")?;
    let (stamp, rest) = rest.split_first_chunk::<8>().ok_or("the row ends early")?;
    let (len, rest) = rest.split_first_chunk::<2>().ok_or("the row ends early")?;
    let len = usize::from(u16::from_be_bytes(*len));
    let (instance, digest) = rest.split_at_checked(len).ok_or("the row ends early")?;
    let instance = std::str::from_utf8(instance).map_err(|_| "an instance name is not text")?;
    let digest = std::str::from_utf8(digest).map_err(|_| "a digest is not text")?;
    let action = Digest::parse(DigestFunction::Sha256, digest).map_err(|_| "not a digest")?;
    Ok(Floor {
        key: ActionKey {
            instance: instance.to_owned(),
            action,
        },
        ask: u64::from_be_bytes(*ask),
        stamp: u64::from_be_bytes(*stamp),
    })
}
