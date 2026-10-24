//! The `/var/folders` entry's clean as the pool user: who each call runs as and in what
//! order (a fake [`Run`]), and the entry mode on a real directory (the test's own uid
//! stands for the pool user).

use std::cell::RefCell;
use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;

use kbf_testkit::dir::TestDir;

use super::*;

const PROGRAM: &str = "/opt/kbf/kbf-daemon";
const HOME: &str = "/pool/home-3";
const FOLDERS: &str = "/private/var/folders/xx/yyyy/0";

fn me() -> AsUid<'static> {
    AsUid {
        program: Path::new(PROGRAM),
        uid: 61003,
        gid: 20,
    }
}

/// Everything a step did, in order: `root <path>` for a part done as root, and
/// `<program> <args>` for each call of the real runner.
type Log = RefCell<Vec<String>>;

fn words(args: &[OsString]) -> String {
    let args: Vec<String> = args
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    args.join(" ")
}

/// A runner that logs each call and fails for a call that has `fail` among its arguments.
fn runner<'a>(
    log: &'a Log,
    fail: &'a [&'a str],
) -> impl FnMut(&Path, &[OsString]) -> Result<String, String> + 'a {
    move |program, args| {
        let line = format!("{} {}", program.display(), words(args));
        log.borrow_mut().push(line.clone());
        if let Some(word) = fail.iter().find(|w| line.contains(**w)) {
            return Err(format!("{word} failed"));
        }
        Ok("lines\nof\noutput".to_owned())
    }
}

fn as_root<'a>(log: &'a Log) -> impl FnMut(&Path) -> Vec<String> + 'a {
    move |path| {
        log.borrow_mut().push(format!("root {}", path.display()));
        Vec::new()
    }
}

/// The call of the entry mode `mode` on the home.
fn home_call(mode: &str) -> String {
    format!("{PROGRAM} __entry {mode} 61003 20 {HOME}")
}

/// The call of the entry mode `mode`: it drops to the uid itself.
fn mode_call(mode: &str) -> String {
    format!("{PROGRAM} __entry {mode} 61003 20 {FOLDERS}")
}

#[test]
fn the_flags_step_clears_the_home_as_root_and_the_entry_as_the_uid_in_that_order() {
    let log = Log::default();
    flags_step(
        &me(),
        (Path::new(HOME), Path::new(FOLDERS)),
        &mut as_root(&log),
        &mut runner(&log, &[]),
    )
    .expect("clean");
    assert_eq!(
        *log.borrow(),
        vec![
            format!("root {HOME}"),
            mode_call("acl"),
            mode_call("clear"),
            mode_call("acl"),
        ]
    );
}

#[test]
fn root_is_given_the_home_only_to_clear_never_the_entry_and_never_to_empty() {
    let log = Log::default();
    let mut roots = Vec::new();
    let mut record = |path: &Path| {
        roots.push(path.to_owned());
        Vec::new()
    };
    let run = &mut runner(&log, &[]);
    flags_step(
        &me(),
        (Path::new(HOME), Path::new(FOLDERS)),
        &mut record,
        run,
    )
    .expect("flags");
    contents_step(&me(), (Path::new(HOME), Path::new(FOLDERS)), run).expect("contents");
    assert_eq!(roots, vec![PathBuf::from(HOME)]);
}

#[test]
fn every_call_on_the_entry_is_made_as_the_uid() {
    let log = Log::default();
    let run = &mut runner(&log, &[]);
    let dir = (Path::new(HOME), Path::new(FOLDERS));
    flags_step(&me(), dir, &mut as_root(&Log::default()), run).expect("flags");
    contents_step(&me(), dir, run).expect("contents");
    left(&me(), Path::new(FOLDERS), run).expect("left");
    left_home(&me(), Path::new(HOME), run).expect("left home");
    let calls = log.borrow();
    assert_eq!(calls.len(), 8);
    // The daemon's modes get the ids and drop inside their own process (the pool user
    // need not be able to execute the binary).
    for call in calls.iter() {
        let own = call.starts_with(&format!("{PROGRAM} __entry "))
            && (call.contains(&format!(" 61003 20 {FOLDERS}"))
                || call.contains(&format!(" 61003 20 {HOME}")));
        assert!(own, "{call}");
    }
}

#[test]
fn the_acls_are_removed_before_and_after_the_sweep_and_only_the_last_removal_is_judged() {
    let log = Log::default();
    let first = format!("{PROGRAM} __entry acl ");
    // Every call fails but the sweep, which is the second: the first removal is let fail.
    let calls = RefCell::new(0);
    let mut run = |program: &Path, args: &[OsString]| {
        let line = format!("{} {}", program.display(), words(args));
        log.borrow_mut().push(line.clone());
        *calls.borrow_mut() += 1;
        if line.starts_with(&first) && *calls.borrow() == 1 {
            return Err("early removal failed".to_owned());
        }
        Ok(String::new())
    };
    clear_flags(&me(), Path::new(FOLDERS), &mut run).expect("only the last removal is judged");
    assert_eq!(
        *log.borrow(),
        vec![mode_call("acl"), mode_call("clear"), mode_call("acl")]
    );
}

#[test]
fn the_contents_step_empties_the_home_then_the_entry_both_as_the_uid() {
    let log = Log::default();
    contents_step(
        &me(),
        (Path::new(HOME), Path::new(FOLDERS)),
        &mut runner(&log, &[]),
    )
    .expect("clean");
    assert_eq!(
        *log.borrow(),
        vec![
            home_call("keychain"),
            home_call("empty-home"),
            mode_call("empty")
        ]
    );
}

