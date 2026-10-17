//! Unit tests of the flags (`super`).

use std::num::{NonZeroU32, NonZeroU64, NonZeroUsize};
use std::time::Duration;

use clap::Parser;
use kbf_front::{MIN_SEGMENT_UPLOAD_BYTES, WriteConfig};

use kbf_threads::disk::{FreeSpace, Space};

use super::{Args, loopback_only};

/// Catches: `min-put-rate` lost on the way from the URI to the store (the flat 60 s
/// that cut slow segment PUTs), a default other than 1 MiB/s, `conditional-put` and
/// the region dropped, and a rate of 0 accepted (it would divide by zero in the
/// store's timeout).
#[test]
fn the_s3_uri_reaches_the_store_config() {
    let parse = |query: &str| {
        let uri = format!("s3://b/p?endpoint=http://e:9{query}");
        Args::try_parse_from(["kbf-server", "--cold-store", &uri])
    };
    let config = |query: &str| {
        let args = parse(query).expect("a store");
        let uri = args.cold_store.expect("a store");
        let uri = uri.as_s3().expect("an s3 store");
        super::s3_config(uri, super::Credentials::new("k", "s"))
    };
    let default = config("");
    assert_eq!(default.min_body_rate, NonZeroU64::new(1 << 20));
    assert_eq!(default.region, "us-east-1");
    assert!(!default.capabilities.conditional_put);
    assert_eq!(default.endpoint, "http://e:9");
    assert_eq!(default.bucket, "b");
    let over = config("&min-put-rate=4096&region=eu-1&conditional-put=true");
    assert_eq!(over.min_body_rate, NonZeroU64::new(4096));
    assert_eq!(over.region, "eu-1");
    assert!(over.capabilities.conditional_put);
    assert!(parse("&min-put-rate=0").is_err());
}

/// Catches: the store flags not reaching the resolution (a hot store ignored, the
/// budget from the wrong filesystem number), a fake free-space source not used, and
/// the old flags accepted (a stale unit would go unnoticed).
#[test]
fn the_store_flags_resolve_and_the_old_ones_are_refused() {
    let space = Space {
        total: 100 << 30,
        available: 50 << 30,
    };
    let args = Args::parse_from([
        "kbf-server",
        "--cold-store=file:///c",
        "--hot-store=file:///h",
    ]);
    let stores = args.stores(&Fake(space)).expect("stores");
    assert_eq!(stores.hot.expect("hot").budget, space.of_free(80, None));
    assert_eq!(stores.meta_dir, std::path::Path::new("/h/meta"));
    for flag in [
        "--store=s3",
        "--blob-dir=/b",
        "--blob-dir-min-free-bytes=1",
        "--meta-log=raft",
        "--raft-dir=/r",
        "--s3-endpoint=http://e:9",
        "--s3-bucket=b",
        "--s3-region=r",
        "--s3-prefix=p/",
        "--s3-conditional-put",
        "--s3-min-put-rate=9",
        "--write-tier-dir=/t",
        "--write-tier-bytes=9",
        "--write-tier-seal-after=9",
        "--write-tier-abandon=9",
    ] {
        let refused = Args::try_parse_from(["kbf-server", flag]).expect_err(flag);
        assert_eq!(
            refused.kind(),
            clap::error::ErrorKind::UnknownArgument,
            "{flag}"
        );
    }
    for flag in ["--hot-store-max-bytes=9", "--hot-store-abandon=9"] {
        assert!(
            Args::try_parse_from(["kbf-server", flag]).is_err(),
            "{flag}"
        );
    }
}

