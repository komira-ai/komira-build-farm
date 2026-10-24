//! The assertion's parsers on fixtures of what the Mac's tools print, its comparison with
//! a baseline, and what a red result does to the slot book: all on every platform, with a
//! fake [`Reader`].

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use super::*;
use crate::slots::book::{Backoff, Book, Cleaner, Slotted};
use crate::slots::clean::Outcome;

fn lines(texts: &[&str]) -> Lines {
    texts.iter().map(|t| (*t).to_owned()).collect()
}

/// A reader that answers what a test put in it, and counts nothing it was not asked.
#[derive(Default)]
struct Fake(Mutex<BTreeMap<Probe, Result<Lines, String>>>);

impl Fake {
    /// A user with nothing wrong: every probe answers its clean lines.
    fn clean() -> Self {
        let fake = Self::default();
        for probe in ALL {
            fake.set(probe, &clean_lines(probe));
        }
        fake
    }

    fn set(&self, probe: Probe, answer: &[&str]) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(probe, Ok(lines(answer)));
    }

    fn fail(&self, probe: Probe, why: &str) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(probe, Err(why.to_owned()));
    }
}

impl Reader for Fake {
    fn read(&self, probe: Probe) -> Result<Lines, String> {
        let answers = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        answers
            .get(&probe)
            .cloned()
            .unwrap_or_else(|| Err("not answered".to_owned()))
    }
}

/// What a clean used-once user reads for `probe`.
fn clean_lines(probe: Probe) -> Vec<&'static str> {
    match probe {
        Probe::Entry => vec![". owned by 61001:20 mode 700 flags 0x0"],
        Probe::Record => vec!["UniqueID=61001", "id uid=61001(u) gid=20(staff)"],
        Probe::Services => vec![
            "com.apple.cfprefsd.xpc.agent\t/System/Library/LaunchAgents/com.apple.cfprefsd.xpc.agent.plist\t/usr/sbin/cfprefsd",
            "com.apple.distnoted.xpc.agent\t/System/Library/LaunchAgents/com.apple.distnoted.xpc.agent.plist\t",
        ],
        _ => Vec::new(),
    }
}

fn baseline() -> Snapshot {
    record(&Fake::clean()).expect("a clean user is a baseline")
}

const PS: &str = "\
    1     0     0     0 Ss   /sbin/launchd
  345   501   501   501 Ss   /usr/libexec/distnoted
61001 61001 61001 61001 S    /bin/sleep
61002 61001     0     0 S    /usr/bin/su
61003     0     0 61001 S    /x/saved uid
61004 61001 61001 61001 Z    /bin/dead
61005     0 61001     0 S    /x/effective only
";

/// Catches a process table read as the uid alone (a setuid-root program the uid started,
/// whose effective uid is root's, would not be found; nor one that dropped to the uid as
/// its saved uid), a zombie counted as a process (it cannot be killed and goes when its
/// parent reaps it), a garbled table taken as an empty one, and a command with spaces cut.
#[test]
fn processes_are_found_by_real_effective_or_saved_uid() {
    let procs = parse_ps(PS).expect("parsed");
    assert_eq!(procs.len(), 7);
    assert_eq!(
        uid_processes(&procs, 61001, &[]),
        lines(&[
            "61001 /bin/sleep",
            "61002 /usr/bin/su",
            "61003 /x/saved uid",
            "61005 /x/effective only"
        ])
    );
    assert!(uid_processes(&procs, 61005, &[]).is_empty());
    assert!(parse_ps("").is_err(), "an empty table is a failed read");
    assert!(parse_ps("12 x 0 0 S /bin/sh").is_err());
    assert!(parse_ps("12 0 0").is_err());
    assert!(parse_ps("12 0 0 0").is_err(), "no state");
    assert!(
        parse_ps("99999999999 0 0 0 S /x").is_err(),
        "a pid past i32"
    );
}

/// Catches launchd's own agents counted as a lease's processes (they start again on
/// demand as the uid after every kill): the pids of own services are left out, and only
/// those.
#[test]
fn launchds_own_processes_are_not_the_leases() {
    let procs = parse_ps(PS).expect("parsed");
    assert!(uid_processes(&procs, 501, &[345]).is_empty());
    assert_eq!(
        uid_processes(&procs, 501, &[]),
        lines(&["345 /usr/libexec/distnoted"])
    );
    assert_eq!(uid_processes(&procs, 61001, &[61001]).len(), 3);
}

fn job(label: &str, pid: Option<i32>, own: bool) -> Job {
    Job {
        pid,
        label: label.to_owned(),
        plist: Some(format!("/System/Library/LaunchAgents/{label}.plist")),
        program: None,
        own,
    }
}