#[test]
fn a_uid_side_failure_of_the_flags_step_fails_it_naming_the_entry() {
    for fail in ["__entry clear", "__entry acl"] {
        let log = Log::default();
        let why = flags_step(
            &me(),
            (Path::new(HOME), Path::new(FOLDERS)),
            &mut as_root(&log),
            &mut runner(&log, &[fail]),
        )
        .expect_err("fails");
        assert!(why.starts_with(&format!("{FOLDERS}: ")), "{why}");
        assert!(why.contains(&format!("{fail} failed")), "{why}");
        // The other call is made anyway: both failures are shown.
        assert_eq!(log.borrow().len(), 4, "{fail}");
    }
}

#[test]
fn both_uid_side_failures_of_the_flags_step_are_reported() {
    let log = Log::default();
    let why = flags_step(
        &me(),
        (Path::new(HOME), Path::new(FOLDERS)),
        &mut as_root(&log),
        &mut runner(&log, &["__entry clear", "__entry acl"]),
    )
    .expect_err("fails");
    assert!(
        why.contains("__entry clear failed; __entry acl failed"),
        "{why}"
    );
}

#[test]
fn a_failure_of_the_home_as_root_fails_the_step_and_the_entry_is_still_cleaned() {
    let log = Log::default();
    let why = flags_step(
        &me(),
        (Path::new(HOME), Path::new(FOLDERS)),
        &mut |_| vec!["home: locked".to_owned()],
        &mut runner(&log, &[]),
    )
    .expect_err("fails");
    assert_eq!(why, "home: locked");
    assert_eq!(log.borrow().len(), 3);
}

#[test]
fn a_uid_side_failure_of_the_contents_step_fails_it_naming_the_path() {
    let log = Log::default();
    let why = contents_step(
        &me(),
        (Path::new(HOME), Path::new(FOLDERS)),
        &mut runner(&log, &["__entry empty "]),
    )
    .expect_err("fails");
    assert_eq!(why, format!("{FOLDERS}: __entry empty  failed"));
}

#[test]
fn a_failure_of_the_home_and_of_the_entry_are_both_in_the_contents_failure() {
    let log = Log::default();
    let why = contents_step(
        &me(),
        (Path::new(HOME), Path::new(FOLDERS)),
        &mut runner(&log, &["__entry empty"]),
    )
    .expect_err("fails");
    assert!(why.starts_with(&format!("{HOME}: ")), "{why}");
    assert!(why.contains(&format!("{FOLDERS}: ")), "{why}");
}

#[test]
fn the_look_of_the_home_is_one_call_as_the_uid_and_names_the_home_when_it_fails() {
    let log = Log::default();
    let lines = left_home(&me(), Path::new(HOME), &mut runner(&log, &[])).expect("looked");
    assert_eq!(lines, vec!["lines", "of", "output"]);
    assert_eq!(*log.borrow(), vec![home_call("look-home")]);
    let why = left_home(
        &me(),
        Path::new(HOME),
        &mut runner(&Log::default(), &["look-home"]),
    )
    .expect_err("fails");
    assert_eq!(why, format!("{HOME}: look-home failed"));
}

#[test]
fn the_look_reports_each_line_the_uid_printed() {
    let log = Log::default();
    let lines = left(&me(), Path::new(FOLDERS), &mut runner(&log, &[])).expect("looked");
    assert_eq!(lines, vec!["lines", "of", "output"]);
    assert_eq!(*log.borrow(), vec![mode_call("look")]);
}

#[test]
fn a_look_that_could_not_be_made_is_an_error_naming_the_entry() {
    let log = Log::default();
    let why = left(
        &me(),
        Path::new(FOLDERS),
        &mut runner(&log, &["__entry look"]),
    )
    .expect_err("fails");
    assert_eq!(why, format!("{FOLDERS}: __entry look failed"));
}

#[test]
fn acls_are_removed_only_from_the_uids_entries_and_never_through_a_link() {
    let args = words(&acl_args(Path::new(FOLDERS), 61003));
    assert_eq!(
        args,
        format!(
            "-x {FOLDERS} ! ( -path {FOLDERS} -o -path {FOLDERS}/0 -o -path {FOLDERS}/C -o -path {FOLDERS}/T -o -path {FOLDERS}/X ) -user 61003 ( ( -type d -exec /bin/chmod -N {{}} ; ) -o ( ! -type l -exec /bin/chmod -N {{}} + ) )"
        )
    );
}

// The entry mode, on a real directory.

fn tree(name: &str) -> TestDir {
    TestDir::under(
        std::env::temp_dir(),
        &format!("kbf-driver-native-entry-{name}"),
    )
}

fn me_now() -> u32 {
    rustix::process::geteuid().as_raw().max(1)
}

/// The entry mode with `words` after argv0, dropping through `drop_to`; its exit code
/// (`None` when the arguments do not select it), stdout and stderr.
fn run_mode(
    words: &[&str],
    drop_to: &mut dyn FnMut(u32, u32) -> Result<(), String>,
) -> (Option<i32>, String, String) {
    let mut args: Vec<OsString> = vec!["kbf-daemon".into()];
    args.extend(words.iter().map(OsString::from));
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = dispatch(&args, drop_to, &mut out, &mut err);
    (
        code,
        String::from_utf8(out).expect("utf8"),
        String::from_utf8(err).expect("utf8"),
    )
}

/// `mode` on `dir` as the test's own uid and gid, which the fake drop accepts.
fn call(mode: &str, dir: &Path) -> (Option<i32>, String, String) {
    let (uid, gid) = (me_now().to_string(), my_gid().to_string());
    run_mode(
        &[ARG, mode, &uid, &gid, &dir.to_string_lossy()],
        &mut |_, _| Ok(()),
    )
}