/// Behaviour equivalence for a converged config: the flags of the unit this release
/// renders open the stores the old unit's flags did (`--store=s3 --s3-endpoint=...
/// --s3-bucket=kbf-cas --s3-conditional-put --meta-log=raft --raft-dir=...`): the
/// same endpoint, bucket, region, capabilities, timeouts and put rate; the key prefix
/// root `kbf/`; the log in the same directory. And a directory cold store and a hot
/// directory keep what `--blob-dir` (prefix `kbf/`) and `--write-tier-dir` (the
/// 30 s seal) had; the directory's floor is no longer a fixed 1 GiB but the
/// evicting directory's `keep_free` (5%, at least 1 GiB).
#[test]
fn a_converged_unit_opens_the_stores_the_old_flags_did() {
    let args = Args::parse_from([
        "kbf-server",
        "--cold-store=s3://kbf-cas?endpoint=http://127.0.0.1:9000&conditional-put=true",
        "--meta-dir=/var/lib/kbf-server/raft",
    ]);
    let stores = args
        .stores(&Fake(Space {
            total: 100 << 30,
            available: 50 << 30,
        }))
        .expect("stores");
    let uri = stores.cold.as_s3().expect("an S3 cold store");
    assert_eq!(uri.prefix, "kbf/", "the old --s3-prefix default");
    let config = super::s3_config(uri, super::Credentials::new("k", "s"));
    assert_eq!(config.endpoint, "http://127.0.0.1:9000");
    assert_eq!(config.bucket, "kbf-cas");
    assert_eq!(config.region, "us-east-1");
    assert!(config.capabilities.conditional_put);
    assert!(!config.capabilities.object_lock);
    assert_eq!(config.connect_timeout, Duration::from_secs(5));
    assert_eq!(config.request_timeout, Duration::from_secs(60));
    assert_eq!(config.min_body_rate, NonZeroU64::new(1 << 20));
    assert_eq!(
        stores.meta_dir,
        std::path::Path::new("/var/lib/kbf-server/raft")
    );
    assert_eq!(stores.hot, None, "no tier unless asked, as before");
    assert_eq!(
        crate::stores::COLD_FILE_FLOOR,
        kbf_objstore::Floor::KeepFree
    );
    assert_eq!(crate::stores::FILE_PREFIX_ROOT, "kbf/");
    assert_eq!(kbf_tier::TierOptions::SEAL_AFTER, Duration::from_secs(30));
}

/// A free-space source with one answer.
struct Fake(Space);

impl FreeSpace for Fake {
    fn space(&self, _: &std::path::Path) -> std::io::Result<Space> {
        Ok(self.0)
    }
}

/// Catches a `--cpu-threads` that does not reach the cache's hashing pool (the
/// flusher would hash on the default width whatever the operator set), a default
/// that is not the CPU count (`kbf_threads`: the server's own `Args::cpu_threads`),
/// a `--reserve-cpus` or `--reserve-share` still accepted (a stale unit would go
/// unnoticed, and the server would ignore the number it was given), and 0 accepted
/// (a pool that never runs a job). Parsing at all catches a second `--cpu-threads`
/// (clap panics on a duplicate name).
#[test]
fn cpu_threads_reaches_the_hashing_pool() {
    let args = Args::parse_from(["kbf-server"]);
    let defaults = args.write_config();
    assert_eq!(defaults.cpu_threads, args.cpu_threads());
    let machine = kbf_threads::Topology::detect().logical();
    assert_eq!(defaults.cpu_threads.get(), machine);
    let set = Args::parse_from(["kbf-server", "--cpu-threads=7"]).write_config();
    assert_eq!(set.cpu_threads.get(), 7);
    assert!(Args::try_parse_from(["kbf-server", "--cpu-threads=0"]).is_err());
    for flag in ["--reserve-cpus=4", "--reserve-share=2"] {
        let refused = Args::try_parse_from(["kbf-server", flag]).expect_err(flag);
        assert_eq!(
            refused.kind(),
            clap::error::ErrorKind::UnknownArgument,
            "{flag}"
        );
        assert!(
            refused
                .to_string()
                .contains(&flag[..flag.find('=').unwrap_or(flag.len())])
        );
    }
}

/// Catches a `--segment-upload-*` flag that does not reach the listeners, a default
/// other than the design's (33 MiB, 60 s), and a cap below it accepted (a daemon's
/// upload of small files would be refused for good).
#[test]
fn segment_upload_flags_reach_the_listeners() {
    let defaults = Args::parse_from(["kbf-server"])
        .listeners()
        .expect("listeners");
    assert_eq!(defaults.segments, kbf_front::SegmentsConfig::DEFAULT);
    assert_eq!(defaults.segments.max_bytes, 33 << 20);
    assert_eq!(defaults.segments.idle, std::time::Duration::from_secs(60));
    let set = Args::parse_from([
        "kbf-server",
        "--segment-upload-max-bytes=1073741824",
        "--segment-upload-idle-secs=5",
    ])
    .listeners()
    .expect("listeners");
    assert_eq!(set.segments.max_bytes, 1 << 30);
    assert_eq!(set.segments.idle, std::time::Duration::from_secs(5));
    let below = format!("--segment-upload-max-bytes={}", (33u64 << 20) - 1);
    for refused in [below.as_str(), "--segment-upload-idle-secs=0"] {
        assert!(
            Args::try_parse_from(["kbf-server", refused]).is_err(),
            "{refused} accepted"
        );
    }
}