/// Catches a job judged by its label (a lease can name its agent `com.apple.x`): the
/// judgement is the job's `own`, which the reader takes from its plist path, so a
/// foreign job labelled like launchd's is listed and an own one is not; the pids of own
/// services are those the processes probe leaves out.
#[test]
fn a_job_is_launchds_own_by_its_plist_not_its_label() {
    let jobs = [
        job("com.apple.cfprefsd.xpc.agent", Some(77), true),
        job("com.apple.kbf-fresh", Some(88), false),
        job("kbf.fresh", None, false),
    ];
    assert_eq!(
        foreign_jobs(&jobs),
        lines(&["job com.apple.kbf-fresh (pid 88)", "job kbf.fresh"])
    );
    assert_eq!(own_pids(&jobs), vec![77]);
    assert_eq!(
        own_services(&jobs),
        lines(&[
            "com.apple.cfprefsd.xpc.agent\t/System/Library/LaunchAgents/com.apple.cfprefsd.xpc.agent.plist\t"
        ])
    );
}

const IPCS: &str = "\
IPC status from <running system> as of Thu Oct 22 10:00:00 UTC 2026
T     ID     KEY        MODE       OWNER    GROUP  CREATOR   CGROUP CBYTES  QNUM QBYTES LSPID LRPID STIME    RTIME    CTIME
Message Queues:
q  none 0x00000009 --rw------- alice staff alice staff 0 0 2048 0 0 no-entry no-entry 10:00:00
q  100 0x00000001 --rw------- alice staff alice staff 0 0 2048 0 0 no-entry no-entry 10:00:00
Shared Memory:
m  200 0x00000002 --rw------- root wheel kbf-s staff 4096 0 0 0 0 no-entry no-entry 10:00:00
m  201 0x00000003 --rw------- kbf-s staff root wheel 4096 0 0 0 0 no-entry no-entry 10:00:00
Semaphores:
s  300 0x00000004 --rw------- 61001 staff 61001 staff 0 0 0 0 0 no-entry no-entry 10:00:00
s  301 0x00000005 --rw------- bob staff bob staff 0 0 0 0 0 no-entry no-entry 10:00:00
";

/// Catches `ipcs` rows matched on the owner alone (an object the uid made and gave away
/// stays), on the name alone (a name the user database does not give is its uid), or the
/// header lines and section titles taken as objects.
#[test]
fn ipc_objects_are_found_by_owner_or_creator() {
    let ids = |name, uid| -> Vec<(char, String)> {
        ipc_rows(IPCS, name, uid)
            .into_iter()
            .map(|r| (r.kind, r.id))
            .collect()
    };
    assert_eq!(
        ids("kbf-s", 61001),
        vec![
            ('m', "200".to_owned()),
            ('m', "201".to_owned()),
            ('s', "300".to_owned())
        ]
    );
    assert_eq!(ids("alice", 5), vec![('q', "100".to_owned())]);
    assert!(ids("nobody", 9).is_empty());
}

const HDIUTIL: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict><key>images</key><array>
<dict><key>image-path</key><string>/private/var/pool/u/a.dmg</string><key>owner-uid</key><integer>61001</integer>
<key>system-entities</key><array>
<dict><key>dev-entry</key><string>/dev/disk4s1</string></dict>
<dict><key>dev-entry</key><string>/dev/disk4</string></dict></array></dict>
<dict><key>image-path</key><string>/private/var/folders/xx/yy/T/b.dmg</string><key>owner-uid</key><integer>0</integer>
<key>system-entities</key><array>
<dict><key>dev-entry</key><string>/dev/disk5</string><key>mount-point</key><string>/private/var/folders/xx/yy/T/vol</string></dict></array></dict>
<dict><key>image-path</key><string>/Applications/other.dmg</string><key>owner-uid</key><integer>0</integer>
<key>system-entities</key><array>
<dict><key>dev-entry</key><string>/dev/disk6</string><key>mount-point</key><string>/Volumes/other</string></dict></array></dict>
<dict><key>image-path</key><string>/Applications/third.dmg</string><key>owner-uid</key><integer>0</integer>
<key>system-entities</key><array>
<dict><key>dev-entry</key><string>/dev/disk7</string><key>mount-point</key><string>/private/var/pool/u/vol</string></dict></array></dict>
</array></dict></plist>"#;

