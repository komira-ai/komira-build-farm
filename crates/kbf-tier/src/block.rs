//! The names in a tier directory and the `TIER` file.

use std::num::NonZeroU64;

use kbf_meta::{Epoch, LegacyWriter, NodeWriter, ObjectId, TierId, Writer};

/// The file that names the tier a directory belongs to.
pub const TIER: &str = "TIER";

/// Where the `TIER` file is written before it is renamed into place.
pub(crate) const TIER_TMP: &str = "TIER.tmp";

/// The content of a `TIER` file naming `tier`.
pub(crate) fn tier_file_bytes(tier: TierId) -> Vec<u8> {
    format!("{}\n", tier.get()).into_bytes()
}

/// The tier a `TIER` file names, or `None` if its bytes are not one (a damaged or
/// foreign file).
pub(crate) fn parse_tier_file(bytes: &[u8]) -> Option<TierId> {
    let text = std::str::from_utf8(bytes).ok()?;
    let id = TierId::new(text.strip_suffix('\n')?.parse().ok()?);
    (tier_file_bytes(id) == bytes).then_some(id)
}

/// The stem of `object`'s files: `<epoch>-<seq>` for an epoch writer's object,
/// `<member>-<boot>-<seq>` for a node writer's, each as 16 lowercase hex digits. An
/// object of another log's epoch is never staged (only a rebuild of the index names one),
/// but has a stem all the same: `l<log id>-<epoch>-<seq>`.
fn stem(object: ObjectId) -> String {
    match object.writer() {
        Writer::Epoch(e) => format!("{:016x}-{:016x}", e.get(), object.seq()),
        Writer::Node(n) => format!(
            "{:016x}-{:016x}-{:016x}",
            n.member(),
            n.boot(),
            object.seq()
        ),
        Writer::Legacy(l) => format!(
            "l{:016x}-{:016x}-{:016x}",
            l.log(),
            l.epoch().get(),
            object.seq()
        ),
    }
}

/// The block file of `object`: `<epoch>-<seq>.blk` (an epoch writer's) or
/// `<member>-<boot>-<seq>.blk` (a node writer's), each as 16 lowercase hex digits.
#[must_use]
pub fn block_name(object: ObjectId) -> String {
    format!("{}.blk", stem(object))
}

/// The object a block file name names, or `None` if `name` is not exactly the name
/// [`block_name`] gives some object.
#[must_use]
pub fn parse_block_name(name: &str) -> Option<ObjectId> {
    let hex = |s: &str| u64::from_str_radix(s, 16).ok();
    let fields: Vec<&str> = name.strip_suffix(".blk")?.split('-').collect();
    let object = match fields[..] {
        [epoch, seq] => ObjectId::new(Epoch::new(hex(epoch)?), hex(seq)?),
        [log, epoch, seq] if log.starts_with('l') => {
            let log = NonZeroU64::new(hex(&log[1..])?)?;
            let epoch = NonZeroU64::new(hex(epoch)?)?;
            let writer = LegacyWriter::new(log, Epoch::new(epoch.get()));
            ObjectId::of(Writer::Legacy(writer), hex(seq)?)
        }
        [member, boot, seq] => {
            let member = NonZeroU64::new(hex(member)?)?;
            let boot = NonZeroU64::new(hex(boot)?)?;
            ObjectId::of(Writer::Node(NodeWriter::new(member, boot)), hex(seq)?)
        }
        _ => return None,
    };
    (block_name(object) == name).then_some(object)
}

/// The flushed copy of `object`: its block renamed once its `ObjectFlushed`
/// committed, `<epoch>-<seq>.seg` or `<member>-<boot>-<seq>.seg`.
#[must_use]
pub fn seg_name(object: ObjectId) -> String {
    format!("{}.seg", stem(object))
}