fn my_gid() -> u32 {
    rustix::process::getegid().as_raw().max(1)
}

fn entry_with_files(dir: &TestDir) -> PathBuf {
    let entry = dir.path().join("entry");
    for folder in ["0", "C", "T", "X"] {
        fs::create_dir_all(entry.join(folder).join("deep")).expect("folder");
        fs::write(entry.join(folder).join("deep").join("file"), "x").expect("file");
        fs::write(entry.join(folder).join("top"), "x").expect("file");
    }
    fs::write(entry.join("stray"), "x").expect("stray");
    entry
}

#[test]
fn other_arguments_are_not_the_entry_mode() {
    let no_drop = &mut |_, _| panic!("dropped");
    assert_eq!(run_mode(&[], no_drop).0, None);
    assert_eq!(run_mode(&["--help"], no_drop).0, None);
    assert_eq!(run_mode(&["__as-user"], no_drop).0, None);
}

#[test]
fn the_entry_mode_refuses_bad_arguments_before_it_drops_anything() {
    let no_drop = &mut |_, _| panic!("dropped");
    for bad in [
        &[ARG][..],
        &[ARG, EMPTY, "5", "5"],
        &[ARG, EMPTY, "5", "5", "/x", "/y"],
    ] {
        let (code, _, err) = run_mode(bad, no_drop);
        assert_eq!(
            (code, err.starts_with("usage: ")),
            (Some(2), true),
            "{bad:?}"
        );
    }
    for ids in [
        ("0", "20"),
        ("501", "0"),
        ("x", "20"),
        ("501", "-1"),
        ("", ""),
    ] {
        let (code, _, err) = run_mode(&[ARG, EMPTY, ids.0, ids.1, "/x"], no_drop);
        assert_eq!(
            (code, err.contains("non-root ids")),
            (Some(2), true),
            "{ids:?}"
        );
    }
    // An id that is not text is refused like one that is not a number.
    use std::os::unix::ffi::OsStringExt as _;
    let args: Vec<OsString> = vec![
        "kbf-daemon".into(),
        ARG.into(),
        EMPTY.into(),
        OsString::from_vec(vec![0xff, 0xfe]),
        "20".into(),
        "/x".into(),
    ];
    let (mut out, mut err) = (Vec::new(), Vec::new());
    assert_eq!(dispatch(&args, no_drop, &mut out, &mut err), Some(2));
    assert!(String::from_utf8_lossy(&err).contains("non-root ids"));
    let (code, _, err) = run_mode(&[ARG, "bogus", "501", "20", "/x"], no_drop);
    assert_eq!((code, err.contains("no mode")), (Some(2), true));
}

#[test]
fn the_entry_mode_drops_to_the_given_ids_and_stops_when_the_drop_fails() {
    let dir = tree("drop");
    let entry = entry_with_files(&dir);
    let mut dropped = Vec::new();
    let (uid, gid) = (me_now().to_string(), my_gid().to_string());
    let words = [ARG, LOOK, &uid, &gid, &*entry.to_string_lossy()];
    let (code, _, _) = run_mode(&words, &mut |u, g| {
        dropped.push((u, g));
        Ok(())
    });
    assert_eq!((code, dropped), (Some(0), vec![(me_now(), my_gid())]));
    // A failed drop is 126, and nothing was done to the entry.
    let words = [ARG, EMPTY, &uid, &gid, &*entry.to_string_lossy()];
    let (code, _, err) = run_mode(&words, &mut |_, _| Err("setuid: denied".to_owned()));
    assert_eq!(
        (code, err.as_str()),
        (Some(126), "__entry: setuid: denied\n")
    );
    assert!(entry.join("stray").exists());
}

#[test]
fn the_empty_mode_keeps_the_four_folders_empty_and_removes_the_rest() {
    let dir = tree("empty");
    let entry = entry_with_files(&dir);
    let (code, _, err) = call(EMPTY, &entry);
    assert_eq!((code, err.as_str()), (Some(0), ""));
    let mut left: Vec<String> = fs::read_dir(&entry)
        .expect("entry")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    left.sort();
    assert_eq!(left, ["0", "C", "T", "X"]);
    for folder in ["0", "C", "T", "X"] {
        assert_eq!(fs::read_dir(entry.join(folder)).expect("folder").count(), 0);
    }
}

#[test]
fn the_empty_mode_fails_with_the_path_when_something_cannot_be_removed() {
    let dir = tree("empty-fails");
    // The removal of a file in a read-only directory fails, but not for root.
    if rustix::process::geteuid().is_root() {
        eprintln!("skipped: root removes from a read-only directory");
        return;
    }
    let entry = entry_with_files(&dir);
    let locked = entry.join("T").join("deep");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o500)).expect("chmod");
    let (code, _, err) = call(EMPTY, &entry);
    assert_eq!(code, Some(1), "{err}");
    assert!(err.contains(&*locked.to_string_lossy()), "{err}");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).expect("chmod");
}

#[test]
fn the_clear_mode_makes_the_uids_entries_usable() {
    let dir = tree("clear");
    let entry = entry_with_files(&dir);
    let below = entry.join("C").join("deep");
    fs::set_permissions(&below, fs::Permissions::from_mode(0o000)).expect("chmod");
    let (code, _, err) = call(CLEAR, &entry);
    assert_eq!((code, err.as_str()), (Some(0), ""));
    let mode = fs::metadata(&below).expect("meta").permissions().mode();
    assert_eq!(mode & 0o700, 0o700);
}

