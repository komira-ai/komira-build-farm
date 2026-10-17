//! The binary's data directory (`docs/design/worker-cache.md`): the container driver
//! opens the cache and the trees under it by default, a second owner runs uncached,
//! the native driver makes them on macOS only, the per-piece flags of an older binary are
//! refused, and the binary answers as the driver's `__chown-dirs` helper before it
//! parses its flags.

use super::{BIN, one_native_daemon, runs_until_sigterm_logged, tls};

/// Whether a line of `log` holds every one of `words`.
fn logged(log: &[String], words: &[&str]) -> bool {
    log.iter()
        .any(|line| words.iter().all(|w| line.contains(w)))
}

/// The flags of a container daemon with the id tools on `PATH`, a stand-in cgroup tree and
/// a full subordinate id range, run as `name`, with its data directory `data`.
#[cfg(target_os = "linux")]
fn container(name: &str, data: &std::path::Path) -> (Vec<String>, std::path::PathBuf) {
    let ids = super::id_files(&format!("{name}-ids"), "65536");
    let (_, cgroup) = super::cgroup_tree(&tls(&format!("{name}-cgroup")), "cpu memory pids");
    let mut flags = vec![
        "--driver=container".to_owned(),
        super::OCI_RUNTIME.to_owned(),
        "--cas=https://127.0.0.1:1".to_owned(),
        format!("--data-dir={}", data.display()),
        format!("--id-files={}", ids.display()),
        "--actions-memory-max-gib=1".to_owned(),
    ];
    flags.extend(cgroup);
    let (bin, _) = super::container_bin(&format!("{name}-bin"));
    (flags, bin)
}

/// C-16 and contract 3, at the daemon's level. Catches the container driver not opening
/// the cache under `--data-dir` without any cache flag (it was off by default), a daemon
/// that exits without saving its index, and a cache another daemon owns used or
/// stopping the start: the first daemon opens `<data>/cache`; once the snapshot written
/// right after load is there, this test removes it, so the one found after SIGTERM is
/// the clean stop's (the next timed snapshot is 5 minutes away, the first pass's end
/// minutes); while this test holds the cache's lock, a daemon warns `off:locked` and
/// serves. It also catches the layout drifting: `leases` and `cache` 0700 under a 0755
/// data directory, and the trees store opened with the constants the daemon runs
/// with, made 0711 for a rootless runtime's container root.
#[test]
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "the container driver is Linux-only"
)]
fn a_container_node_opens_its_cache_and_trees_under_the_data_dir() {
    #[cfg(target_os = "linux")]
    {
        use kbf_daemon::cache::{Cache, CacheConfig};
        use std::os::unix::fs::PermissionsExt as _;
        let data = tls("cache-data");
        let (leases, dir) = (data.join("leases"), data.join("cache"));
        let (flags, bin) = container("cache-open", &data);
        let snap = dir.join("v1/index.snap");
        let after_load = || {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            while !snap.exists() {
                assert!(
                    std::time::Instant::now() < deadline,
                    "no snapshot after load"
                );
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            std::fs::remove_file(&snap).expect("remove the snapshot written after load");
        };
        let (status, log) =
            super::runs_until_sigterm_logged_then("cache-open", &flags, Some(&bin), after_load);
        assert!(status.success(), "{status}: {log:#?}");
        assert!(
            logged(&log, &["cache: ", "0 entries, link mode hardlink"]),
            "{log:#?}"
        );
        assert!(snap.exists(), "no index saved at the clean stop");
        let mode = |path: &std::path::Path| {
            std::fs::metadata(path).expect("made").permissions().mode() & 0o777
        };
        // Searchable by the container's ids, which are not the daemon's.
        assert_eq!(mode(&leases), 0o711);
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&data.join("trees")), 0o711);
        assert!(
            logged(&log, &["trees: ", "0 trees"]) && logged(&log, &["min_files=256 max_mounts=8"]),
            "{log:#?}"
        );
        assert!(
            logged(&log, &["data dir ", "leases "])
                && logged(&log, &["cache ", "up to ", "evicting to keep"])
                && logged(&log, &["trees ", "up to "]),
            "the start logs every directory and budget: {log:#?}"
        );
        let held = Cache::open(CacheConfig::new(dir.clone(), leases.clone(), 1000))
            .expect("this test owns it");
        let (flags, bin) = container("cache-locked", &data);
        let (status, log) = runs_until_sigterm_logged("cache-locked", &flags, Some(&bin));
        assert!(status.success(), "{status}: {log:#?}");
        assert!(
            logged(&log, &["cache: off:locked", "leases run uncached"]),
            "{log:#?}"
        );
        drop(held);
    }
}

