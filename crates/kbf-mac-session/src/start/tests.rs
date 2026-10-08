use clap::Parser as _;

use super::*;
use crate::grant::testing::key_line;
use crate::testing::{FakeHost, scratch, trace};

/// A group the test process may give files to, other than `staff` (20): the first of
/// its groups that is not 20, by name and id.
fn my_group() -> (String, u32) {
    let output = |flag: &str| {
        let out = std::process::Command::new("id").arg(flag).output().unwrap();
        String::from_utf8(out.stdout).unwrap()
    };
    let names = output("-Gn");
    let ids = output("-G");
    names
        .split_whitespace()
        .zip(ids.split_whitespace())
        .map(|(name, id)| (name.to_owned(), id.parse().unwrap()))
        .find(|&(_, id)| id != STAFF_GID)
        .expect("a group other than staff")
}

/// The name of the group with id `gid`, from `/etc/group`.
fn group_named(gid: u32) -> Option<String> {
    let text = std::fs::read_to_string("/etc/group").ok()?;
    text.lines().find_map(|line| {
        let fields: Vec<&str> = line.split(':').collect();
        (fields.len() > 2 && fields[2] == gid.to_string()).then(|| fields[0].to_owned())
    })
}

/// Mode 0700: private whatever the test's umask.
fn private(path: &Path) {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
}

fn args(dir: &Path, group: &str) -> Args {
    Args::parse_from([
        "kbf-mac-session",
        "--socket",
        &dir.join("run/socket").to_string_lossy(),
        "--socket-group",
        group,
        "--state-dir",
        &dir.join("state").to_string_lossy(),
        "--uid-range",
        "700-709",
        "--homes",
        &dir.to_string_lossy(),
        "--daemon-requirement",
        "cdhash H\"00\"",
    ])
}

fn prepared(args: &Args) -> Result<Ready, String> {
    trace();
    prepare(
        args,
        Box::new(FakeHost::default()),
        "S".to_owned(),
        crate::sweep::SweepPlan::macos_schedules(),
        crate::sweep::SweepPlan::macos(),
    )
}

#[test]
fn the_flags_default_to_the_design() {
    let args = Args::parse_from(["kbf-mac-session", "--daemon-requirement", "r"]);
    assert_eq!(
        args.socket,
        PathBuf::from("/var/run/kbf-mac-session/socket")
    );
    assert_eq!(args.socket_group, "_kbf");
    assert_eq!(args.uid_range.to_string(), "600-699");
    assert_eq!(args.lease_gid, STAFF_GID);
    assert_eq!(args.homes, PathBuf::from("/Users"));
    assert_eq!(args.grant_keys, None);
    assert!(Args::try_parse_from(["kbf-mac-session"]).is_err());
    assert!(
        Args::try_parse_from([
            "kbf-mac-session",
            "--daemon-requirement",
            "r",
            "--uid-range",
            "1-2"
        ])
        .is_err()
    );
}

/// Catches: a start-up that skips the state directory's privacy or the socket's
/// group, or one that ignores the grant key file.
#[test]
fn prepare_makes_private_state_and_binds_the_socket() {
    let dir = scratch("start-ok");
    let (group, gid) = my_group();
    let mut args = args(&dir, &group);
    let keys = dir.join("grant-keys");
    std::fs::write(&keys, key_line(3)).unwrap();
    private(&keys);
    args.grant_keys = Some(keys);
    let ready = prepared(&args).unwrap();
    drop(ready.listener);
    let state = std::fs::metadata(dir.join("state")).unwrap();
    assert_eq!(
        std::os::unix::fs::PermissionsExt::mode(&state.permissions()) & 0o777,
        0o700
    );
    assert!(dir.join("state/ledger").is_file());
    let socket = std::fs::metadata(dir.join("run/socket")).unwrap();
    assert_eq!(std::os::unix::fs::MetadataExt::gid(&socket), gid);
    // Started again over the same state.
    prepared(&args).unwrap();
}

/// Catches: the socket's group allowed to be `staff` (S4.3: every macOS user is in
/// it) or the lease users' own group, which would let lease users reach the helper.
#[test]
fn the_socket_group_must_be_dedicated() {
    let dir = scratch("start-group");
    let (group, gid) = my_group();
    let mut same = args(&dir, &group);
    same.lease_gid = gid;
    let error = prepared(&same).err().unwrap();
    assert!(error.contains("must be the daemon's own"), "{error}");
    if let Some(staff) = group_named(STAFF_GID) {
        let error = prepared(&args(&dir, &staff)).err().unwrap();
        assert!(error.contains("must be the daemon's own"), "{error}");
    }
    let error = prepared(&args(&dir, "no-such-group-kbf")).err().unwrap();
    assert!(error.contains("no group named"), "{error}");
    let error = prepared(&args(&dir, "nul\0group")).err().unwrap();
    assert!(error.contains("nul"), "{error}");
}

