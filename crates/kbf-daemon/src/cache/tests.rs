//! Unit tests of the cache's parts: key names, the snapshot format, the stat check.

use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;

use kbf_proto::reapi::Digest;

use super::index::{Discarded, Index, Record, decode, encode};
use super::layout::{Key, link_max, swept_name};
use super::verify::{Found, inspect, stat_check};
use super::{InputStats, LinkMode};
use crate::cas::digest_of;

fn scratch(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/kbf-daemon-unit")
        .join(format!("cache-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch");
    dir
}

/// Catches a key name that does not round-trip, a name parsed as a key that is not
/// exactly one (upper-case hex, a size with a leading zero, no size, a short hash), and
/// a digest with a negative size taken as a key.
#[test]
fn key_names_round_trip_and_nothing_else_parses() {
    let digest = digest_of(b"bytes");
    for executable in [false, true] {
        let key = Key::of(&digest, executable).expect("a key");
        assert_eq!(Key::parse(&key.name()), Some(key));
        assert_eq!(key.digest(), digest);
        assert_eq!(key.sibling().sibling(), key);
        assert_ne!(key.sibling(), key);
        assert_eq!(key.prefix(), digest.hash[..2]);
    }
    let name = Key::of(&digest, false).expect("a key").name();
    let (hash, _) = name.split_once('-').expect("a dash");
    for bad in [
        name.to_uppercase(),
        format!("{hash}-05"),
        hash.to_owned(),
        format!("{hash}-x"),
        format!("{}-5", &hash[..62]),
        format!("{name}.y"),
    ] {
        assert_eq!(Key::parse(&bad), None, "{bad}");
    }
    let negative = Digest {
        hash: digest.hash.clone(),
        size_bytes: -1,
    };
    assert_eq!(Key::of(&negative, false), None);
    assert_eq!(
        Key::of(
            &Digest {
                hash: "zz".repeat(32),
                size_bytes: 1
            },
            false
        ),
        None
    );
}

fn records() -> Vec<Record> {
    let mut records: Vec<Record> = (0..3_u8)
        .map(|i| Record {
            key: Key::of(&digest_of(&[i]), i == 1).expect("a key"),
            last_use: 1000 + u64::from(i),
        })
        .collect();
    records.sort_by_key(|r| r.key);
    records
}

/// Catches an encoding that does not decode to itself, and each malformed snapshot
/// read as entries: too short for a header, a length that is not the count's, a CRC
/// that does not match, another magic or version, a key listed twice.
#[test]
fn snapshots_decode_only_when_whole() {
    let good = encode(&records());
    assert_eq!(good.len(), 16 + 8 + 3 * 56 + 4);
    assert_eq!(decode(&good), Ok(records()));
    assert_eq!(decode(&encode(&[])), Ok(Vec::new()));
    let recrc = |mut bytes: Vec<u8>| {
        let at = bytes.len() - 4;
        let crc = crc32fast::hash(&bytes[..at]);
        bytes[at..].copy_from_slice(&crc.to_le_bytes());
        bytes
    };
    assert_eq!(decode(&good[..27]), Err(Discarded::Corrupt));
    let mut header_only = good[..16].to_vec();
    header_only.extend_from_slice(&[0; 4]);
    let header_only = recrc(header_only);
    assert_eq!(
        decode(&header_only),
        Err(Discarded::Corrupt),
        "short, with a CRC that matches"
    );
    let mut flipped = good.clone();
    flipped[30] ^= 1;
    assert_eq!(decode(&flipped), Err(Discarded::Corrupt));
    let mut magic = good.clone();
    magic[0] = b'X';
    assert_eq!(decode(&recrc(magic)), Err(Discarded::Version));
    let mut version = good.clone();
    version[8] = 2;
    assert_eq!(decode(&recrc(version)), Err(Discarded::Version));
    let mut counted = good.clone();
    counted[16] = 4;
    assert_eq!(decode(&recrc(counted)), Err(Discarded::Corrupt));
    let mut huge = good.clone();
    huge[16..24].copy_from_slice(&u64::MAX.to_le_bytes());
    assert_eq!(decode(&recrc(huge)), Err(Discarded::Corrupt));
    let mut twice = good.clone();
    let first = twice[24..80].to_vec();
    twice[80..136].copy_from_slice(&first);
    assert_eq!(decode(&recrc(twice)), Err(Discarded::Corrupt));
    for why in [Discarded::Missing, Discarded::Corrupt, Discarded::Version] {
        assert!(!why.word().is_empty());
    }
}

/// Catches an index whose total or LRU order drifts: inserts count once, removals
/// uncount, a touch moves a key to the newest end, and the records come out in key
/// order.
#[test]
fn the_index_counts_each_key_once_in_lru_order() {
    let mut index = Index::loaded(&records());
    let keys: Vec<Key> = records().iter().map(|r| r.key).collect();
    assert_eq!(index.total(), 3);
    assert!(!index.insert(keys[0], 5, true), "a key added twice");
    assert_eq!(index.total(), 3);
    index.touch(&keys[0], 2000, true);
    assert_eq!(index.oldest().last(), Some(&keys[0]));
    assert!(index.get(&keys[0]).is_some_and(|e| e.verified));
    assert!(index.remove(&keys[1]));
    assert!(!index.remove(&keys[1]));
    assert_eq!((index.total(), index.len()), (2, 2));
    let under = index.under(keys[2].hash[0]);
    assert!(under.contains(&keys[2]));
    assert!(index.under(0xff).iter().all(|k| k.hash[0] == 0xff));
    assert_eq!(index.records().len(), 2);
}

/// Catches a stat check that passes a file of another owner, mode, size or type, or
/// that reads a path it cannot reach as absent; and a re-hash that passes other bytes.
#[test]
fn the_stat_check_and_the_re_hash_refuse_what_is_not_the_entry() {
    let dir = scratch("stat");
    let key = Key::of(&digest_of(b"entry"), false).expect("a key");
    let uid = rustix::process::geteuid().as_raw();
    let path = dir.join("entry");
    assert_eq!(stat_check(&path, &key, uid), Err(Found::Absent));
    assert_eq!(inspect(&path, &key, uid), Found::Absent);
    std::fs::write(&path, b"entry").expect("write");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444)).expect("chmod");
    assert_eq!(stat_check(&path, &key, uid), Ok(1));
    assert_eq!(inspect(&path, &key, uid), Found::Good(1));
    assert_eq!(
        stat_check(&path, &key, uid + 1),
        Err(Found::Bad),
        "another owner"
    );
    assert_eq!(
        stat_check(&path, &key.sibling(), uid),
        Err(Found::Bad),
        "another mode"
    );
    let other = Key::of(&digest_of(b"other"), false).expect("a key");
    assert_eq!(inspect(&path, &other, uid), Found::Bad, "other bytes");
    let longer = Key::of(&digest_of(b"entry!"), false).expect("a key");
    assert_eq!(
        stat_check(&path, &longer, uid),
        Err(Found::Bad),
        "another size"
    );
    std::fs::create_dir(dir.join("dir")).expect("dir");
    std::fs::set_permissions(dir.join("dir"), std::fs::Permissions::from_mode(0o444))
        .expect("chmod");
    let empty = Key::of(&digest_of(b""), false).expect("a key");
    assert_eq!(
        stat_check(&dir.join("dir"), &empty, uid),
        Err(Found::Bad),
        "a directory"
    );
    assert_eq!(
        stat_check(&path.join("below"), &key, uid),
        Err(Found::Bad),
        "ENOTDIR"
    );
}