/// C-19. Catches the native driver on Linux using a cache its own actions could write
/// (the "enable the cache on Linux native" mutant), or making the cache or trees
/// directories: under `--data-dir` it makes `leases` and nothing else, and logs that
/// the cache and trees are off.
#[test]
#[cfg_attr(
    target_os = "macos",
    ignore = "macOS native nodes make the cache and trees: see the clone tests below"
)]
fn a_native_node_never_makes_a_cache_or_trees() {
    let dir = tls("native-cache");
    let flags = [
        "--driver=native".to_owned(),
        "--cas=https://127.0.0.1:1".to_owned(),
        format!("--data-dir={}", dir.display()),
    ];
    let one = one_native_daemon();
    let (status, log) = runs_until_sigterm_logged("native-cache", &flags, None);
    drop(one);
    assert!(status.success(), "{status}: {log:#?}");
    assert!(
        logged(&log, &["cache: off:no driver that uses it", "trees: off"]),
        "{log:#?}"
    );
    assert!(dir.join("leases").is_dir(), "no lease directory");
    for absent in ["cache", "trees"] {
        assert!(
            !dir.join(absent).exists(),
            "the native daemon made {absent}"
        );
    }
}

/// C-19 on macOS. Catches the native driver not opening the cache under `--data-dir`
/// (every lease would fetch its inputs again), a cache that is not opened in clone mode,
/// and a tree store that is not made or is made inside `leases` (which lease sweeps
/// clean): a macOS native node with only `--data-dir` makes `leases`, opens `cache` and
/// logs `link mode clone`, opens `trees` (0711, as the store sets it) outside `leases`
/// and logs that trees are cloned whole.
#[test]
#[cfg_attr(
    not(target_os = "macos"),
    ignore = "only a macOS native node opens the cache and the trees"
)]
fn a_macos_native_node_opens_its_cache_and_trees_under_the_data_dir() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tls("native-data-mac");
    let flags = [
        "--driver=native".to_owned(),
        "--cas=https://127.0.0.1:1".to_owned(),
        format!("--data-dir={}", dir.display()),
    ];
    let one = one_native_daemon();
    let (status, log) = runs_until_sigterm_logged("native-data-mac", &flags, None);
    drop(one);
    assert!(status.success(), "{status}: {log:#?}");
    assert!(logged(&log, &["cache:", "link mode clone"]), "{log:#?}");
    assert!(
        logged(&log, &["trees:", "0 trees", "cloned whole"]),
        "{log:#?}"
    );
    assert!(dir.join("leases").is_dir(), "no lease directory");
    assert!(dir.join("cache/v1").is_dir(), "the cache has no layout");
    let trees = std::fs::metadata(dir.join("trees")).expect("the store was not made");
    assert_eq!(trees.permissions().mode() & 0o777, 0o711);
}

/// The native node's tree store on macOS cannot be opened. Catches a daemon that stops
/// the start for it, or that drops the cache with it: with a file where `trees` goes the
/// node serves, warns `trees: off:unusable` and that inputs are cloned file by file, and
/// still opens the cache.
#[test]
#[cfg_attr(
    not(target_os = "macos"),
    ignore = "only a macOS native node opens a tree store"
)]
fn a_macos_native_node_whose_tree_store_cannot_open_clones_file_by_file() {
    let dir = tls("native-trees-off-mac");
    std::fs::write(dir.join("trees"), b"not a directory").expect("plant a file");
    let flags = [
        "--driver=native".to_owned(),
        "--cas=https://127.0.0.1:1".to_owned(),
        format!("--data-dir={}", dir.display()),
    ];
    let one = one_native_daemon();
    let (status, log) = runs_until_sigterm_logged("native-trees-off-mac", &flags, None);
    drop(one);
    assert!(status.success(), "{status}: {log:#?}");
    assert!(
        logged(&log, &["trees:", "off:unusable", "file by file"]),
        "{log:#?}"
    );
    assert!(logged(&log, &["cache:", "link mode clone"]), "{log:#?}");
    assert!(dir.join("cache/v1").is_dir(), "the cache was not opened");
}