/// The object a flushed copy's name names, or `None` if `name` is not exactly the
/// name [`seg_name`] gives some object.
#[must_use]
pub fn parse_seg_name(name: &str) -> Option<ObjectId> {
    let stem = name.strip_suffix(".seg")?;
    parse_block_name(&format!("{stem}.blk")).filter(|o| seg_name(*o) == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches: a block name that does not round-trip (a restart would treat every
    /// block as foreign and leave it, or as an orphan of the wrong object), and a
    /// parser that takes a name the tier never writes (`+`, upper case, short fields,
    /// other suffixes) and so deletes a file that is not its own.
    #[test]
    fn block_names_round_trip_and_nothing_else_parses() {
        let id = ObjectId::new(Epoch::new(0xab), 7);
        assert_eq!(block_name(id), "00000000000000ab-0000000000000007.blk");
        assert_eq!(parse_block_name(&block_name(id)), Some(id));
        assert_eq!(seg_name(id), "00000000000000ab-0000000000000007.seg");
        assert_eq!(parse_seg_name(&seg_name(id)), Some(id));
        assert_eq!(parse_seg_name(&block_name(id)), None);
        assert_eq!(
            parse_seg_name("00000000000000AB-0000000000000007.seg"),
            None
        );
        for bad in [
            "00000000000000AB-0000000000000007.blk",
            "+0000000000000ab-0000000000000007.blk",
            "ab-7.blk",
            "00000000000000ab-0000000000000007.seg",
            "00000000000000ab0000000000000007.blk",
            "00000000000000ag-0000000000000007.blk",
            "00000000000000ab-000000000000000g.blk",
        ] {
            assert_eq!(parse_block_name(bad), None, "{bad}");
        }
    }

    /// Catches: a node writer's block name that does not round-trip, that drops the
    /// boot (two boots of one member would share a block name), or that parses with a
    /// zero member or boot, a fourth field, or a non-canonical spelling; and an epoch
    /// name read as a node's or the other way round.
    #[test]
    fn node_block_names_round_trip_and_nothing_else_parses() {
        let n = |v| NonZeroU64::new(v).expect("not zero");
        let id = ObjectId::of(Writer::Node(NodeWriter::new(n(7), n(0x2b))), 3);
        let stem = "0000000000000007-000000000000002b-0000000000000003";
        assert_eq!(block_name(id), format!("{stem}.blk"));
        assert_eq!(parse_block_name(&block_name(id)), Some(id));
        assert_eq!(seg_name(id), format!("{stem}.seg"));
        assert_eq!(parse_seg_name(&seg_name(id)), Some(id));
        let next_boot = ObjectId::of(Writer::Node(NodeWriter::new(n(7), n(0x2c))), 3);
        assert_ne!(block_name(next_boot), block_name(id));
        let epoch = ObjectId::new(Epoch::new(7), 3);
        assert_ne!(parse_block_name(&block_name(epoch)), Some(id));
        for bad in [
            "0000000000000000-000000000000002b-0000000000000003.blk",
            "0000000000000007-0000000000000000-0000000000000003.blk",
            "0000000000000007-000000000000002B-0000000000000003.blk",
            "0000000000000007-000000000000002b-0000000000000003-0000000000000001.blk",
            "7-2b-3.blk",
        ] {
            assert_eq!(parse_block_name(bad), None, "{bad}");
        }
    }

    /// Catches: a name for an object of another log's epoch (which a tier never stages,
    /// but the type allows) that collides with an epoch's or a node's, or does not
    /// round-trip, or parses with a zero log or epoch.
    #[test]
    fn another_logs_block_names_round_trip_and_stay_apart() {
        let n = |v| NonZeroU64::new(v).expect("not zero");
        let id = ObjectId::of(Writer::Legacy(LegacyWriter::new(n(9), Epoch::new(2))), 3);
        let stem = "l0000000000000009-0000000000000002-0000000000000003";
        assert_eq!(block_name(id), format!("{stem}.blk"));
        assert_eq!(parse_block_name(&block_name(id)), Some(id));
        assert_eq!(parse_seg_name(&seg_name(id)), Some(id));
        for bad in [
            "l0000000000000000-0000000000000002-0000000000000003.blk",
            "l0000000000000009-0000000000000000-0000000000000003.blk",
            "L0000000000000009-0000000000000002-0000000000000003.blk",
        ] {
            assert_eq!(parse_block_name(bad), None, "{bad}");
        }
    }

    /// Catches: a `TIER` file that does not round-trip, and one whose damaged or
    /// foreign content is read as some tier id (a wrong mount would then pass).
    #[test]
    fn tier_files_round_trip_and_damage_is_none() {
        let id = TierId::new(42);
        assert_eq!(tier_file_bytes(id), b"42\n");
        assert_eq!(parse_tier_file(&tier_file_bytes(id)), Some(id));
        for bad in [&b"42"[..], b"042\n", b"x\n", b"\xff\n", b"42\n\n", b""] {
            assert_eq!(parse_tier_file(bad), None, "{bad:?}");
        }
    }
}