/// Catches a server that starts with `--segment-upload-max-bytes` over
/// `--write-bytes-in-flight` (mutant: the check removed). A segment upload holds its
/// ranges in memory under the write bound, so an upload of the whole cap would hold
/// more than the bound admits. A cap equal to the bound starts; one byte of bound
/// less is refused, naming both flags.
#[test]
fn a_segment_upload_cap_over_the_write_bound_is_refused() {
    let cap = kbf_front::MIN_SEGMENT_UPLOAD_BYTES + 1;
    let args = |bound: u64| {
        Args::parse_from([
            "kbf-server".to_owned(),
            format!("--segment-upload-max-bytes={cap}"),
            format!("--write-bytes-in-flight={bound}"),
        ])
    };
    args(cap).listeners().expect("a cap equal to the bound");
    let refused = args(cap - 1)
        .listeners()
        .map(drop)
        .expect_err("a cap over the bound");
    let message = refused.to_string();
    assert!(message.contains("--segment-upload-max-bytes"), "{message}");
    assert!(message.contains("--write-bytes-in-flight"), "{message}");
}

/// Catches a server that starts with a `--segment-upload-max-bytes` larger than one
/// admission counts (mutant: the flag's upper limit removed). `Cache::admit` counts
/// at most `kbf_front::MAX_ADMISSION_BYTES` (what its semaphore holds) for one call;
/// a `Begin` over it would hold more ranges than it is counted for. A cap of exactly
/// the most one admission counts starts; one byte more is refused.
#[test]
fn a_segment_upload_cap_over_one_admission_is_refused() {
    let most = kbf_front::MAX_ADMISSION_BYTES;
    let bound = format!("--write-bytes-in-flight={most}");
    let cap = |cap: u64| format!("--segment-upload-max-bytes={cap}");
    Args::try_parse_from(["kbf-server", &cap(most), &bound])
        .expect("a cap one admission counts in full")
        .listeners()
        .expect("under the bound");
    let refused = Args::try_parse_from(["kbf-server", &cap(most + 1), &bound])
        .map(drop)
        .expect_err("a cap over one admission");
    let message = refused.to_string();
    assert!(message.contains("--segment-upload-max-bytes"), "{message}");
}

/// Catches the input-tree flags not reaching the config, a zero where the check
/// needs at least one, and defaults other than the design's.
#[test]
fn input_tree_flags_make_the_input_tree_config() {
    let defaults = Args::parse_from(["kbf-server"]).input_tree_config();
    assert_eq!(defaults, kbf_front::InputTreeConfig::DEFAULT);
    let set = Args::parse_from([
        "kbf-server",
        "--input-tree-cache-entries",
        "11",
        "--input-tree-cache-bytes",
        "2222",
        "--input-tree-reads-in-flight",
        "3",
    ]);
    let want = kbf_front::InputTreeConfig {
        max_entries: NonZeroUsize::new(11).expect("11"),
        max_bytes: 2222,
        reads_in_flight: NonZeroUsize::new(3).expect("3"),
    };
    assert_eq!(set.input_tree_config(), want);
    for flag in ["--input-tree-cache-entries", "--input-tree-reads-in-flight"] {
        assert!(
            Args::try_parse_from(["kbf-server", flag, "0"]).is_err(),
            "{flag}"
        );
    }
}