#[test]
fn bad_state_keys_or_socket_stop_the_start() {
    let dir = scratch("start-bad");
    let (group, _) = my_group();
    let good = args(&dir, &group);

    let mut missing_keys = good.clone();
    missing_keys.grant_keys = Some(dir.join("no-such-keys"));
    assert!(
        prepared(&missing_keys)
            .err()
            .unwrap()
            .contains("no-such-keys")
    );
    let mut bad_keys = good.clone();
    std::fs::write(dir.join("bad-keys"), "zz\n").unwrap();
    private(&dir.join("bad-keys"));
    bad_keys.grant_keys = Some(dir.join("bad-keys"));
    assert!(prepared(&bad_keys).err().unwrap().contains("line 1"));

    let mut orphan = good.clone();
    orphan.state_dir = dir.join("no/such/state");
    assert!(prepared(&orphan).err().unwrap().contains("no/such/state"));
    std::fs::create_dir(dir.join("real-state")).unwrap();
    std::os::unix::fs::symlink(dir.join("real-state"), dir.join("linked-state")).unwrap();
    let mut linked = good.clone();
    linked.state_dir = dir.join("linked-state");
    assert!(prepared(&linked).is_err());

    let mut corrupt = good.clone();
    corrupt.state_dir = dir.join("corrupt");
    std::fs::create_dir(dir.join("corrupt")).unwrap();
    std::fs::write(dir.join("corrupt/ledger"), "nonsense\n").unwrap();
    private(&dir.join("corrupt"));
    private(&dir.join("corrupt/ledger"));
    assert!(prepared(&corrupt).err().unwrap().contains("unknown record"));

    let mut unbindable = good;
    unbindable.socket = dir.join("bad-keys/socket");
    assert!(
        prepared(&unbindable)
            .err()
            .unwrap()
            .contains("bad-keys/socket")
    );
}

/// Catches: trusted files read with no check of who may write them (S4.3, item 6 of
/// the review): a grant key file, state directory or ledger that another user owns,
/// or that its group or others may write, or a key file reached through a link. Any
/// of them would let someone else make administrators or replay a grant.
#[test]
fn trusted_files_must_be_the_helpers_own_and_private() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = scratch("start-trusted");
    let (group, _) = my_group();
    let good = args(&dir, &group);
    let mode = |path: &Path, mode: u32| {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    };

    let keys = dir.join("grant-keys");
    std::fs::write(&keys, key_line(3)).unwrap();
    let mut with_keys = good.clone();
    with_keys.grant_keys = Some(keys.clone());
    mode(&keys, 0o644);
    prepared(&with_keys).unwrap();
    mode(&keys, 0o664);
    let error = prepared(&with_keys).err().unwrap();
    assert!(
        error.contains("writable by its group or others (mode 664)"),
        "{error}"
    );
    mode(&keys, 0o646);
    assert!(prepared(&with_keys).err().unwrap().contains("writable by"));
    mode(&keys, 0o600);

    std::os::unix::fs::symlink(&keys, dir.join("linked-keys")).unwrap();
    let mut linked = good.clone();
    linked.grant_keys = Some(dir.join("linked-keys"));
    assert!(prepared(&linked).err().unwrap().contains("linked-keys"));
    let mut a_dir = good.clone();
    a_dir.grant_keys = Some(dir.clone());
    assert!(
        prepared(&a_dir)
            .err()
            .unwrap()
            .contains("not a regular file")
    );

    // State: the directory and the ledger in it.
    let state = dir.join("state");
    mode(&state, 0o770);
    let error = prepared(&good).err().unwrap();
    assert!(error.contains("state: writable by"), "{error}");
    mode(&state, 0o700);
    let ledger = state.join("ledger");
    mode(&ledger, 0o622);
    let error = prepared(&good).err().unwrap();
    assert!(error.contains("ledger: writable by"), "{error}");
    mode(&ledger, 0o600);
    prepared(&good).unwrap();

    // Another user's: root's own files, seen by a test that is not root.
    if rustix::process::geteuid().as_raw() != 0 {
        let mut foreign_keys = good.clone();
        foreign_keys.grant_keys = Some(PathBuf::from("/etc/passwd"));
        let error = prepared(&foreign_keys).err().unwrap();
        assert!(error.contains("owned by uid 0"), "{error}");
        let mut foreign_state = good;
        foreign_state.state_dir = PathBuf::from("/");
        let error = prepared(&foreign_state).err().unwrap();
        assert!(error.contains("owned by uid 0"), "{error}");
    }
}

/// Catches: a serial taken from the wrong line, or a value with quotes or spaces.
#[test]
fn the_serial_comes_from_its_ioreg_line() {
    let ioreg = "+-o J316sAP  <class IOPlatformExpertDevice>\n    {\n      \"IOPlatformUUID\" = \"0000-1111\"\n      \"IOPlatformSerialNumber\" = \"C02ABC123\"\n    }\n";
    assert_eq!(parse_serial(ioreg), Some("C02ABC123".to_owned()));
    for bad in [
        "",
        "\"IOPlatformSerialNumber\" = \"\"",
        "\"IOPlatformSerialNumber\" = \"A B\"",
        "\"IOPlatformSerialNumber\" = \"ABC",
        "\"IOPlatformSerialNumbe\" = \"ABC\"",
    ] {
        assert_eq!(parse_serial(bad), None, "{bad:?}");
    }
}

#[cfg(not(target_os = "macos"))]
#[test]
fn main_serves_only_on_macos() {
    let dir = scratch("start-main");
    assert_eq!(main(&args(&dir, "g")), ExitCode::from(2));
}