#[test]
fn the_look_mode_prints_what_is_left_and_nothing_when_clean() {
    let dir = tree("look");
    let entry = entry_with_files(&dir);
    let (code, out, err) = call(LOOK, &entry);
    assert_eq!((code, err.as_str()), (Some(0), ""));
    let shown = |name: &str| out.lines().filter(|l| l.contains(name)).count();
    assert_eq!(shown("stray"), 1, "{out}");
    for folder in ["0", "C", "T", "X"] {
        assert_eq!(shown(&format!("{folder}: [")), 1, "{folder}: {out}");
    }
    call(EMPTY, &entry);
    let (code, out, _) = call(LOOK, &entry);
    assert_eq!((code, out.as_str()), (Some(0), ""));
}

#[test]
fn the_look_mode_fails_when_it_cannot_list_the_entry_or_one_of_its_folders() {
    let dir = tree("look-fails");
    let missing = dir.path().join("missing");
    let (code, out, err) = call(LOOK, &missing);
    assert_eq!((code, out.as_str()), (Some(1), ""));
    assert!(err.contains(&*missing.to_string_lossy()), "{err}");
    // A folder that is a file cannot be listed: seen as a failure, never as empty.
    let entry = dir.path().join("entry");
    fs::create_dir_all(&entry).expect("entry");
    fs::write(entry.join("T"), "x").expect("file");
    let (code, _, err) = call(LOOK, &entry);
    assert_eq!(code, Some(1));
    assert!(err.contains("entry/T"), "{err}");
    // A link in the place of a folder is reported, never followed into its target.
    fs::remove_file(entry.join("T")).expect("rm");
    let target = dir.path().join("elsewhere");
    fs::create_dir_all(&target).expect("target");
    std::os::unix::fs::symlink(&target, entry.join("T")).expect("link");
    let (code, out, err) = call(LOOK, &entry);
    assert_eq!((code, out.as_str()), (Some(1), ""));
    assert!(err.contains("entry/T: not a directory"), "{err}");
}

#[test]
fn the_restore_is_one_call_as_the_uid_and_names_the_entry_when_it_fails() {
    let log = Log::default();
    restore(&me(), Path::new(FOLDERS), &mut runner(&log, &[])).expect("restored");
    assert_eq!(*log.borrow(), vec![mode_call("restore")]);
    let why = restore(
        &me(),
        Path::new(FOLDERS),
        &mut runner(&Log::default(), &["restore"]),
    )
    .expect_err("fails");
    assert_eq!(why, format!("{FOLDERS}: restore failed"));
}

#[test]
fn the_clear_mode_leaves_the_entry_and_its_four_folders_as_the_system_has_them() {
    if rustix::process::geteuid().is_root() {
        eprintln!("skipped: permission bits do not stop root");
        return;
    }
    let dir = tree("clear-keeps");
    let entry = entry_with_files(&dir);
    for folder in ["0", "C", "T", "X"] {
        fs::set_permissions(entry.join(folder), fs::Permissions::from_mode(0o500)).expect("chmod");
    }
    let (code, _, err) = call(CLEAR, &entry);
    assert_eq!((code, err.as_str()), (Some(0), ""));
    for folder in ["0", "C", "T", "X"] {
        let mode = fs::metadata(entry.join(folder))
            .expect("meta")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o500, "{folder}");
    }
    for folder in ["0", "C", "T", "X"] {
        fs::set_permissions(entry.join(folder), fs::Permissions::from_mode(0o700)).expect("chmod");
    }
}

fn restore_in(entry: &Path, uid: u32) -> (Vec<String>, Vec<(PathBuf, u32)>) {
    let mut changed = Vec::new();
    let swept = restore_folders(
        entry,
        uid,
        (
            &mut |path, mode| {
                changed.push((path.to_owned(), mode));
                Ok(())
            },
            &mut |_| Ok(()),
        ),
    );
    (swept.errors, changed)
}

fn set_modes(entry: &Path, modes: [u32; 4]) {
    for (folder, mode) in ["0", "C", "T", "X"].into_iter().zip(modes) {
        fs::set_permissions(entry.join(folder), fs::Permissions::from_mode(mode)).expect("chmod");
    }
}

#[test]
fn the_restore_makes_no_chmod_on_a_folder_within_the_systems_layout() {
    let dir = tree("restore-within");
    let entry = entry_with_files(&dir);
    // The system's own: 0 and X at 755, C and T at 700.
    set_modes(&entry, [0o755, 0o700, 0o700, 0o755]);
    assert_eq!(restore_in(&entry, me_now()), (Vec::new(), Vec::new()));
    // Narrower is within it and is not widened.
    set_modes(&entry, [0o700, 0o700, 0o500, 0o750]);
    assert_eq!(restore_in(&entry, me_now()), (Vec::new(), Vec::new()));
}

#[test]
fn the_restore_narrows_only_a_folder_wider_than_the_systems_layout() {
    let dir = tree("restore-wider");
    let entry = entry_with_files(&dir);
    set_modes(&entry, [0o777, 0o755, 0o710, 0o775]);
    let (errors, changed) = restore_in(&entry, me_now());
    assert_eq!(errors, Vec::<String>::new());
    assert_eq!(
        changed,
        vec![
            (entry.join("0"), 0o755),
            (entry.join("C"), 0o700),
            (entry.join("T"), 0o700),
            (entry.join("X"), 0o755),
        ]
    );
    set_modes(&entry, [0o700; 4]);
}