/// Catches the membership flags not reaching the config (swapped, or the defaults
/// other than the design's) and the replication factor changing with them.
#[test]
fn member_flags_make_the_membership_config() {
    let defaults = Args::parse_from(["kbf-server"]).members();
    assert_eq!(defaults, crate::members::MembershipConfig::default());
    let set = Args::parse_from([
        "kbf-server",
        "--state-store",
        "sqlite:///var/lib/kbf/state.db",
        "--member-ttl-secs",
        "30",
        "--member-dwell-secs",
        "600",
    ])
    .members();
    assert_eq!(set.ttl, Duration::from_secs(30));
    assert_eq!(set.dwell, Duration::from_secs(600));
    assert_eq!(
        set.replication,
        crate::members::MembershipConfig::default().replication
    );
}

/// Catches: a `--write-bytes-in-flight` below the smallest segment upload cap (a
/// `Begin` of that cap would wait for admission forever), a flag wired to the wrong field of
/// the write configuration, and defaults other than the design's.
#[test]
fn write_flags_make_the_write_config() {
    let defaults = Args::parse_from(["kbf-server"]).write_config();
    // The CPU pool's width is the CPU count (`kbf_threads`), not the front's default.
    assert_eq!(
        defaults,
        WriteConfig {
            cpu_threads: defaults.cpu_threads,
            ..WriteConfig::DEFAULT
        }
    );
    let set = Args::parse_from([
        "kbf-server",
        "--write-linger",
        "7",
        "--write-own-segment-bytes",
        "12345",
        "--write-bytes-in-flight",
        &MIN_SEGMENT_UPLOAD_BYTES.to_string(),
        "--write-puts-in-flight",
        "9",
        "--store-put-attempts",
        "5",
        "--cpu-threads",
        "3",
    ]);
    let want = WriteConfig {
        linger: Duration::from_millis(7),
        own_segment_bytes: 12345,
        bytes_in_flight: MIN_SEGMENT_UPLOAD_BYTES,
        puts_in_flight: NonZeroUsize::new(9).expect("9"),
        put_attempts: NonZeroU32::new(5).expect("5"),
        cpu_threads: NonZeroUsize::new(3).expect("3"),
    };
    assert_eq!(set.write_config(), want);
    let below = (MIN_SEGMENT_UPLOAD_BYTES - 1).to_string();
    for (flag, value) in [
        ("--write-bytes-in-flight", below.as_str()),
        ("--write-puts-in-flight", "0"),
        ("--store-put-attempts", "0"),
        ("--store-put-attempts", "6"),
        ("--write-linger", "1001"),
    ] {
        let refused = Args::try_parse_from(["kbf-server", flag, value]);
        assert!(refused.is_err(), "{flag} {value} was accepted");
    }
}

/// Catches: a `loopback_only` that knows only wildcards (`is_unspecified`), so one
/// interface other machines reach (`192.0.2.10`, a documentation address standing
/// in for a LAN or mesh one) counts as loopback; and one that drops `to_canonical`,
/// so the IPv4-mapped loopback address does not.
#[test]
fn only_loopback_addresses_are_loopback_only() {
    for (ip, want) in [
        ("127.0.0.1", true),
        ("::1", true),
        ("::ffff:127.0.0.1", true),
        ("0.0.0.0", false),
        ("::", false),
        ("192.0.2.10", false),
    ] {
        assert_eq!(loopback_only(ip.parse().expect("an address")), want, "{ip}");
    }
}

/// Catches: a call site in [`Args::plain_text_off_loopback`] that does not go
/// through `loopback_only`. One listener is on a single non-loopback interface
/// (`192.0.2.10`, which a wildcard-only check such as `is_unspecified` misses) and
/// the other on the IPv4-mapped loopback address (`::ffff:127.0.0.1`, which
/// `is_loopback` without `to_canonical` misses), each way round, so either
/// mistake at either listener changes the list.
#[test]
fn plain_text_off_loopback_checks_each_listener_through_loopback_only() {
    for (listen, worker_listen, want) in [
        ("192.0.2.10:0", "[::ffff:127.0.0.1]:0", vec!["reapi"]),
        ("[::ffff:127.0.0.1]:0", "192.0.2.10:0", vec!["worker"]),
    ] {
        let args = Args::parse_from([
            "kbf-server",
            "--listen",
            listen,
            "--worker-listen",
            worker_listen,
        ]);
        assert_eq!(
            args.plain_text_off_loopback(),
            want,
            "--listen {listen} --worker-listen {worker_listen}"
        );
    }
}