/// Catches an attached image the uid made with no mount point (`attach -nomount` shows in
/// no `mount` line), an image or a mount under the folders the clean empties, the
/// partition taken for the image (the whole device is the shortest), and an unrelated
/// image or mount counted.
#[test]
fn mounts_and_attached_images_under_the_users_folders_are_found() {
    let images = hdiutil_images(HDIUTIL.as_bytes()).expect("a property list");
    assert_eq!(images.len(), 4);
    assert_eq!(images[0].device.as_deref(), Some("/dev/disk4"));
    assert_eq!(images[0].owner, Some(61001));
    assert_eq!(images[1].mounts, vec!["/private/var/folders/xx/yy/T/vol"]);
    let mount = "\
/dev/disk3s1 on / (apfs, sealed, local)
/dev/disk5 on /private/var/folders/xx/yy/T/vol (hfs, local)
/dev/disk6 on /Volumes/other (hfs, local)
";
    let home = Path::new("/private/var/pool/u");
    let folders = Path::new("/private/var/folders/xx/yy");
    assert_eq!(
        mount_lines(mount, &images, (61001, "u"), &[home, folders]),
        lines(&[
            "mounted /private/var/folders/xx/yy/T/vol",
            "image /dev/disk4 attached by the uid",
            "image /dev/disk5 under the user's folders",
            "image /dev/disk7 under the user's folders",
        ])
    );
    assert!(
        mount_lines(
            "/dev/d on / (apfs)\n",
            &images[2..3],
            (61001, "u"),
            &[home, folders]
        )
        .is_empty()
    );
    assert!(hdiutil_images(b"not a plist").is_err());
}

/// Catches a mount the user made anywhere but under its own folders (an smb or nfs share
/// on a directory in `/private/tmp`): macOS marks such a mount `mounted by <name>`, and
/// that is a line, once, wherever the mount is; another user's mount, and a name that only
/// starts like the user's, are not.
#[test]
fn a_mount_marked_as_mounted_by_the_user_is_found_wherever_it_is() {
    let mount = "\
//u@host/share on /private/tmp/share (smbfs, nodev, nosuid, mounted by u)
//v@host/other on /private/tmp/other (smbfs, nodev, nosuid, mounted by v)
//w@host/third on /private/tmp/third (smbfs, nodev, nosuid, mounted by uu)
//u@host/under on /private/var/pool/u/mnt (smbfs, nodev, nosuid, mounted by u)
";
    let home = Path::new("/private/var/pool/u");
    assert_eq!(
        mount_lines(mount, &[], (61001, "u"), &[home]),
        lines(&[
            "mounted /private/var/pool/u/mnt",
            "mounted /private/tmp/share by the user",
        ])
    );
    assert_eq!(
        mounted_by(mount, "v"),
        vec![PathBuf::from("/private/tmp/other")]
    );
}

/// Catches an ACL on the home read as the listing's first line (the directory itself),
/// and a listing with none read as one.
#[test]
fn acl_entries_are_the_numbered_lines_after_the_first() {
    let listing = "drwx------+ 2 u staff 64 Oct 22 10:00 /home\n 0: user:x allow list,add_file\n 1: group:y deny delete\n";
    assert_eq!(
        acl_entries(listing),
        lines(&["0: user:x allow list,add_file", "1: group:y deny delete"])
    );
    assert!(acl_entries("drwx------ 2 u staff 64 Oct 22 10:00 /home\n").is_empty());
    // Lines that look a little like an entry are not one.
    let odd = "drwx------ 2 u staff 64 Oct 22 10:00 /home\ntotal 3\nname: value\n: nothing\n 7: \n";
    assert!(acl_entries(odd).is_empty());
}

fn facts() -> Facts {
    Facts {
        uid: 61001,
        gid: 20,
        mode: 0o700,
        ..Facts::default()
    }
}

/// Catches a home that is judged on part of what the assertion promises: each of the
/// owner, the group, the mode, a flag, an ACL, an extended attribute and a file makes a
/// line of its own, and an empty home of the user's makes none.
#[test]
fn a_home_is_empty_0700_owned_and_has_no_flag_acl_or_xattr() {
    assert!(home_lines(&facts(), (61001, 20)).is_empty());
    let cases: [(Facts, &str); 7] = [
        (Facts { uid: 0, ..facts() }, "owned by 0:20, not 61001:20"),
        (
            Facts { gid: 80, ..facts() },
            "owned by 61001:80, not 61001:20",
        ),
        (
            Facts {
                mode: 0o755,
                ..facts()
            },
            "mode 755, not 700",
        ),
        (
            Facts {
                flags: 0x2,
                ..facts()
            },
            "flags 0x2",
        ),
        (
            Facts {
                acl: lines(&["0: user:x allow list"]),
                ..facts()
            },
            "acl 0: user:x allow list",
        ),
        (
            Facts {
                xattrs: lines(&["com.apple.x"]),
                ..facts()
            },
            "xattr com.apple.x",
        ),
        (
            Facts {
                entries: lines(&["a.txt"]),
                ..facts()
            },
            "entry a.txt",
        ),
    ];
    for (found, line) in cases {
        assert_eq!(home_lines(&found, (61001, 20)), lines(&[line]), "{line}");
    }
}