#[test]
fn the_restore_fails_naming_a_folder_that_is_not_the_pool_users() {
    let dir = tree("restore-owner");
    let entry = entry_with_files(&dir);
    // The test is another uid than the one asked for: the folders are not that uid's.
    let (errors, changed) = restore_in(&entry, me_now() + 1);
    assert_eq!(changed, Vec::new());
    assert_eq!(errors.len(), 4, "{errors:?}");
    assert!(
        errors[0].starts_with(&*entry.join("0").to_string_lossy()),
        "{errors:?}"
    );
    assert!(errors[0].contains("not the pool user"), "{errors:?}");
}

#[test]
fn the_restore_skips_a_missing_folder_and_fails_on_a_file_or_a_refused_change() {
    let dir = tree("restore-odd");
    let entry = dir.path().join("entry");
    fs::create_dir_all(entry.join("T")).expect("T");
    fs::write(entry.join("C"), "x").expect("file");
    fs::set_permissions(entry.join("T"), fs::Permissions::from_mode(0o750)).expect("chmod");
    let mut refuse = |_: &Path, _: u32| Err(std::io::Error::from_raw_os_error(1));
    let swept = restore_folders(&entry, me_now(), (&mut refuse, &mut |_| Ok(())));
    assert_eq!(swept.errors.len(), 2, "{:?}", swept.errors);
    assert!(
        swept.errors[0].contains("C: not a directory"),
        "{:?}",
        swept.errors
    );
    assert!(swept.errors[1].contains("T: "), "{:?}", swept.errors);
    // The entry itself is not a directory: the folders under it cannot be looked at.
    let file = dir.path().join("file");
    fs::write(&file, "x").expect("file");
    let swept = restore_folders(&file, me_now(), (&mut refuse, &mut |_| Ok(())));
    assert_eq!(swept.errors.len(), 4, "{:?}", swept.errors);
}

#[test]
fn the_restore_mode_is_selected_and_runs_as_the_uid() {
    let dir = tree("restore-mode");
    let entry = entry_with_files(&dir);
    fs::set_permissions(entry.join("T"), fs::Permissions::from_mode(0o755)).expect("chmod");
    fs::set_permissions(entry.join("0"), fs::Permissions::from_mode(0o755)).expect("chmod");
    let (code, _, err) = call(RESTORE, &entry);
    assert_eq!((code, err.as_str()), (Some(0), ""));
    let mode = fs::metadata(entry.join("T"))
        .expect("meta")
        .permissions()
        .mode();
    assert_eq!(mode & 0o7777, 0o700);
    // `0` at 755 is the system's own and stays.
    let zero = fs::metadata(entry.join("0"))
        .expect("meta")
        .permissions()
        .mode();
    assert_eq!(zero & 0o7777, 0o755);
}

#[test]
fn only_an_attribute_that_went_or_is_protected_is_left_on_a_folder() {
    assert!(xattr_left(libc::EPERM));
    assert!(xattr_left(NO_ATTR));
    assert!(!xattr_left(libc::EIO));
    assert!(!xattr_left(libc::EACCES));
}

fn set_attr(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt as _;
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).expect("path");
    #[cfg(target_os = "macos")]
    let name = c"kbf.assert";
    #[cfg(not(target_os = "macos"))]
    let name = c"user.kbf.assert";
    #[cfg(target_os = "macos")]
    // SAFETY: NUL-terminated strings and a 1-byte value.
    let set = unsafe {
        libc::setxattr(
            c_path.as_ptr(),
            name.as_ptr(),
            b"x".as_ptr().cast(),
            1,
            0,
            0,
        )
    };
    #[cfg(not(target_os = "macos"))]
    // SAFETY: NUL-terminated strings and a 1-byte value.
    let set = unsafe { libc::setxattr(c_path.as_ptr(), name.as_ptr(), b"x".as_ptr().cast(), 1, 0) };
    set == 0
}

fn has_attr(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt as _;
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).expect("path");
    #[cfg(target_os = "macos")]
    // SAFETY: NUL-terminated strings; a null buffer of size 0 only asks for the size.
    let got = unsafe {
        libc::getxattr(
            c_path.as_ptr(),
            c"kbf.assert".as_ptr(),
            std::ptr::null_mut(),
            0,
            0,
            0,
        )
    };
    #[cfg(not(target_os = "macos"))]
    // SAFETY: NUL-terminated strings; a null buffer of size 0 only asks for the size.
    let got = unsafe {
        libc::getxattr(
            c_path.as_ptr(),
            c"user.kbf.assert".as_ptr(),
            std::ptr::null_mut(),
            0,
        )
    };
    got >= 0
}

#[test]
fn the_restore_takes_an_attribute_off_a_folder() {
    let dir = tree("restore-xattr");
    let entry = entry_with_files(&dir);
    if !set_attr(&entry.join("0")) {
        eprintln!("skipped: this file system keeps no extended attributes");
        return;
    }
    assert!(has_attr(&entry.join("0")));
    let (code, _, err) = call(RESTORE, &entry);
    assert_eq!((code, err.as_str()), (Some(0), ""));
    assert!(!has_attr(&entry.join("0")));
    // A path that cannot be a C string is an error.
    let bad = Path::new("a\0b");
    assert!(strip_xattrs(bad).is_err());
    // A path that is not there lists nothing and is no error to the caller's walk.
    assert!(strip_xattrs(&entry.join("absent")).is_err());
}

