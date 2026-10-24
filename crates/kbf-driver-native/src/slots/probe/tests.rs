//! The clean's decisions over text, with no Mac.

use super::*;

/// Catches a job told to be launchd's own by its label (a lease chooses its labels) and
/// not by where its plist is: only a path under a sealed system folder or Apple's own
/// is launchd's. A plist in a home, or one that merely contains `/System/` later in the
/// path, or a missing path, is the lease's.
#[test]
fn a_job_is_launchds_own_by_the_folder_of_its_plist() {
    let printed = "com.apple.cfprefsd.xpc.agent = {\n\tactive count = 1\n\tpath = /System/Library/LaunchAgents/com.apple.cfprefsd.xpc.agent.plist\n\tstate = running\n\n\tinherited environment = {\n\t\tpath = /ignored\n\t}\n}\n";
    assert_eq!(
        plist_path(printed),
        Some("/System/Library/LaunchAgents/com.apple.cfprefsd.xpc.agent.plist")
    );
    assert_eq!(plist_path("x = {\n\t\tpath = /deeper\n}\n"), None);
    for own in [
        "/System/Library/LaunchAgents/a.plist",
        "/Library/Apple/System/Library/LaunchAgents/b.plist",
    ] {
        assert!(is_own_plist(own), "{own}");
    }
    for lease in [
        "/Volumes/pool/kbf-pool-0/Library/LaunchAgents/com.apple.fake.plist",
        "/Library/LaunchAgents/System.plist",
        "/tmp/System/x.plist",
        "/Library/Apple",
        "",
    ] {
        assert!(!is_own_plist(lease), "{lease}");
    }
}

/// Catches a services table read wrong: a running service's pid, a stopped one's `-`
/// and 0, a label with a dot, the lines outside the table, and a domain with no table.
#[test]
fn the_services_of_a_domain_are_the_table_between_its_braces() {
    let domain = "user/700 = {\n\ttype = user\n\tservices = {\n\t\t   123      -   com.apple.a\n\t\t     0      -   kbf.b\n\t\t     -     78   kbf.c\n\t}\n\n\tunrelated = {\n\t\t9 - nope\n\t}\n}\n";
    assert_eq!(
        listed_services(domain),
        [
            (Some(123), "com.apple.a".to_owned()),
            (None, "kbf.b".to_owned()),
            (None, "kbf.c".to_owned()),
        ]
    );
    // A label with a space in it is whole, not its last word.
    let spaced = "user/700 = {\n\tservices = {\n\t\t   55      -   a b\n\t\t   -     -   \n\t\t   7\n\t}\n}\n";
    assert_eq!(listed_services(spaced), [(Some(55), "a b".to_owned())]);
    assert_eq!(listed_services("user/700 = {\n}\n"), []);
    assert_eq!(listed_services(""), []);
}

/// Catches a clean that lists launchd's services (one print of the domain and one per
/// service, 40 or so) when no process of the uid is alive: with live pids the list is
/// read and the services judged (counted), with none nothing is listed or judged.
#[test]
fn no_service_is_listed_when_no_process_is_alive() {
    let (listed, judged) = (std::cell::Cell::new(0), std::cell::Cell::new(0));
    let own = |pids: &[i32]| {
        own_among(
            pids,
            || {
                listed.set(listed.get() + 1);
                vec![(Some(7), "own.a".to_owned())]
            },
            |_, _| {
                judged.set(judged.get() + 1);
                true
            },
        )
    };
    assert_eq!(own(&[7]), [7]);
    assert_eq!((listed.get(), judged.get()), (1, 1));
    assert_eq!(own(&[]), Vec::<i32>::new());
    assert_eq!((listed.get(), judged.get()), (1, 1));
}

/// Catches a clean that prints every service for its plist: only a service that runs one
/// of the live pids is judged (not one with no process, nor one of another pid), and the
/// result is the live pids whose service is judged launchd's own.
#[test]
fn only_the_services_running_a_live_pid_are_judged() {
    let services = vec![
        (Some(10), "own.a".to_owned()),
        (Some(11), "lease.b".to_owned()),
        (None, "own.idle".to_owned()),
        (Some(99), "own.other".to_owned()),
        (Some(12), "own.c".to_owned()),
    ];
    let mut asked = Vec::new();
    let own = own_among(
        &[10, 11, 12, 13],
        || services,
        |_, label| {
            asked.push(label.to_owned());
            label.starts_with("own.")
        },
    );
    assert_eq!(own, [10, 12]);
    assert_eq!(asked, ["own.a", "lease.b", "own.c"]);
}