/// Catches an entry that is only listed (a name in the entry root that is not one of the
/// four folders stays, a folder with a file in it stays), one whose folders another user
/// owns, and a root recorded without its owner, mode and flags.
#[test]
fn the_entry_keeps_its_four_folders_empty_and_nothing_else() {
    let root = Facts {
        mode: 0o755,
        ..facts()
    };
    let kept = |name: &str| (name.to_owned(), true, facts());
    let skeleton = ["0", "C", "T", "X"];
    let clean = entry_lines(&root, &[kept("0"), kept("C"), kept("T")], &skeleton, 61001);
    assert_eq!(
        clean,
        lines(&[
            ". owned by 61001:20 mode 755 flags 0x0",
            ". 0 mode 700",
            ". 0 flags 0x0",
            ". C mode 700",
            ". C flags 0x0",
            ". T mode 700",
            ". T flags 0x0",
        ])
    );
    let full = (
        "T".to_owned(),
        true,
        Facts {
            entries: lines(&["leak"]),
            ..facts()
        },
    );
    let stray = ("stray".to_owned(), false, Facts::default());
    let dir_stray = ("Z".to_owned(), true, facts());
    let foreign = ("0".to_owned(), true, Facts { uid: 0, ..facts() });
    let locked = (
        "C".to_owned(),
        true,
        Facts {
            flags: 0x2,
            ..facts()
        },
    );
    let found = entry_lines(
        &root,
        &[full, stray, dir_stray, foreign, locked],
        &skeleton,
        61001,
    );
    assert_eq!(
        found,
        lines(&[
            ". owned by 61001:20 mode 755 flags 0x0",
            ". T mode 700",
            ". T flags 0x0",
            "T: entry leak",
            "stray: not part of the entry",
            "Z: not part of the entry",
            ". 0 mode 700",
            "0: owned by 0",
            ". 0 flags 0x0",
            ". C mode 700",
            ". C flags 0x2",
        ])
    );
}

/// Catches a record read with the wrong attribute (`RecordName` for `RealName`), a value
/// on the line after its key lost, and an attribute that vanished compared as equal:
/// missing is an empty value, which differs.
#[test]
fn the_directory_record_is_read_by_attribute() {
    let said = "NFSHomeDirectory: /private/var/pool/x\nPrimaryGroupID: 20\nnot a pair\n indented: 1\nRealName:\n The X\nRecordName: x\nUniqueID: 61001\nUserShell:\n";
    assert_eq!(
        dscl_lines(said, &["RecordName", "UniqueID", "RealName", "UserShell"]),
        lines(&[
            "RecordName=x",
            "UniqueID=61001",
            "RealName=The X",
            "UserShell="
        ])
    );
}

/// Catches a baseline taken of a user that is not clean (a leftover process would be
/// recorded as normal and never found again), a probe that could not run recorded as an
/// empty one, and a baseline that leaves out the lines a later read is compared with.
#[test]
fn a_baseline_is_recorded_only_of_a_clean_user_and_every_probe() {
    let snapshot = baseline();
    assert_eq!(snapshot.lines(Probe::Record).map(Vec::len), Some(2));
    assert!(snapshot.lines(Probe::Services).is_some());
    for probe in ALL {
        assert!(snapshot.lines(probe).is_some(), "{probe}");
    }
    let dirty = Fake::clean();
    dirty.set(Probe::Processes, &["61001 /bin/sleep"]);
    assert_eq!(
        record(&dirty).expect_err("not clean"),
        lines(&["processes: + 61001 /bin/sleep"])
    );
    let blind = Fake::clean();
    blind.fail(Probe::Ipc, "ipcs: not found");
    let wrong = record(&blind).expect_err("a probe that cannot run");
    assert_eq!(wrong.len(), 1);
    assert!(wrong[0].starts_with("ipc: cannot run"), "{wrong:?}");
}