#[test]
fn a_refused_strip_of_a_folder_fails_the_restore_naming_it() {
    let dir = tree("restore-strip");
    let entry = entry_with_files(&dir);
    let swept = restore_folders(
        &entry,
        me_now(),
        (&mut |_, _| Ok(()), &mut |_| {
            Err(std::io::Error::from_raw_os_error(libc::EIO))
        }),
    );
    assert_eq!(swept.errors.len(), 4, "{:?}", swept.errors);
    assert!(swept.errors[0].starts_with(&*entry.join("0").to_string_lossy()));
}

#[test]
fn the_strip_removes_each_listed_attribute_and_leaves_what_the_system_protects() {
    // Two names listed; the first is protected, the second goes.
    let mut removed = Vec::new();
    let mut list = |buf: Option<&mut [u8]>| match buf {
        None => Ok(7),
        Some(buf) => {
            buf.copy_from_slice(b"a\0b\0\0c\0");
            Ok(7)
        }
    };
    let mut remove = |name: &std::ffi::CStr| {
        removed.push(name.to_string_lossy().into_owned());
        if name.to_bytes() == b"a" {
            Err(libc::EPERM)
        } else {
            Ok(())
        }
    };
    strip_with(&mut list, &mut remove).expect("protected is left");
    assert_eq!(removed, ["a", "b", "c"]);
    // Any other refusal of a removal fails.
    let mut list = |buf: Option<&mut [u8]>| match buf {
        None => Ok(2),
        Some(buf) => {
            buf.copy_from_slice(b"a\0");
            Ok(2)
        }
    };
    let err = strip_with(&mut list, &mut |_| Err(libc::EIO)).expect_err("fails");
    assert_eq!(err.raw_os_error(), Some(libc::EIO));
    // A name with no more room than the listing said is cut to what was got.
    let mut list = |buf: Option<&mut [u8]>| match buf {
        None => Ok(4),
        Some(_) => Ok(0),
    };
    strip_with(&mut list, &mut |_| panic!("nothing listed")).expect("empty");
}

#[test]
fn a_listing_that_fails_is_left_when_protected_and_an_error_otherwise() {
    for (first_fails, errno, left) in [
        (true, libc::EPERM, true),
        (true, libc::EIO, false),
        (false, libc::EPERM, true),
        (false, libc::EIO, false),
    ] {
        let mut calls = 0;
        let mut list = |_: Option<&mut [u8]>| {
            calls += 1;
            if first_fails || calls == 2 {
                Err(errno)
            } else {
                Ok(1)
            }
        };
        let got = strip_with(&mut list, &mut |_| Ok(()));
        assert_eq!(got.is_ok(), left, "{first_fails} {errno}");
    }
}

#[test]
fn a_refusal_line_of_find_or_chmod_names_its_path() {
    for (line, path) in [
        ("find: /a/b: Operation not permitted", Some("/a/b")),
        ("find: /a/b: Permission denied", Some("/a/b")),
        (
            "chmod: Failed to clear ACL on file /a/c: Operation not permitted",
            Some("/a/c"),
        ),
        ("find: /a/b: No such file or directory", None),
        ("chmod: invalid option", None),
        ("/a/b: Operation not permitted", None),
    ] {
        assert_eq!(refused_path(line), path.map(Path::new), "{line}");
    }
}

fn fake_find(
    ok: bool,
    said: &str,
) -> impl FnMut(&[OsString]) -> std::io::Result<(bool, String)> + use<'_> {
    move |_| Ok((ok, said.to_owned()))
}

#[test]
fn the_acl_mode_fails_on_a_find_error_that_is_not_the_systems_refusal() {
    let dir = tree("acl");
    let vault = dir.path().join("vault");
    fs::create_dir_all(&vault).expect("vault");
    fs::set_permissions(&vault, fs::Permissions::from_mode(0o000)).expect("chmod");
    let skip = !rustix::process::geteuid().is_root();
    let refusal = format!("find: {}: Operation not permitted\n", vault.display());
    let mut err = Vec::new();
    assert_eq!(
        acl_mode(dir.path(), 5, &mut fake_find(true, ""), &mut err),
        0
    );
    if skip {
        // Only refusals of entries the pool user cannot open are let go.
        assert_eq!(
            acl_mode(dir.path(), 5, &mut fake_find(false, &refusal), &mut err),
            0
        );
        assert!(err.is_empty());
    }
    let other = "find: /x: Input/output error\n";
    let both = format!("{refusal}{other}");
    assert_eq!(
        acl_mode(dir.path(), 5, &mut fake_find(false, &both), &mut err),
        1
    );
    let said = String::from_utf8(err).expect("utf8");
    assert!(said.contains("Input/output error"), "{said}");
    assert!(!said.contains("vault"), "{said}");
    let mut err = Vec::new();
    assert_eq!(
        acl_mode(dir.path(), 5, &mut fake_find(false, ""), &mut err),
        1
    );
    assert!(String::from_utf8_lossy(&err).contains("failed"));
    let mut broken = |_: &[OsString]| Err(std::io::Error::other("no find"));
    let mut err = Vec::new();
    assert_eq!(acl_mode(dir.path(), 5, &mut broken, &mut err), 1);
    assert!(String::from_utf8_lossy(&err).contains("no find"));
    fs::set_permissions(&vault, fs::Permissions::from_mode(0o700)).expect("chmod");
}

#[test]
fn the_acl_mode_runs_find_on_a_real_tree() {
    let dir = tree("acl-real");
    let entry = entry_with_files(&dir);
    let (code, _, _) = call(ACL, &entry);
    assert!(matches!(code, Some(0 | 1)), "{code:?}");
}