/// Catches a clean that prints a service again for every scan of its kill loop and every
/// look after it: a service (label and pid) is judged once, whatever the answer, and a
/// new one is judged on its own; a new `OwnLabels` (the next clean) judges again. A print
/// that told nothing counts as not own and is asked again, not kept (a timeout must not
/// make a launchd agent a stray for the rest of the clean).
#[test]
fn a_clean_judges_each_service_once() {
    let judged = OwnLabels::default();
    let calls = std::cell::Cell::new(0);
    let calls = &calls;
    let judge = |answer: Option<bool>| {
        move || {
            calls.set(calls.get() + 1);
            answer
        }
    };
    for _ in 0..3 {
        assert!(judged.is_own("own.a", Some(5), judge(Some(true))));
        assert!(!judged.is_own("lease.b", None, judge(Some(false))));
    }
    assert_eq!(calls.get(), 2);
    assert!(judged.is_own("own.c", Some(6), judge(Some(true))));
    assert_eq!(calls.get(), 3);
    assert!(!OwnLabels::default().is_own("own.a", Some(5), judge(Some(false))));
    assert_eq!(calls.get(), 4);
    for asked in 5..=7 {
        assert!(!judged.is_own("slow.d", Some(8), judge(None)));
        assert_eq!(calls.get(), asked);
    }
    assert!(judged.is_own("slow.d", Some(8), judge(Some(true))));
    assert!(judged.is_own("slow.d", Some(8), judge(Some(false))));
    assert_eq!(calls.get(), 8);
}

/// Catches a memo that keeps a label as launchd's own for the whole clean: the same label
/// running another process (the job was registered again, from another plist) is
/// judged again, and its answer is its own.
#[test]
fn the_same_label_under_another_process_is_judged_again() {
    let judged = OwnLabels::default();
    assert!(judged.is_own("com.apple.x", Some(10), || Some(true)));
    assert!(!judged.is_own("com.apple.x", Some(11), || Some(false)));
    assert!(judged.is_own("com.apple.x", Some(10), || Some(false)));
    assert!(!judged.is_own("com.apple.x", Some(11), || Some(true)));
}

/// Catches a memo that stops answering once a thread panicked while holding it: the
/// answers already kept are still given and new ones are kept.
#[test]
fn a_poisoned_memo_still_answers() {
    let judged = OwnLabels::default();
    assert!(judged.is_own("own.a", Some(1), || Some(true)));
    std::thread::scope(|scope| {
        let gone = scope.spawn(|| {
            let _held = judged.known.lock().expect("not yet poisoned");
            panic!("a thread died holding the memo");
        });
        assert!(gone.join().is_err());
    });
    assert!(judged.known.is_poisoned());
    assert!(judged.is_own("own.a", Some(1), || Some(false)));
    assert!(judged.is_own("own.b", Some(2), || Some(true)));
}

/// Catches an own list that is empty when the process table could not be read (launchd's
/// own agents would then be sent SIGTERM at the end of a lease): a table read gives the
/// pids found among its pids and lists nothing else; a failed read falls back to all
/// the own services.
#[test]
fn a_failed_process_table_read_lists_all_the_own_services() {
    let (among, all) = (std::cell::Cell::new(0), std::cell::Cell::new(0));
    let own = |table: Result<Vec<i32>, ()>| {
        own_of_uid(
            table,
            |pids| {
                among.set(among.get() + 1);
                pids.to_vec()
            },
            || {
                all.set(all.get() + 1);
                vec![3, 4]
            },
        )
    };
    assert_eq!(own(Ok(vec![1, 2])), [1, 2]);
    assert_eq!((among.get(), all.get()), (1, 0));
    assert_eq!(own(Err(())), [3, 4]);
    assert_eq!((among.get(), all.get()), (1, 1));
}

/// Catches a shared sweep without `/Library/Caches` (a raw plant there by the uid survived
/// the clean), one that still sweeps all of `/private/var/folders` (the other uids'
/// entries are listed and searched), and one that lost a place or launchd's per-uid
/// files: the plan names exactly the five places and the two named files.
#[test]
fn the_shared_plan_covers_the_shared_places_and_caches_not_the_folders() {
    let plan = shared_plan();
    assert_eq!(
        plan.owned,
        [
            Path::new("/Users").join("Shared"),
            PathBuf::from("/private/tmp"),
            PathBuf::from("/private/var/tmp"),
            PathBuf::from("/Library/Caches"),
        ]
    );
    assert_eq!(
        plan.named,
        [
            "/private/var/db/com.apple.xpc.launchd/disabled.{uid}.plist",
            "/private/var/db/com.apple.xpc.launchd/loginitems.{uid}.plist"
        ]
    );
}