/// Catches a baseline of a recorded probe that accepts a wrong state as a recorded fact:
/// a home with another mode or a file in it, and an entry with a name that is not one of
/// its folders, are not baselines (the user is not clean), while a home with an ACL or an
/// extended attribute is, and a later home without it differs.
#[test]
fn a_recorded_baseline_holds_facts_and_not_a_wrong_state() {
    for (probe, wrong) in [
        (Probe::Home, "mode 755, not 700"),
        (Probe::Home, "entry leftover"),
        (Probe::Home, "flags 0x2"),
        (Probe::Entry, "stray: not part of the entry"),
        (Probe::Entry, "T: entry leak"),
    ] {
        let user = Fake::clean();
        let mut found = clean_lines(probe);
        found.push(wrong);
        user.set(probe, &found);
        assert_eq!(
            record(&user).expect_err("not a baseline"),
            lines(&[&format!("{probe}: + {wrong}")]),
            "{probe}"
        );
    }
    let user = Fake::clean();
    user.set(
        Probe::Home,
        &[
            "acl 0: group:everyone deny delete",
            "xattr com.apple.provenance",
        ],
    );
    user.set(
        Probe::Entry,
        &[
            ". owned by 61001:20 mode 700 flags 0x0",
            ". T xattr com.apple.x",
        ],
    );
    let base = record(&user).expect("facts are recorded");
    assert!(assert(&user, Some(&base), &ALL, (&|| false, None)).is_empty());
    // The attribute gone and a new one are both differences.
    let now = Fake::clean();
    now.set(
        Probe::Home,
        &["acl 0: group:everyone deny delete", "xattr kbf.new"],
    );
    now.set(
        Probe::Entry,
        &[
            ". owned by 61001:20 mode 700 flags 0x0",
            ". T xattr com.apple.x",
        ],
    );
    assert_eq!(
        assert(
            &now,
            Some(&base),
            &[Probe::Home, Probe::Entry],
            (&|| false, None)
        ),
        lines(&[
            "home: + xattr kbf.new",
            "home: - xattr com.apple.provenance"
        ])
    );
}

/// Catches a check that goes on reading after the lease was killed: once `stopped` says
/// so no further probe runs, and the result is red and says why.
#[test]
fn a_stopped_check_reads_no_further_probe() {
    let base = baseline();
    let reads = std::cell::Cell::new(0);
    let stopped = || {
        reads.set(reads.get() + 1);
        reads.get() > 2
    };
    let red = assert(&Fake::clean(), Some(&base), &CHEAP, (&stopped, None));
    assert_eq!(reads.get(), 3);
    assert_eq!(red, lines(&["ipc: not read, the check was stopped"]));
}

/// Catches a job told to be launchd's own by a line of text it can write, or by a path
/// that only looks like a system one: the plist must really lie under the system folder
/// (a `..` or a link out of it does not), be owned by the system's owner and not
/// writable by others, and carry the job's label (a real system path put in a job's
/// description does not make it that service). XML and binary plists are both read.
#[test]
fn a_plist_is_launchds_own_only_by_the_file_it_names() {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _, symlink};
    let dir = kbf_testkit::dir::TestDir::under(std::env::temp_dir(), "kbf-driver-native-plist");
    let base = std::fs::canonicalize(dir.path()).expect("the directory");
    let (system, outside) = (base.join("system"), base.join("outside"));
    std::fs::create_dir_all(&system).expect("system");
    std::fs::create_dir_all(&outside).expect("outside");
    let owner = std::fs::metadata(&base).expect("meta").uid();
    let roots = [format!("{}/", system.display())];
    let roots: Vec<&str> = roots.iter().map(String::as_str).collect();
    let xml = |label: &str| {
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?><plist version=\"1.0\"><dict><key>Label</key><string>{label}</string></dict></plist>"
        )
    };
    let write = |path: &Path, text: &str, mode: u32| {
        std::fs::write(path, text).expect("plist");
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("mode");
        path.to_string_lossy().into_owned()
    };
    let good = write(&system.join("a.plist"), &xml("com.apple.a"), 0o644);
    assert!(own_plist(&good, "com.apple.a", &roots, owner));
    // Another label: a system path in a job's description does not make it that service.
    assert!(!own_plist(&good, "kbf.job", &roots, owner));
    // Outside the folder, by `..` and by a link.
    let outer = write(&outside.join("b.plist"), &xml("com.apple.b"), 0o644);
    assert!(own_plist(
        &outer,
        "com.apple.b",
        &[&format!("{}/", outside.display())],
        owner
    ));
    assert!(!own_plist(&outer, "com.apple.b", &roots, owner));
    let dotdot = format!("{}/../outside/b.plist", system.display());
    assert!(!own_plist(&dotdot, "com.apple.b", &roots, owner));
    symlink(&outer, system.join("link.plist")).expect("link");
    let linked = system.join("link.plist").to_string_lossy().into_owned();
    assert!(!own_plist(&linked, "com.apple.b", &roots, owner));
    // Owned by another, writable by the group or others, missing, not a plist.
    assert!(!own_plist(&good, "com.apple.a", &roots, owner + 1));
    let group = write(&system.join("g.plist"), &xml("com.apple.g"), 0o664);
    assert!(!own_plist(&group, "com.apple.g", &roots, owner));
    let other = write(&system.join("o.plist"), &xml("com.apple.o"), 0o646);
    assert!(!own_plist(&other, "com.apple.o", &roots, owner));
    let missing = format!("{}/missing.plist", system.display());
    assert!(!own_plist(&missing, "x", &roots, owner));
    let junk = write(&system.join("j.plist"), "not a plist", 0o644);
    assert!(!own_plist(&junk, "x", &roots, owner));
    let empty = write(
        &system.join("e.plist"),
        "<plist version=\"1.0\"><dict/></plist>",
        0o644,
    );
    assert!(!own_plist(&empty, "x", &roots, owner));
    // A binary plist is read as well.
    let mut dict = plist::Dictionary::new();
    dict.insert(
        "Label".to_owned(),
        plist::Value::String("com.apple.bin".to_owned()),
    );
    let binary = system.join("bin.plist");
    plist::Value::Dictionary(dict)
        .to_file_binary(&binary)
        .expect("binary");
    let binary = binary.to_string_lossy().into_owned();
    assert!(own_plist(&binary, "com.apple.bin", &roots, owner));
}