/// Catches the per-piece flags an older binary took still accepted (a unit that was
/// not updated would run with defaults it did not ask for): each exits 2 naming the
/// flag as unexpected, before any driver starts. `--data-max-bytes` needs a number
/// of 1 or more.
#[test]
fn the_per_piece_flags_are_refused() {
    let run = |flag: &str| {
        let out = std::process::Command::new(BIN)
            .args(["--driver=native", "--cas=https://127.0.0.1:1", flag])
            .output()
            .expect("run");
        (
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    };
    for flag in [
        "--scratch=/s",
        "--cache-dir=/c",
        "--cache-max-bytes=9",
        "--cache-min-free-bytes=9",
        "--cache-link-concurrency=9",
        "--tree-dir=/t",
        "--tree-max-bytes=9",
        "--tree-min-files=9",
        "--tree-max-mounts=9",
    ] {
        let (code, stderr) = run(flag);
        assert_eq!(code, Some(2), "{flag}: {stderr}");
        let name = flag.split_once('=').map_or(flag, |(name, _)| name);
        assert!(
            stderr.contains("unexpected argument") && stderr.contains(name),
            "{flag}: {stderr}"
        );
    }
    let (code, stderr) = run("--data-max-bytes=0");
    assert_eq!(code, Some(2), "{stderr}");
    assert!(stderr.contains("--data-max-bytes"), "{stderr}");
}

/// Catches a binary that parses its flags before it answers as the container driver's
/// `__chown-dirs` helper (clap would refuse the mode and the missing `--driver`): the
/// daemon's own ids are given and the walk succeeds; arguments it cannot read exit 2.
#[test]
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "the container driver is Linux-only"
)]
fn the_binary_runs_as_the_chown_dirs_helper_before_it_parses_its_flags() {
    let dir = tls("chown-dirs");
    std::fs::create_dir_all(dir.join("sub")).expect("dir");
    let ids = format!("{}:{}", rustix_ids().0, rustix_ids().1);
    let run = |args: &[&str]| {
        std::process::Command::new(BIN)
            .args(args)
            .output()
            .expect("spawn")
    };
    let ok = run(&["__chown-dirs", &ids, &dir.display().to_string()]);
    assert_eq!(
        ok.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&ok.stderr)
    );
    let bad = run(&["__chown-dirs", "kbf"]);
    assert_eq!(bad.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&bad.stderr).contains("usage"));
}

/// Catches a binary that parses its flags before it answers as the container driver's
/// `__unpack-layers` helper (the OCI backend's image unpack, run in a helper container):
/// given a stage directory, a directory of blobs and one layer, it makes `rootfs` with the
/// layer's file and the mount points the runtime binds over, and exits 0; arguments it
/// cannot read exit 2 with a usage line; a layer it cannot read exits 1.
#[test]
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "the container driver is Linux-only"
)]
fn the_binary_runs_as_the_unpack_layers_helper_before_it_parses_its_flags() {
    let dir = tls("unpack-layers");
    let (stage, blobs) = (dir.join("stage"), dir.join("blobs"));
    std::fs::create_dir_all(&stage).expect("stage");
    std::fs::create_dir_all(&blobs).expect("blobs");
    let (uid, gid) = rustix_ids();
    let mut header = tar::Header::new_gnu();
    header.set_size(5);
    header.set_mode(0o644);
    header.set_uid(u64::from(uid));
    header.set_gid(u64::from(gid));
    header.set_cksum();
    let mut builder = tar::Builder::new(Vec::new());
    builder
        .append_data(&mut header, "etc/os-release", &b"image"[..])
        .expect("append");
    std::fs::write(blobs.join("0.tar"), builder.into_inner().expect("tar")).expect("layer");
    let run = |args: &[&str]| {
        std::process::Command::new(BIN)
            .args(args)
            .output()
            .expect("spawn")
    };
    let (stage_arg, blobs_arg) = (stage.display().to_string(), blobs.display().to_string());
    let bad = run(&["__unpack-layers", &stage_arg]);
    assert_eq!(bad.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&bad.stderr).contains("usage"));
    let ok = run(&["__unpack-layers", &stage_arg, &blobs_arg, "0.tar"]);
    assert_eq!(
        ok.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&ok.stderr)
    );
    assert_eq!(
        std::fs::read(stage.join("rootfs/etc/os-release")).expect("unpacked"),
        b"image"
    );
    for mount_point in ["dev", "proc", "kbf/root", "sys/fs/cgroup"] {
        assert!(
            stage.join("rootfs").join(mount_point).is_dir(),
            "{mount_point}"
        );
    }
    assert!(stage.join("rootfs/etc/resolv.conf").is_file());
    // The root exists now: a second unpack into the same stage is refused, not merged.
    let again = run(&["__unpack-layers", &stage_arg, &blobs_arg, "0.tar"]);
    assert_eq!(again.status.code(), Some(1));
}

/// This process's uid and gid.
fn rustix_ids() -> (u32, u32) {
    use std::os::unix::fs::MetadataExt as _;
    let meta = std::fs::metadata(tls("ids")).expect("a directory of ours");
    (meta.uid(), meta.gid())
}