#[test]
fn the_home_is_emptied_as_the_uid_and_a_directory_it_owns_with_no_permission_is_opened() {
    if rustix::process::geteuid().is_root() {
        eprintln!("skipped: permission bits do not stop root");
        return;
    }
    let dir = tree("empty-home");
    let home = dir.path().join("home");
    fs::create_dir_all(home.join("Library/Preferences")).expect("dirs");
    fs::write(home.join("Library/x"), "x").expect("file");
    fs::write(home.join("top"), "x").expect("file");
    fs::set_permissions(
        home.join("Library/Preferences"),
        fs::Permissions::from_mode(0o000),
    )
    .expect("chmod");
    let (code, _, err) = call(EMPTY_HOME, &home);
    assert_eq!((code, err.as_str()), (Some(0), ""));
    // The uid owns it, so it can give itself the permission back: nothing keeps its files.
    assert!(!home.join("Library").exists());
    assert!(!home.join("top").exists());
    // The look agrees: nothing is left.
    let (code, out, err) = call(LOOK_HOME, &home);
    assert_eq!((code, out.as_str(), err.as_str()), (Some(0), "", ""));
}

#[test]
fn the_look_of_the_home_reports_files_and_empty_directories_it_can_open() {
    let dir = tree("look-home");
    let home = dir.path().join("home");
    fs::create_dir_all(home.join("a/b")).expect("dirs");
    fs::create_dir_all(home.join("full")).expect("dirs");
    fs::write(home.join("full/f"), "x").expect("file");
    std::os::unix::fs::symlink("/nowhere", home.join("link")).expect("link");
    let (code, out, err) = call(LOOK_HOME, &home);
    assert_eq!((code, err.as_str()), (Some(0), ""));
    let lines: Vec<&str> = out.lines().collect();
    let at = |name: &str| home.join(name).display().to_string();
    // A file, a link and an empty directory; a directory holding a file is not itself named.
    assert_eq!(
        lines,
        [at("a/b"), at("full/f"), at("link")]
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
    );
    // A home that cannot be listed fails.
    let (code, _, err) = call(LOOK_HOME, &dir.path().join("absent"));
    assert_eq!(code, Some(1));
    assert!(err.contains("absent"), "{err}");
}

#[test]
fn the_look_of_the_home_and_of_the_entry_leave_out_what_the_user_cannot_open() {
    if rustix::process::geteuid().is_root() {
        eprintln!("skipped: permission bits do not stop root");
        return;
    }
    let dir = tree("look-protected");
    let entry = dir.path().join("entry");
    for folder in ["0", "C", "T", "X"] {
        fs::create_dir_all(entry.join(folder)).expect("folder");
    }
    let vault = entry.join("T").join("vault");
    fs::create_dir_all(&vault).expect("vault");
    fs::set_permissions(&vault, fs::Permissions::from_mode(0o000)).expect("chmod");
    let (code, out, err) = call(LOOK, &entry);
    assert_eq!((code, out.as_str(), err.as_str()), (Some(0), "", ""));
    fs::create_dir_all(entry.join("T").join("kept")).expect("kept");
    let (_, out, _) = call(LOOK, &entry);
    assert!(out.contains("kept") && !out.contains("vault"), "{out}");
    // A directory that holds only a protected one is not named in the home's look.
    let home = dir.path().join("home");
    fs::create_dir_all(home.join("Library")).expect("dirs");
    let inner = home.join("Library/Preferences");
    fs::create_dir_all(&inner).expect("dirs");
    fs::set_permissions(&inner, fs::Permissions::from_mode(0o000)).expect("chmod");
    let (code, out, _) = call(LOOK_HOME, &home);
    assert_eq!((code, out.as_str()), (Some(0), ""));
    fs::set_permissions(&inner, fs::Permissions::from_mode(0o700)).expect("chmod");
    fs::set_permissions(&vault, fs::Permissions::from_mode(0o700)).expect("chmod");
}

#[test]
fn only_leftovers_all_in_the_keychains_are_waited_for() {
    let home = Path::new(HOME);
    let line = |rest: &str| format!("{HOME}/{rest}");
    let some = [
        line("Library/Keychains/.flA5"),
        line("Library/Keychains/login"),
    ];
    assert!(keychain_only(&some, home));
    assert!(!keychain_only(&[], home));
    assert!(!keychain_only(&[line("Library/Preferences/x")], home));
    assert!(!keychain_only(
        &[line("Library/Keychains/a"), line("top")],
        home
    ));
    assert!(!keychain_only(&[line("Library/KeychainsX/a")], home));
}

/// A runner for the contents step: `empties` is how many of the home's empties fail, then
/// succeed; the look of the home says `left`.
fn keychain_runner<'a>(
    log: &'a Log,
    mut fail_empties: usize,
    left: &'a str,
) -> impl FnMut(&Path, &[OsString]) -> Result<String, String> + 'a {
    move |program, args| {
        let line = format!("{} {}", program.display(), words(args));
        log.borrow_mut().push(line.clone());
        if line.contains("__entry empty-home") && fail_empties > 0 {
            fail_empties -= 1;
            return Err(format!("{HOME}/Library: Operation not permitted"));
        }
        if line.contains("__entry look-home") {
            return Ok(left.to_owned());
        }
        Ok(String::new())
    }
}