/// Catches a comparison that tolerates nothing launchd varies (a pid, a service loaded
/// on demand, a service that was not loaded yet) and one that tolerates too much: the
/// services differ from the baseline's and the user is still clean, while a foreign job,
/// a process, an IPC object, a mount, a dirty home, a changed record, a changed entry
/// root and an owned file each make a red line of their own.
#[test]
fn the_comparison_tolerates_launchds_services_and_nothing_a_lease_can_make() {
    let base = baseline();
    let now = Fake::clean();
    now.set(
        Probe::Services,
        &["com.apple.other.agent\t/System/x.plist\t"],
    );
    assert!(
        assert(&now, Some(&base), &ALL, (&|| false, None)).is_empty(),
        "launchd's own services vary"
    );
    let cases: [(Probe, &[&str], &str); 8] = [
        (
            Probe::Processes,
            &["9 /bin/sleep"],
            "processes: + 9 /bin/sleep",
        ),
        (Probe::Domain, &["job kbf.x"], "launchd: + job kbf.x"),
        (Probe::Ipc, &["s 300"], "ipc: + s 300"),
        (Probe::Mounts, &["mounted /m"], "mounts: + mounted /m"),
        (Probe::Home, &["entry a"], "home: + entry a"),
        (
            Probe::Shared,
            &["/private/tmp/x"],
            "shared: + /private/tmp/x",
        ),
        (
            Probe::Schedules,
            &["/private/var/at/tabs/u"],
            "schedules: + /private/var/at/tabs/u",
        ),
        (
            Probe::Record,
            &["UniqueID=61002", "id uid=61001(u) gid=20(staff)"],
            "record: + UniqueID=61002",
        ),
    ];
    for (probe, found, line) in cases {
        let now = Fake::clean();
        now.set(probe, found);
        let red = assert(&now, Some(&base), &ALL, (&|| false, None));
        assert!(red.contains(&line.to_owned()), "{probe}: {red:?}");
    }
    let now = Fake::clean();
    now.set(Probe::Record, &["id uid=61001(u) gid=20(staff)"]);
    let red = assert(&now, Some(&base), &[Probe::Record], (&|| false, None));
    assert_eq!(
        red,
        lines(&["record: - UniqueID=61001"]),
        "a line gone is a difference"
    );
    let now = Fake::clean();
    now.set(Probe::Entry, &[". owned by 61001:20 mode 700 flags 0x2"]);
    let red = assert(&now, Some(&base), &[Probe::Entry], (&|| false, None));
    assert_eq!(
        red,
        lines(&[
            "entry: + . owned by 61001:20 mode 700 flags 0x2",
            "entry: - . owned by 61001:20 mode 700 flags 0x0"
        ])
    );
}

/// Catches a probe that cannot run counted as green (the most dangerous way for the
/// assertion to fail), and one that needs a baseline passing without one.
#[test]
fn a_probe_that_cannot_run_or_has_no_baseline_is_red() {
    let base = baseline();
    for probe in ALL {
        let now = Fake::clean();
        now.fail(probe, "boom");
        let red = assert(&now, Some(&base), &[probe], (&|| false, None));
        assert_eq!(red.len(), 1, "{probe}: {red:?}");
        assert!(
            red[0].starts_with(&format!("{probe}: cannot run")),
            "{red:?}"
        );
        assert!(red[0].contains("boom"));
    }
    let red = assert(
        &Fake::clean(),
        None,
        &[Probe::Home, Probe::Entry, Probe::Record, Probe::Ipc],
        (&|| false, None),
    );
    assert_eq!(
        red,
        lines(&[
            "home: no baseline to compare with",
            "entry: no baseline to compare with",
            "record: no baseline to compare with"
        ])
    );
    assert!(assert(&Fake::clean(), None, &[Probe::Ipc], (&|| false, None)).is_empty());
}