/// Catches a mount list read wrong: a device with spaces in its name, options after the
/// path, a mount that is only a prefix of the root (`kbf-pool-10` is not under
/// `kbf-pool-1`), and the order: a mount inside a mount first.
#[test]
fn mounts_under_the_roots_come_off_deepest_first() {
    let mount = "\
/dev/disk3s1s1 on / (apfs, sealed, local, read-only, journaled)
/dev/disk4s1 on /Volumes/kbf/home/kbf-pool-1/mnt (hfs, local, nodev, nosuid, mounted by u)
/dev/disk5s1 on /Volumes/kbf/home/kbf-pool-1/mnt/inner (hfs, local, nodev)
map auto home on /Volumes/kbf/home/kbf-pool-10/mnt (autofs)
/dev/disk6s1 on /private/var/folders/xx/yyyy/T/m (apfs)
garbage line
";
    let home = Path::new("/Volumes/kbf/home/kbf-pool-1");
    let entry = Path::new("/private/var/folders/xx/yyyy");
    assert_eq!(
        mounts_under(mount, &[home, entry]),
        [
            PathBuf::from("/private/var/folders/xx/yyyy/T/m"),
            PathBuf::from("/Volumes/kbf/home/kbf-pool-1/mnt/inner"),
            PathBuf::from("/Volumes/kbf/home/kbf-pool-1/mnt"),
        ]
    );
    assert_eq!(
        mounts_under(mount, &[Path::new("/nowhere")]),
        Vec::<PathBuf>::new()
    );
    assert_eq!(mounts_under("", &[home]), Vec::<PathBuf>::new());
}

/// Catches a restore that touches a folder within the system's layout (0 and X at 755 or
/// narrower, C and T at 700 or narrower), one that widens a folder, and one that leaves a
/// group or other bit on C or T, or a group or other write on 0 or X.
#[test]
fn a_folder_is_narrowed_only_when_it_is_wider_than_the_systems_layout() {
    assert_eq!((folder_ceiling("0"), folder_ceiling("X")), (0o755, 0o755));
    assert_eq!((folder_ceiling("C"), folder_ceiling("T")), (0o700, 0o700));
    // Within: no change, and the file-type bits of a stat mode are not part of the mode.
    for (name, mode) in [
        ("0", 0o755),
        ("0", 0o40755),
        ("X", 0o700),
        ("X", 0o750),
        ("C", 0o700),
        ("T", 0o500),
        ("0", 0o655),
        ("T", 0o40700),
    ] {
        let want = folder_fix((501, mode), 501, folder_ceiling(name));
        assert_eq!(want, (false, None), "{name} {mode:o}");
    }
    // Wider: narrowed to the bits the ceiling allows, never widened.
    for (name, mode, to) in [
        ("0", 0o775, 0o755),
        ("0", 0o757, 0o755),
        ("X", 0o777, 0o755),
        ("C", 0o755, 0o700),
        ("T", 0o710, 0o700),
        ("T", 0o770, 0o700),
        ("C", 0o4700, 0o700),
    ] {
        let got = folder_fix((501, mode), 501, folder_ceiling(name));
        assert_eq!(got, (false, Some(to)), "{name} {mode:o}");
    }
    // A wrong owner is reported whatever the mode.
    assert_eq!(folder_fix((0, 0o700), 501, 0o700), (true, None));
    assert_eq!(folder_fix((0, 0o777), 501, 0o700), (true, Some(0o700)));
}

/// A scripted uid: `scans` are what the successive scans find (then nothing).
fn run_loop(
    scans: &[&[i32]],
    expire_after: usize,
    kill: Result<(), String>,
) -> (Result<(), String>, usize, usize) {
    let (mut kills, mut pauses, mut seen) = (0, 0, 0);
    let mut script = scans.iter();
    let result = kill_until_quiet(
        &mut || {
            seen += 1;
            seen > expire_after
        },
        &mut || {
            kills += 1;
            kill.clone()
        },
        &mut || Ok(script.next().map_or_else(Vec::new, |s| s.to_vec())),
        &mut || pauses += 1,
    );
    (result, kills, pauses)
}

/// Catches a kill loop that stops at the first empty scan (a respawn after the kill
/// would be missed: two empty scans, a pause apart, are the rule), one that does not
/// restart its count when a process shows up between two empty scans: empty, then a
/// process, then empty twice, is five rounds with the quiet pair at the end.
#[test]
fn the_kill_loop_ends_on_two_empty_scans_in_a_row() {
    assert_eq!(run_loop(&[], 100, Ok(())), (Ok(()), 2, 1));
    assert_eq!(
        run_loop(&[&[5], &[], &[6], &[], &[]], 100, Ok(())),
        (Ok(()), 5, 4)
    );
    assert_eq!(run_loop(&[&[5]], 100, Ok(())), (Ok(()), 3, 2));
}