#[test]
fn a_home_held_by_the_keychain_is_emptied_again_once_after_a_wait() {
    let dirs = (Path::new(HOME), Path::new(FOLDERS));
    let held = format!("{HOME}/Library/Keychains/.flA5\\n");
    // One failure, then it goes: one wait, the empty made twice, the entry emptied.
    let log = Log::default();
    let mut waits = Vec::new();
    contents_step_waiting(
        &me(),
        dirs,
        &mut keychain_runner(&log, 1, &held),
        &mut |d| waits.push(d),
    )
    .expect("emptied on the second try");
    assert_eq!(waits, vec![KEYCHAIN_POLL]);
    assert_eq!(
        *log.borrow(),
        vec![
            home_call("keychain"),
            home_call("empty-home"),
            home_call("look-home"),
            home_call("empty-home"),
            mode_call("empty"),
        ]
    );
    // Still there after the wait: it fails, naming the path; it is never skipped.
    let log = Log::default();
    let mut waits = Vec::new();
    let why = contents_step_waiting(
        &me(),
        dirs,
        &mut keychain_runner(&log, usize::MAX, &held),
        &mut |d| waits.push(d),
    )
    .expect_err("fails");
    // Polled every 2 s for 60 s, then the failure names what is left.
    assert_eq!(waits.len(), 30);
    assert!(waits.iter().all(|w| *w == KEYCHAIN_POLL));
    assert!(why.contains(".flA5"), "{why}");
    assert!(why.contains("Operation not permitted"), "{why}");
}

#[test]
fn a_home_that_fails_for_another_reason_is_not_waited_for() {
    let dirs = (Path::new(HOME), Path::new(FOLDERS));
    let other = format!("{HOME}/Library/Preferences/x\\n");
    for left in [other.as_str(), ""] {
        let log = Log::default();
        let mut waits = 0;
        let why = contents_step_waiting(
            &me(),
            dirs,
            &mut keychain_runner(&log, 1, left),
            &mut |_| waits += 1,
        )
        .expect_err("fails at once");
        assert_eq!(waits, 0, "{left:?}");
        assert!(why.contains("Operation not permitted"), "{why}");
    }
    // The home cannot be looked at: no wait either.
    let log = Log::default();
    let mut run = |program: &Path, args: &[OsString]| {
        let line = format!("{} {}", program.display(), words(args));
        log.borrow_mut().push(line.clone());
        if line.contains("empty-home") || line.contains("look-home") {
            return Err("failed".to_owned());
        }
        Ok(String::new())
    };
    let mut waits = 0;
    contents_step_waiting(&me(), dirs, &mut run, &mut |_| waits += 1).expect_err("fails");
    assert_eq!(waits, 0);
}

#[cfg(not(target_os = "macos"))]
#[test]
fn a_directory_the_system_placed_is_accepted_when_empty_and_its_contents_are_not() {
    use kbf_mac_session::sweep::pretend_placed;
    let dir = tree("placed");
    let entry = entry_with_files(&dir);
    call(EMPTY, &entry);
    let placed = entry.join("T").join("com.apple.trustd");
    fs::create_dir_all(&placed).expect("placed");
    pretend_placed(&["com.apple.trustd"]);
    // Empty, it is accepted by the look of the entry and of a home that holds it.
    let (code, out, err) = call(LOOK, &entry);
    assert_eq!((code, out.as_str(), err.as_str()), (Some(0), "", ""));
    let home = dir.path().join("home");
    fs::create_dir_all(home.join("Library").join("com.apple.trustd")).expect("dirs");
    let (code, out, _) = call(LOOK_HOME, &home);
    assert_eq!((code, out.as_str()), (Some(0), ""));
    // What is in it is left, and named.
    fs::write(placed.join("leak"), "x").expect("file");
    let (code, out, _) = call(LOOK, &entry);
    assert_eq!(code, Some(0));
    assert!(out.contains("com.apple.trustd/leak"), "{out}");
    // The empty keeps it and removes what is in it.
    let (code, _, err) = call(EMPTY, &entry);
    assert_eq!((code, err.as_str()), (Some(0), ""));
    assert!(placed.is_dir() && !placed.join("leak").exists());
    // The ACL pass lets go the refusal for it.
    let refusal = format!(
        "chmod: Failed to clear ACL on file {}: Operation not permitted\n",
        placed.display()
    );
    let mut err = Vec::new();
    assert_eq!(
        acl_mode(&entry, 5, &mut fake_find(false, &refusal), &mut err),
        0
    );
    pretend_placed(&[]);
    assert_eq!(
        acl_mode(&entry, 5, &mut fake_find(false, &refusal), &mut err),
        1
    );
}

#[test]
fn the_keychain_items_are_deleted_first_and_a_failure_names_the_home_and_is_not_skipped() {
    let dirs = (Path::new(HOME), Path::new(FOLDERS));
    let log = Log::default();
    let why = contents_step_waiting(
        &me(),
        dirs,
        &mut runner(&log, &["__entry keychain"]),
        &mut |_| panic!("no wait"),
    )
    .expect_err("fails");
    assert!(
        why.starts_with(&format!("keychain items of {HOME}: ")),
        "{why}"
    );
    // The home and the entry are emptied anyway.
    assert_eq!(log.borrow().len(), 3);
}

#[test]
fn the_keychain_mode_prints_what_failed_and_exits_1() {
    let mut err = Vec::new();
    assert_eq!(keychain_mode(&mut Vec::new, &mut err), 0);
    assert!(err.is_empty());
    let mut wipe = || vec!["key in the file keychain: delete status -50".to_owned()];
    assert_eq!(keychain_mode(&mut wipe, &mut err), 1);
    assert!(String::from_utf8_lossy(&err).contains("delete status -50"));
}

#[cfg(not(target_os = "macos"))]
#[test]
fn the_keychain_mode_is_selected_and_has_nothing_to_wipe_off_macos() {
    let dir = tree("keychain");
    let (code, out, err) = call(KEYCHAIN, dir.path());
    assert_eq!((code, out.as_str(), err.as_str()), (Some(0), "", ""));
}