/// Catches a cheap subset that leaves out one of the probes a slot is handed over with
/// (processes, launchd's jobs, IPC, mounts, the home and the entry), one that runs the
/// slow walks of the shared places at every lease, and a full list that misses a probe
/// or reads one twice.
#[test]
fn the_cheap_probes_are_the_hand_over_ones_and_all_probes_are_read_once() {
    for probe in [
        Probe::Processes,
        Probe::Domain,
        Probe::Ipc,
        Probe::Mounts,
        Probe::Home,
        Probe::Entry,
    ] {
        assert!(CHEAP.contains(&probe), "{probe}");
    }
    assert_eq!(CHEAP.len(), 6);
    for probe in [Probe::Shared, Probe::Schedules, Probe::Record] {
        assert!(!CHEAP.contains(&probe));
    }
    let mut all = ALL.to_vec();
    all.sort();
    all.dedup();
    assert_eq!(all.len(), ALL.len());
    assert_eq!(all.len(), 10);
}

/// A slot of a book whose clean runs the assertion against a world a test controls.
struct Slot(u32);

impl Slotted for Slot {
    fn uid(&self) -> u32 {
        61000 + self.0
    }
}

/// A book over two slots whose cleaner asserts `world`: red is dirty, as the Mac's
/// cleaner makes it.
fn asserted_book(world: &Arc<Fake>, base: &Arc<Snapshot>) -> Arc<Book<Slot>> {
    let (world, base) = (Arc::clone(world), Arc::clone(base));
    let cleaner: Cleaner<Slot> = Arc::new(move |_: &Slot| {
        let red = assert(world.as_ref(), Some(&base), &ALL, (&|| false, None));
        if red.is_empty() {
            Outcome::Clean
        } else {
            Outcome::Dirty {
                why: format!("assertion: {}", red.join("; ")),
            }
        }
    });
    let never = Backoff {
        first: Duration::from_secs(3600),
        max: Duration::from_secs(3600),
    };
    Book::start(vec![Slot(0), Slot(1)], cleaner, never)
}

/// Catches a red assertion that still offers the slot (a lease would run on a user that
/// holds what the last one left), a count that does not drop, a recovery that frees the
/// slot while the user is still red, and one that never asserts again: with the user red
/// both slots are dirty and the count is 0; a re-clean that finds it still red frees
/// nothing; once it is green again the next re-clean frees both.
#[test]
fn a_red_assertion_keeps_the_slot_out_until_a_clean_and_assert_passes() {
    let base = Arc::new(baseline());
    let world = Arc::new(Fake::clean());
    world.set(Probe::Processes, &["61001 /bin/sleep"]);
    let book = asserted_book(&world, &base);
    assert_eq!(book.count(), 0);
    assert_eq!(book.dirty().len(), 2);
    assert_eq!(book.retry(), 0, "still red: nothing is freed");
    assert_eq!(book.count(), 0);
    world.set(Probe::Processes, &[]);
    assert_eq!(book.retry(), 2);
    assert_eq!(book.count(), 2);
}

/// Catches a slot-ready assertion that does not set the slot aside through the book
/// (the lease would take it anyway), and one that sets aside the wrong slot or all of
/// them: the taken slot is the one dirty, the other stays offered, and a later clean
/// that asserts green frees it.
#[tokio::test]
async fn a_slot_that_fails_the_assertion_at_slot_ready_is_set_aside() {
    let base = Arc::new(baseline());
    let world = Arc::new(Fake::clean());
    let book = asserted_book(&world, &base);
    assert_eq!(book.count(), 2);
    let (index, permit) = book.take().await.expect("a slot");
    // What a lease finds at slot-ready: a mount that was not there at the clean.
    world.set(Probe::Mounts, &["mounted /private/var/folders/x/T/vol"]);
    let red = assert(world.as_ref(), Some(&base), &CHEAP, (&|| false, None));
    assert_eq!(
        red,
        lines(&["mounts: + mounted /private/var/folders/x/T/vol"])
    );
    book.set_aside(index, permit, &format!("assertion: {}", red.join("; ")));
    assert_eq!(book.count(), 1);
    assert_eq!(book.dirty().into_iter().collect::<Vec<_>>(), vec![index]);
    assert_eq!(
        book.retry(),
        0,
        "the re-clean asserts again, and it is still red"
    );
    world.set(Probe::Mounts, &[]);
    assert_eq!(book.retry(), 1);
    assert_eq!(book.count(), 2);
}