/// Catches a kill loop with no bound (a process stuck in the kernel would hang the slot),
/// one whose bound is not the scan count, one that forgets the deadline, and an error
/// that hides the pids: processes that never go fail the loop at the 100th scan, or at
/// the deadline, naming them.
#[test]
fn the_kill_loop_is_bounded_by_scans_and_by_the_deadline() {
    let stuck: Vec<&[i32]> = vec![&[7]; 200];
    let (result, kills, pauses) = run_loop(&stuck, usize::MAX, Ok(()));
    assert_eq!(result, Err("processes [7] survived SIGKILL".to_owned()));
    assert_eq!((kills, pauses), (MAX_SCANS, MAX_SCANS));
    let (result, kills, pauses) = run_loop(&stuck, 3, Ok(()));
    assert_eq!(result, Err("processes [7] survived SIGKILL".to_owned()));
    assert_eq!((kills, pauses), (4, 3));
    // Out of time while quiet once, and not quiet at the one more scan: not quiet twice.
    let (result, _, _) = run_loop(&[&[], &[8]], 0, Ok(()));
    assert_eq!(result, Err("processes [8] survived SIGKILL".to_owned()));
    // A uid that is quiet only every other scan never gets its pair: at the scan bound
    // the last scan was empty, once.
    let flapping: Vec<&[i32]> = (0..MAX_SCANS)
        .map(|n| if n % 2 == 0 { &[7][..] } else { &[][..] })
        .collect();
    let (result, kills, _) = run_loop(&flapping, usize::MAX, Ok(()));
    assert_eq!(
        result,
        Err("the uid was not quiet for two scans".to_owned())
    );
    assert_eq!(kills, MAX_SCANS);
}

/// Catches a kill loop that fails a uid which went quiet just as the deadline passed:
/// the first scan finds a process, the second (after the deadline) finds none, and the
/// one more scan the deadline allows finds none again, so the uid is quiet. The extra
/// scan is the only one: a process found at it fails the loop, with no third.
#[test]
fn a_deadline_that_passes_after_one_quiet_scan_allows_one_more_scan() {
    assert_eq!(run_loop(&[&[5]], 1, Ok(())), (Ok(()), 3, 2));
    let (result, kills, _) = run_loop(&[&[5], &[], &[9], &[9]], 1, Ok(()));
    assert_eq!(result, Err("processes [9] survived SIGKILL".to_owned()));
    assert_eq!(kills, 3);
}

/// Catches a kill that fails and is ignored (the scans would then judge a live uid) and a
/// scan that fails and is read as empty: both end the loop with their error.
#[test]
fn a_failed_kill_or_scan_ends_the_loop_with_its_error() {
    let (result, kills, _) = run_loop(&[], 100, Err("setuid".to_owned()));
    assert_eq!((result, kills), (Err("setuid".to_owned()), 1));
    let result = kill_until_quiet(
        &mut || false,
        &mut || Ok(()),
        &mut || Err("table".to_owned()),
        &mut || {},
    );
    assert_eq!(result, Err("table".to_owned()));
}

/// Catches a program read from a nested dictionary of the printed job, or from a line
/// that merely contains the word: only `program = ` one level in is the job's program.
#[test]
fn the_program_of_a_job_is_the_line_one_level_in() {
    let printed = "x = {\n\tpath = /System/a.plist\n\tprogram = /usr/sbin/cfprefsd\n\n\tinherited environment = {\n\t\tprogram = /ignored\n\t}\n}\n";
    assert_eq!(program_path(printed), Some("/usr/sbin/cfprefsd"));
    assert_eq!(program_path("x = {\n\t\tprogram = /deeper\n}\n"), None);
    assert_eq!(program_path("x = {\n\tpath = /p\n}\n"), None);
}

#[test]
fn the_restore_changes_only_what_differs() {
    let want = (501, 20, 0o700);
    assert_eq!(ownership_to_change(want, want), (false, false));
    // The file-type bits of a stat mode are not part of the mode.
    assert_eq!(
        ownership_to_change((501, 20, 0o40700), want),
        (false, false)
    );
    assert_eq!(ownership_to_change((0, 20, 0o700), want), (true, false));
    assert_eq!(ownership_to_change((501, 0, 0o700), want), (true, false));
    assert_eq!(ownership_to_change((501, 20, 0o755), want), (false, true));
    assert_eq!(ownership_to_change((0, 0, 0o755), want), (true, true));
}