/// Catches a panic on the blocking pool lost or turned into a value, and the small
/// words the daemon logs.
#[tokio::test]
async fn a_panic_on_the_blocking_pool_is_resumed() {
    let joined = tokio::spawn(super::fill::blocking(|| -> u8 { panic!("in the pool") })).await;
    assert!(joined.expect_err("it panicked").is_panic());
    assert_eq!(super::fill::blocking(|| 7).await, 7);
    assert_eq!(LinkMode::Hardlink.to_string(), "hardlink");
    assert_eq!(LinkMode::Copy.to_string(), "copy");
    assert_eq!(LinkMode::Clone.to_string(), "clone");
    assert!(super::system_clock()() > 1_000_000_000);
    super::Hooks::at(&super::NoHooks, super::Point::PassStarts);
    for (disabled, word) in [
        (super::Disabled::Locked, "locked"),
        (super::Disabled::Unusable(String::new()), "unusable"),
        (super::Disabled::LeaseName, "lease-name"),
        (super::Disabled::NoBudget, "no-budget"),
        (super::Disabled::NativeSameUid, "native-same-uid"),
    ] {
        assert_eq!(disabled.reason(), word);
    }
    assert!(link_max(std::path::Path::new("/")) > 1000);
    assert!(swept_name(std::path::Path::new("kbf-lease-1")));
    assert!(swept_name(std::path::Path::new("lease-1")));
    assert!(!swept_name(std::path::Path::new("cache")));
    let line = InputStats {
        files: 3,
        ..InputStats::default()
    }
    .to_string();
    assert!(line.starts_with("files=3 "), "{line}");
}

/// Catches `CacheConfig::new` giving a node with one cache another room check than the
/// one its docs promise: local storage that grows past the budget is evicted for within
/// a second (a room check an hour apart would let uploads fill the disk meanwhile), and
/// a cache is not one cache unless it is told so.
#[test]
fn a_new_config_looks_for_room_every_second_and_has_no_one_cache() {
    let config = super::CacheConfig::new(PathBuf::from("c"), PathBuf::from("s"), 7);
    assert_eq!(config.room_every, std::time::Duration::from_secs(1));
    assert!(config.local.is_none());
}