const SERVICE: &str = "com.apple.cfprefsd.xpc.agent";
const PLIST: &str = "/System/Library/LaunchAgents/com.apple.cfprefsd.xpc.agent.plist";
const PROGRAM: &str = "/usr/sbin/cfprefsd";

/// Catches a job of a baseline label taken as launchd's own with no look at its plist
/// (a lease that boots out a service and submits its own job under the label): the
/// plist path and the program must be the baseline's, and the file check must pass for
/// the path; a label the baseline does not have is judged by the file alone; a job with
/// no plist is never launchd's.
#[test]
fn a_baseline_label_is_launchds_own_only_with_its_plist_and_program() {
    let known = vec![service_line(SERVICE, Some(PLIST), Some(PROGRAM))];
    let accepts = |_: &str| true;
    let refuses = |_: &str| false;
    let judge = |plist, program, on_file: &dyn Fn(&str) -> bool| {
        is_own_job((SERVICE, plist, program), &known, on_file)
    };
    // The same plist and program, and the file is launchd's: own.
    assert!(judge(Some(PLIST), Some(PROGRAM), &accepts));
    // The same path, but the file is not launchd's (replaced, or written by the lease).
    assert!(!judge(Some(PLIST), Some(PROGRAM), &refuses));
    // A different plist path, even a system-looking one that passes the file check.
    assert!(!judge(
        Some("/System/Library/LaunchAgents/other.plist"),
        Some(PROGRAM),
        &accepts
    ));
    // The same plist but a different program.
    assert!(!judge(Some(PLIST), Some("/tmp/x/cfprefsd"), &accepts));
    assert!(!judge(Some(PLIST), None, &accepts));
    // A submitted job has no plist.
    assert!(!judge(None, Some(PROGRAM), &accepts));
    // A label the baseline did not note: the file alone decides.
    let other = |plist, on_file: &dyn Fn(&str) -> bool| {
        is_own_job(("com.apple.new", plist, None), &known, on_file)
    };
    assert!(other(Some("/System/new.plist"), &accepts));
    assert!(!other(Some("/System/new.plist"), &refuses));
    assert!(!other(None, &accepts));
    // A label that is the start of a noted one is not that service.
    let longer = format!("{SERVICE}.more");
    assert!(!is_own_job(
        (&longer, Some(PLIST), Some(PROGRAM)),
        &known,
        &refuses
    ));
}

/// Catches a service line that does not tell a missing program from a path: the line
/// has the label, plist and program, a missing one empty.
#[test]
fn a_service_line_holds_label_plist_and_program() {
    assert_eq!(service_line("a", Some("/p"), Some("/q")), "a\t/p\t/q");
    assert_eq!(service_line("a", None, None), "a\t\t");
}

/// Catches an extended attribute whose value changed under the same name going unseen:
/// the line holds the value, in hex.
#[test]
fn an_extended_attribute_line_holds_its_value() {
    assert_eq!(
        xattr_line("com.apple.x", b"\x00\xffab"),
        "com.apple.x=00ff6162"
    );
    assert_ne!(xattr_line("n", b"1"), xattr_line("n", b"2"));
    let base = baseline_with_home(&["xattr n=31"]);
    let now = Fake::clean();
    now.set(Probe::Home, &["xattr n=32"]);
    assert_eq!(
        assert(&now, Some(&base), &[Probe::Home], (&|| false, None)),
        lines(&["home: + xattr n=32", "home: - xattr n=31"])
    );
}

fn baseline_with_home(home: &[&str]) -> Snapshot {
    let fake = Fake::clean();
    fake.set(Probe::Home, home);
    record(&fake).expect("a baseline")
}

/// A reader that takes `pause` for each probe it reads.
struct Slow(Fake, Duration);

impl Reader for Slow {
    fn read(&self, probe: Probe) -> Result<Lines, String> {
        std::thread::sleep(self.1);
        self.0.read(probe)
    }
}

/// Catches a check with no total budget: a probe not begun when the budget is used is not
/// read, and the result is red and says why; with room left every probe is read; a probe
/// that began is read in full.
#[test]
fn a_check_that_runs_out_of_time_is_red() {
    let base = baseline();
    let slow = Slow(Fake::clean(), Duration::from_millis(60));
    let now = std::time::Instant::now();
    let red = assert(
        &slow,
        Some(&base),
        &CHEAP,
        (&|| false, Some(now + Duration::from_millis(30))),
    );
    // The first probe began in time and was read in full; the second was not begun.
    assert_eq!(
        red,
        lines(&["launchd: not read, the check ran out of time"])
    );
    let room = Some(std::time::Instant::now() + Duration::from_secs(60));
    assert!(assert(&slow, Some(&base), &CHEAP, (&|| false, room)).is_empty());
}
