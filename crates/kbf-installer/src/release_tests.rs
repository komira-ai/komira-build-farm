use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::process::Output;
use std::time::{Duration, UNIX_EPOCH};

use kbf_host::exec::{Entry, Meta};
use kbf_host::path::Root;
use kbf_host::step::{Cmd, HttpUrl, Step};
use kbf_host::testkit::{ScratchHost, scratch};
use kbf_host::trust::Untrusted;

use super::super::state::PendingConfirm;
use super::super::state::tests::{A, B, C, c};
use super::*;

const D: &str = "dddddddddddddddddddddddddddddddddddddddd";
const E: &str = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";
const S: &str = "ffffffffffffffffffffffffffffffffffffffff";

/// The test's own user, standing in for root as the owner of a release: a scratch apply
/// can chown only to it.
fn me() -> Owner {
    Owner {
        uid: rustix::process::geteuid().as_raw(),
        gid: rustix::process::getegid().as_raw(),
    }
}

/// A scratch host with the directories a Linux host has above the layout.
fn host(name: &str) -> (ScratchHost, Layout) {
    let exec = ScratchHost::new(&format!("deploy-release-{name}"));
    for sub in ["run", "var/lib", "usr/local", "etc"] {
        fs::create_dir_all(exec.root.join(sub)).expect("seed");
    }
    let root = Layout::linux(Root::new(&exec.root).expect("absolute"));
    (exec, root)
}

/// A release to install: `kbf` holding `binary`, and `units/` with `units` when given.
fn source(name: &str, binary: &str, units: Option<&[(&str, &str)]>) -> PathBuf {
    let dir = scratch(&format!("deploy-release-src-{name}"));
    fs::write(dir.join("kbf"), binary).expect("binary");
    if let Some(units) = units {
        fs::create_dir(dir.join("units")).expect("units");
        for (unit, text) in units {
            fs::write(dir.join("units").join(unit), text).expect("unit");
        }
    }
    dir
}

const UNITS: &[(&str, &str)] = &[
    ("kbf-server.service", "[Unit]\n"),
    ("kbf.slice", "[Slice]\n"),
];

fn install_on(
    exec: &mut ScratchHost,
    root: &Layout,
    commit: &str,
    from: &Path,
    mode: Mode,
) -> Result<String, ReleaseError> {
    let a = Install {
        commit: c(commit),
        from: from.to_owned(),
    };
    let mut out = Vec::new();
    install(exec, root, me(), &a, mode, &mut out)?;
    Ok(String::from_utf8(out).expect("utf-8"))
}

fn rollback_on(exec: &mut ScratchHost, root: &Layout, mode: Mode) -> Result<String, ReleaseError> {
    let mut out = Vec::new();
    rollback(exec, root, mode, &mut out)?;
    Ok(String::from_utf8(out).expect("utf-8"))
}

fn gc_on(exec: &mut ScratchHost, root: &Layout, keep: usize) -> Result<String, ReleaseError> {
    let mut out = Vec::new();
    gc(exec, root, keep, Mode::Apply, &mut out)?;
    Ok(String::from_utf8(out).expect("utf-8"))
}

/// Where a link under `/usr/local/kbf` points; `None` when there is no link.
fn link(exec: &ScratchHost, name: &str) -> Option<String> {
    let target = fs::read_link(exec.at(&format!("/usr/local/kbf/{name}"))).ok()?;
    Some(target.display().to_string())
}

fn state_of(root: &Layout) -> State {
    state::read_unlocked(root).expect("read").expect("a state")
}

fn mode_of(exec: &ScratchHost, path: &str) -> u32 {
    fs::symlink_metadata(exec.at(path)).expect(path).mode() & 0o7777
}

/// Catches an install that leaves `current` or `previous` on the wrong release (or
/// records other commits than the links name), a rollback that does not go back, a
/// rollback with no previous release that does anything but refuse (it would point
/// `current` at nothing, or at itself), and a release copied with other bytes or modes.
///
/// Mutant seen red: rollback falling back to the current release when there is no
/// previous one (`state.previous.clone().or(state.current.clone())`).
#[test]
fn install_twice_then_roll_back_and_refuse_a_second_rollback() {
    let (mut exec, root) = host("flow");
    let a = source("flow-a", "kbf A", Some(UNITS));
    let out = install_on(&mut exec, &root, A, &a, Mode::Apply).expect("install A");
    assert!(
        out.contains(&format!(
            "links: current none -> {A}, previous none -> none\n"
        )),
        "{out}"
    );
    assert!(
        out.ends_with("then check `kbf-deploy host status`\n"),
        "{out}"
    );
    assert_eq!(link(&exec, "current"), Some(format!("releases/{A}")));
    assert_eq!(link(&exec, "previous"), None);
    let rel = format!("/usr/local/kbf/releases/{A}");
    assert_eq!(
        fs::read_to_string(exec.at(&format!("{rel}/kbf"))).expect("kbf"),
        "kbf A"
    );
    let unit = fs::read_to_string(exec.at(&format!("{rel}/units/kbf.slice"))).expect("unit");
    assert_eq!(unit, "[Slice]\n");
    let modes = [
        ("/usr/local/kbf", 0o755),
        ("/usr/local/kbf/releases", 0o755),
        (rel.as_str(), 0o755),
        (&format!("{rel}/kbf"), 0o755),
        (&format!("{rel}/units"), 0o755),
        (&format!("{rel}/units/kbf-server.service"), 0o644),
    ];
    for (path, mode) in modes {
        assert_eq!(mode_of(&exec, path), mode, "{path}");
    }

    let b = source("flow-b", "kbf B", None);
    let out = install_on(&mut exec, &root, B, &b, Mode::Apply).expect("install B");
    assert!(
        out.contains(&format!(
            "links: current {A} -> {B}, previous none -> {A}\n"
        )),
        "{out}"
    );
    assert_eq!(link(&exec, "current"), Some(format!("releases/{B}")));
    assert_eq!(link(&exec, "previous"), Some(format!("releases/{A}")));
    let s = state_of(&root);
    assert_eq!((s.current, s.previous), (Some(c(B)), Some(c(A))));

    let out = rollback_on(&mut exec, &root, Mode::Apply).expect("rollback");
    assert!(
        out.contains(&format!(
            "links: current {B} -> {A}, previous {A} -> none\n"
        )),
        "{out}"
    );
    assert_eq!(link(&exec, "current"), Some(format!("releases/{A}")));
    assert_eq!(link(&exec, "previous"), None);
    let s = state_of(&root);
    assert_eq!((s.current, s.previous), (Some(c(A)), None));

    let err = rollback_on(&mut exec, &root, Mode::Apply).expect_err("no previous");
    assert_eq!(err.to_string(), "no previous release to roll back to");
    assert_eq!(link(&exec, "current"), Some(format!("releases/{A}")));
    assert_eq!(state_of(&root).current, Some(c(A)));
}

/// Catches a rollback onto a release whose directory (or binary) is gone, which would
/// leave `current` pointing at nothing a service can start.
#[test]
fn a_rollback_onto_a_release_that_is_gone_is_refused() {
    let (mut exec, root) = host("gone");
    install_on(
        &mut exec,
        &root,
        A,
        &source("gone-a", "A", None),
        Mode::Apply,
    )
    .expect("A");
    install_on(
        &mut exec,
        &root,
        B,
        &source("gone-b", "B", None),
        Mode::Apply,
    )
    .expect("B");
    fs::remove_file(exec.at(&format!("/usr/local/kbf/releases/{A}/kbf"))).expect("remove");
    let err = rollback_on(&mut exec, &root, Mode::Apply).expect_err("gone");
    assert_eq!(
        err.to_string(),
        format!("the previous release {A} is gone (no binary at its directory)")
    );
    assert_eq!(link(&exec, "current"), Some(format!("releases/{B}")));
}

/// Catches an install that replaces a release directory already there with other
/// content (a commit names one build; different bytes under its name are a wrong or
/// tampered copy), that takes a file at the release's path for a directory, or that
/// refuses the same release again; and an install of the current release that moves
/// `previous` onto it.
#[test]
fn a_release_directory_with_other_content_is_refused() {
    let (mut exec, root) = host("differs");
    let a = source("differs-a", "kbf A", Some(UNITS));
    install_on(&mut exec, &root, A, &a, Mode::Apply).expect("install A");
    let rel = format!("/usr/local/kbf/releases/{A}");
    let other_binary = source("differs-bin", "kbf X", Some(UNITS));
    let other_unit = source("differs-unit", "kbf A", Some(&[("kbf.slice", "[Other]\n")]));
    let new_unit = source("differs-new", "kbf A", Some(&[("new.service", "[Unit]\n")]));
    let cases = [
        (other_binary, format!("{rel}/kbf")),
        (other_unit, format!("{rel}/units/kbf.slice")),
        (new_unit, format!("{rel}/units/new.service")),
    ];
    for (from, path) in cases {
        let err = install_on(&mut exec, &root, A, &from, Mode::Apply).expect_err(&path);
        assert_eq!(
            err.to_string(),
            format!("{path} differs from the release being installed: a release is never replaced")
        );
    }
    assert_eq!(
        fs::read_to_string(exec.at(&format!("{rel}/kbf"))).expect("kbf"),
        "kbf A"
    );

    install_on(&mut exec, &root, A, &a, Mode::Apply).expect("the same release again");
    let s = state_of(&root);
    assert_eq!((s.current, s.previous), (Some(c(A)), None));

    fs::write(exec.at(&format!("/usr/local/kbf/releases/{B}")), "a file").expect("seed");
    let err = install_on(&mut exec, &root, B, &a, Mode::Apply).expect_err("a file");
    assert_eq!(
        err.to_string(),
        format!("/usr/local/kbf/releases/{B} is there and is not a directory")
    );
    assert_eq!(state_of(&root).current, Some(c(A)));
}

/// A [`ScratchHost`] whose copy of `kbf` lands wrong: with other bytes, or not at all.
struct BadCopy {
    host: ScratchHost,
    skip: bool,
    /// The file whose copy lands wrong: the path ends with this.
    hit: &'static str,
}

impl Exec for BadCopy {
    fn euid(&self) -> u32 {
        self.host.euid()
    }
    fn inspect(&self, path: &HostPath) -> io::Result<Option<Entry>> {
        self.host.inspect(path)
    }
    fn stat(&self, path: &HostPath) -> io::Result<Option<Meta>> {
        self.host.stat(path)
    }
    fn query(&self, cmd: &Cmd) -> io::Result<Output> {
        self.host.query(cmd)
    }
    fn http_status(&self, url: &HttpUrl) -> io::Result<u16> {
        self.host.http_status(url)
    }
    fn apply(&mut self, step: &Step) -> io::Result<()> {
        match step {
            Step::WriteFile {
                path, perms, owner, ..
            } if path.as_str().ends_with(self.hit) => {
                if self.skip {
                    return Ok(());
                }
                let bytes = b"tampered".to_vec();
                let (path, perms, owner) = (path.clone(), *perms, *owner);
                self.host.apply(&Step::WriteFile {
                    path,
                    perms,
                    owner,
                    bytes,
                })
            }
            _ => self.host.apply(step),
        }
    }
    fn check_binary(&self) -> Result<(), Untrusted> {
        self.host.check_binary()
    }
}

/// Catches a copy that is not checked before `current` names it: a binary that landed
/// with other bytes, or not at all, is refused and no link or state moves.
#[test]
fn a_copy_whose_sha256_differs_is_refused_before_the_switch() {
    let from = source("badcopy", "kbf A", None);
    let want = sha256(b"kbf A");
    for (skip, got) in [
        (false, sha256(b"tampered")),
        (true, "none (no file)".to_owned()),
    ] {
        let (host, root) = host(&format!("badcopy-{skip}"));
        let mut exec = BadCopy {
            host,
            skip,
            hit: "/kbf",
        };
        let a = Install {
            commit: c(A),
            from: from.clone(),
        };
        let err = install(&mut exec, &root, me(), &a, Mode::Apply, &mut Vec::new())
            .expect_err("a bad copy");
        let path = format!("/usr/local/kbf/releases/{A}/kbf");
        assert_eq!(
            err.to_string(),
            format!("the copy at {path} has sha256 {got}; the source has {want}")
        );
        assert_eq!(link(&exec.host, "current"), None);
        assert_eq!(state::read_unlocked(&root).expect("read"), None);
    }
}

/// Catches a copy checked for the first binary only: in a release of two binaries the
/// second lands with other bytes (or not at all) and is refused the same way, before any
/// link or state names the release.
///
/// Mutant seen red: the read-back loop over `source.binaries` cut to its first element.
#[test]
fn every_binary_of_a_release_is_read_back_not_only_the_first() {
    let from = source("badcopy2", "kbf A", None);
    fs::write(from.join("kbf-daemon"), "daemon A").expect("daemon");
    for (skip, got) in [
        (false, sha256(b"tampered")),
        (true, "none (no file)".to_owned()),
    ] {
        let (host, root) = host(&format!("badcopy2-{skip}"));
        let mut exec = BadCopy {
            host,
            skip,
            hit: "/kbf-daemon",
        };
        let a = Install {
            commit: c(A),
            from: from.clone(),
        };
        let err = install(&mut exec, &root, me(), &a, Mode::Apply, &mut Vec::new())
            .expect_err("a bad copy of the second binary");
        let want = sha256(b"daemon A");
        let path = format!("/usr/local/kbf/releases/{A}/kbf-daemon");
        assert_eq!(
            err.to_string(),
            format!("the copy at {path} has sha256 {got}; the source has {want}")
        );
        assert_eq!(link(&exec.host, "current"), None);
        assert_eq!(state::read_unlocked(&root).expect("read"), None);
    }
}

/// Every file and link under `dir`, with its mode and its bytes or target.
fn tree(dir: &Path) -> BTreeMap<PathBuf, (u32, String)> {
    let mut found = BTreeMap::new();
    let mut dirs = vec![dir.to_owned()];
    while let Some(d) = dirs.pop() {
        for item in fs::read_dir(&d).expect("list") {
            let p = item.expect("entry").path();
            let meta = fs::symlink_metadata(&p).expect("stat");
            let what = if meta.is_dir() {
                dirs.push(p.clone());
                String::new()
            } else if meta.file_type().is_symlink() {
                fs::read_link(&p).expect("link").display().to_string()
            } else {
                fs::read_to_string(&p).expect("file")
            };
            found.insert(p, (meta.mode(), what));
        }
    }
    found
}

/// Catches a dry run that changes anything: copies, moves a link, writes the state or
/// even makes the lock file. Each verb runs dry on a fresh host and on one with two
/// releases, and the tree must be byte for byte what it was.
///
/// Mutant seen red: the lock taken in a dry run too (`Mode::DryRun => Some(..)`).
#[test]
fn a_dry_run_writes_nothing() {
    let (mut exec, root) = host("dry");
    let a = source("dry-a", "kbf A", Some(UNITS));
    let before = tree(&exec.root);
    let out = install_on(&mut exec, &root, A, &a, Mode::DryRun).expect("dry install");
    assert!(
        out.contains("DRY-RUN: write /usr/local/kbf/releases/"),
        "{out}"
    );
    assert!(!out.contains("next, by hand"), "{out}");
    let mut out = Vec::new();
    gc(&mut exec, &root, 0, Mode::DryRun, &mut out).expect("dry gc");
    assert_eq!(
        String::from_utf8(out).expect("utf-8"),
        "keeping: none; removing: none\nnothing to do\n"
    );
    assert_eq!(tree(&exec.root), before);
    assert!(!exec.at("/run/kbf-deploy.lock").exists());

    install_on(&mut exec, &root, A, &a, Mode::Apply).expect("A");
    install_on(
        &mut exec,
        &root,
        B,
        &source("dry-b", "kbf B", None),
        Mode::Apply,
    )
    .expect("B");
    fs::create_dir(exec.at(&format!("/usr/local/kbf/releases/{C}"))).expect("seed");
    let before = tree(&exec.root);
    let out = install_on(
        &mut exec,
        &root,
        C,
        &source("dry-c", "C", None),
        Mode::DryRun,
    );
    assert!(out.expect_err("C differs").to_string().contains("differs"));
    install_on(
        &mut exec,
        &root,
        D,
        &source("dry-d", "D", None),
        Mode::DryRun,
    )
    .expect("D");
    let out = rollback_on(&mut exec, &root, Mode::DryRun).expect("dry rollback");
    assert!(
        out.contains(&format!(
            "DRY-RUN: link /usr/local/kbf/current -> releases/{A}"
        )),
        "{out}"
    );
    let mut out = Vec::new();
    gc(&mut exec, &root, 0, Mode::DryRun, &mut out).expect("dry gc");
    let out = String::from_utf8(out).expect("utf-8");
    assert!(
        out.contains(&format!("DRY-RUN: remove /usr/local/kbf/releases/{C}\n")),
        "{out}"
    );
    assert_eq!(tree(&exec.root), before);
}

/// A release directory made by hand: `kbf`, `units/a.service`, and a link out of it
/// to `escape` (removed, never followed), modified at `secs` after the epoch.
fn seed_release(exec: &ScratchHost, commit: &str, secs: u64) {
    let dir = exec.at(&format!("/usr/local/kbf/releases/{commit}"));
    fs::create_dir_all(dir.join("units")).expect("dirs");
    fs::write(dir.join("kbf"), commit).expect("kbf");
    fs::write(dir.join("units/a.service"), "[Unit]\n").expect("unit");
    symlink(exec.at("/etc/escape"), dir.join("escape")).expect("link");
    let t = UNIX_EPOCH + Duration::from_secs(secs);
    fs::File::open(&dir)
        .expect("open")
        .set_modified(t)
        .expect("mtime");
}

fn releases_left(exec: &ScratchHost) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(exec.at("/usr/local/kbf/releases"))
        .expect("list")
        .map(|i| i.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// Catches a gc that removes a release still in use: the state's current, previous or
/// staged one, or (with a state that does not name them) the one a link points to;
/// that keeps the oldest instead of the newest under `--keep`; that follows a link out
/// of a release (here into `/etc/escape`); or that removes an entry of `releases/` that
/// is not a release.
///
/// Mutants seen red: gc without the state's current (`[state.previous, state.staged]`),
/// and without the state's previous (`[state.current, state.staged]`).
#[test]
fn gc_keeps_current_previous_staged_and_the_newest() {
    let (mut exec, root) = host("gc");
    fs::create_dir_all(exec.at("/etc/escape")).expect("escape");
    fs::write(exec.at("/etc/escape/keep"), "x").expect("escape");
    for (i, commit) in [A, B, C, D, E, S].into_iter().enumerate() {
        seed_release(&exec, commit, 1_000 + i as u64);
    }
    let releases = exec.at("/usr/local/kbf/releases");
    fs::write(releases.join("notes.txt"), "x").expect("stray");
    fs::create_dir(releases.join("tmp")).expect("stray");
    fs::write(releases.join(D.replace('d', "9")), "x").expect("a file named as a commit");
    let state = State {
        current: Some(c(A)),
        previous: Some(c(B)),
        staged: Some(c(S)),
        ..State::default()
    };
    fs::create_dir(exec.at("/var/lib/kbf-deploy")).expect("state dir");
    Locked::acquire(&root)
        .expect("lock")
        .write(&state)
        .expect("write");
    // No links: only the state names what is in use.
    let out = gc_on(&mut exec, &root, 1).expect("gc");
    assert!(
        out.contains(&format!("keeping: {B} {A} {S}; removing: {D} {C}\n")),
        "{out}"
    );
    assert!(out.contains("left alone (not a release): "), "{out}");
    let mut want = vec![A, B, E, S, "notes.txt", "tmp"]
        .into_iter()
        .map(String::from)
        .collect::<Vec<_>>();
    want.push(D.replace('d', "9"));
    want.sort();
    assert_eq!(releases_left(&exec), want);
    assert_eq!(
        fs::read_to_string(exec.at("/etc/escape/keep")).expect("kept"),
        "x"
    );

    // A state that names nothing: the links keep what they point to.
    Locked::acquire(&root)
        .expect("lock")
        .write(&State::default())
        .expect("write");
    symlink(format!("releases/{E}"), exec.at("/usr/local/kbf/current")).expect("link");
    symlink(format!("releases/{S}"), exec.at("/usr/local/kbf/previous")).expect("link");
    gc_on(&mut exec, &root, 0).expect("gc");
    let mut want = vec![E, S, "notes.txt", "tmp"]
        .into_iter()
        .map(String::from)
        .collect::<Vec<_>>();
    want.push(D.replace('d', "9"));
    want.sort();
    assert_eq!(releases_left(&exec), want);
}

/// Catches a verb that moves links under a pending commit-confirm deploy, which owns
/// them until it is confirmed or rolled back.
#[test]
fn every_verb_refuses_while_a_deploy_is_pending() {
    let (mut exec, root) = host("pending");
    let state = State {
        pending_confirm: Some(PendingConfirm {
            commit: c(C),
            previous: None,
            deadline: Some(1),
            order_id: None,
            starts_left: None,
        }),
        ..State::default()
    };
    fs::create_dir(exec.at("/var/lib/kbf-deploy")).expect("state dir");
    Locked::acquire(&root)
        .expect("lock")
        .write(&state)
        .expect("write");
    let want =
        format!("a commit-confirm deploy of {C} is pending: confirm it or let it roll back first");
    let from = source("pending", "A", None);
    let errs = [
        install_on(&mut exec, &root, A, &from, Mode::DryRun).expect_err("install"),
        rollback_on(&mut exec, &root, Mode::Apply).expect_err("rollback"),
        gc_on(&mut exec, &root, 0).expect_err("gc"),
    ];
    for err in errs {
        assert_eq!(err.to_string(), want);
    }
}

/// Catches errors passed over or misnamed: a source without `kbf`, with `units` a file,
/// with a unit whose name is not UTF-8 (it could not be named in a plan line), a state
/// file that cannot be parsed, a `current` that is a directory, and a layout directory
/// that cannot be read.
#[test]
fn errors_name_what_is_wrong() {
    let (mut exec, root) = host("errors");
    let empty = scratch("deploy-release-src-empty");
    let err = install_on(&mut exec, &root, A, &empty, Mode::DryRun).expect_err("no kbf");
    assert!(
        err.to_string()
            .starts_with(&format!("cannot read {}", empty.join("kbf").display()))
    );
    let units_file = source("errors-units", "A", None);
    fs::write(units_file.join("units"), "x").expect("seed");
    let err = install_on(&mut exec, &root, A, &units_file, Mode::DryRun).expect_err("units");
    assert!(err.to_string().starts_with(&format!(
        "cannot read {}",
        units_file.join("units").display()
    )));
    let odd = source("errors-odd", "A", Some(&[]));
    let name = OsStr::from_bytes(b"bad\xff.service");
    fs::write(odd.join("units").join(name), "x").expect("seed");
    let err = install_on(&mut exec, &root, A, &odd, Mode::DryRun).expect_err("not utf-8");
    assert_eq!(
        err.to_string(),
        format!("{:?} is not a UTF-8 name", odd.join("units").join(name))
    );

    let ok = source("errors-ok", "A", None);
    fs::create_dir_all(exec.at("/usr/local/kbf/current")).expect("a directory");
    let err = install_on(&mut exec, &root, A, &ok, Mode::DryRun).expect_err("not a link");
    assert_eq!(
        err.to_string(),
        "refusing to replace /usr/local/kbf/current with a symbolic link: it is not one"
    );
    fs::create_dir_all(exec.at("/var/lib/kbf-deploy")).expect("dir");
    fs::write(exec.at("/var/lib/kbf-deploy/state.json"), "{").expect("seed");
    let err = rollback_on(&mut exec, &root, Mode::DryRun).expect_err("broken state");
    assert!(err.to_string().contains("is not a deploy state"), "{err}");

    let (mut exec, root) = host("errors-unreadable");
    fs::write(exec.at("/usr/local/kbf"), "a file").expect("seed");
    let err = gc_on(&mut exec, &root, 0).expect_err("unreadable");
    assert!(
        err.to_string()
            .starts_with("cannot read /usr/local/kbf/current: "),
        "{err}"
    );
    fs::remove_file(exec.at("/usr/local/kbf")).expect("remove");
    fs::create_dir(exec.at("/usr/local/kbf")).expect("dir");
    fs::write(exec.at("/usr/local/kbf/releases"), "a file").expect("seed");
    let err = gc_on(&mut exec, &root, 0).expect_err("releases is a file");
    let want = format!(
        "cannot read {}: ",
        exec.at("/usr/local/kbf/releases").display()
    );
    assert!(err.to_string().starts_with(&want), "{err}");
}

/// A scratch Mac: the directories every macOS has above the layout (`/var/run`,
/// `/var/db`, `/usr`) and not `/usr/local`, which a fresh one lacks.
fn mac(name: &str) -> (ScratchHost, Layout) {
    let exec = ScratchHost::new(&format!("deploy-release-mac-{name}"));
    for sub in ["var/run", "var/db", "usr"] {
        fs::create_dir_all(exec.root.join(sub)).expect("seed");
    }
    let layout = Layout::new(
        Root::new(&exec.root).expect("absolute"),
        crate::layout::Platform::MacOs,
    );
    (exec, layout)
}

/// A release for a Mac: `kbf-daemon` holding `binary`.
fn mac_source(name: &str, binary: &str) -> PathBuf {
    let dir = scratch(&format!("deploy-release-src-{name}"));
    fs::write(dir.join("kbf-daemon"), binary).expect("binary");
    dir
}

/// Catches a release of two binaries not being copied whole: both come out 0755 with their
/// bytes, the first is named in the release line as a release of one always was, and the
/// second has a line of its own with its size and sha256.
#[test]
fn a_release_of_two_binaries_is_installed_whole_and_reported() {
    let (mut exec, root) = host("two-binaries");
    let from = source("two-binaries-src", "kbf A", None);
    fs::write(from.join("kbf-daemon"), "daemon A").expect("daemon");
    let out = install_on(&mut exec, &root, A, &from, Mode::Apply).expect("install");
    for (file, text) in [("kbf", "kbf A"), ("kbf-daemon", "daemon A")] {
        let path = format!("/usr/local/kbf/releases/{A}/{file}");
        assert_eq!(fs::read_to_string(exec.at(&path)).expect(file), text);
        assert_eq!(mode_of(&exec, &path), 0o755);
    }
    let r = format!("/usr/local/kbf/releases/{A}");
    assert!(
        out.starts_with(&format!(
            "release {A}: {r} (5 bytes, sha256 {}), 0 unit(s)\nalso kbf-daemon (8 bytes, sha256 {})\n",
            sha256(b"kbf A"),
            sha256(b"daemon A")
        )),
        "{out}"
    );

    // The release is whole or not there: a second binary the directory lacks is a
    // different release, and is refused.
    let (mut exec2, root2) = host("two-binaries-differs");
    let only_kbf = source("two-binaries-only", "kbf A", None);
    install_on(&mut exec2, &root2, A, &only_kbf, Mode::Apply).expect("install");
    let err = install_on(&mut exec2, &root2, A, &from, Mode::Apply).expect_err("a new binary");
    assert!(
        matches!(&err, ReleaseError::Differs { path } if path.as_str().ends_with("/kbf-daemon")),
        "{err}"
    );
}

/// Catches a source with only `kbf-daemon` being refused (a Mac release has no `kbf`), and
/// a source with neither being refused naming `kbf` as before.
#[test]
fn a_source_needs_one_binary_and_a_daemon_alone_will_do() {
    let only = mac_source("daemon-only", "daemon A");
    let s = Source::read(&only).expect("reads");
    assert_eq!(s.binaries, [("kbf-daemon", b"daemon A".to_vec())]);
    let none = scratch("deploy-release-src-none");
    let err = Source::read(&none).expect_err("nothing");
    assert!(
        err.to_string()
            .starts_with(&format!("cannot read {}", none.join("kbf").display())),
        "{err}"
    );
    // A binary that cannot be read is an error, not a missing one.
    let unreadable = scratch("deploy-release-src-unreadable");
    fs::create_dir(unreadable.join("kbf")).expect("a directory");
    let err = Source::read(&unreadable).expect_err("a directory");
    assert!(err.to_string().starts_with("cannot read "), "{err}");
}

/// Catches the macOS layout not being used: an install on a Mac puts the state under
/// `/var/db/kbf-deploy` and takes the lock at `/var/run/kbf-deploy.lock`, never at
/// `/var/lib` or `/run`; it makes the `/usr/local` a fresh Mac lacks (root's, 0755) and
/// the state directory; the links point at the release; the next-steps line names the
/// daemon and `launchctl`, not `kbf` and `systemctl`.
///
/// Mutant seen red: `Locked::acquire` taking the Linux lock (`/run` is not there).
#[test]
fn a_mac_install_uses_the_mac_layout_and_makes_its_missing_parents() {
    let (mut exec, layout) = mac("flow");
    let a = mac_source("mac-flow-a", "daemon A");
    let b = mac_source("mac-flow-b", "daemon B");
    assert!(!exec.at("/usr/local").exists());
    let out = install_on(&mut exec, &layout, A, &a, Mode::Apply).expect("install A");
    install_on(&mut exec, &layout, B, &b, Mode::Apply).expect("install B");
    assert!(
        out.contains("state: written to /var/db/kbf-deploy/state.json\n"),
        "{out}"
    );
    assert!(
        out.contains("restart the units that run /usr/local/kbf/current/kbf-daemon (launchctl kickstart -k system/<label>)"),
        "{out}"
    );
    assert!(exec.at("/var/db/kbf-deploy/state.json").is_file());
    assert!(exec.at("/var/run/kbf-deploy.lock").is_file());
    assert!(!exec.at("/var/lib").exists() && !exec.at("/run").exists());
    for dir in ["/usr/local", "/var/db/kbf-deploy", "/usr/local/kbf"] {
        assert_eq!(mode_of(&exec, dir), 0o755, "{dir}");
    }
    assert_eq!(link(&exec, "current"), Some(format!("releases/{B}")));
    assert_eq!(link(&exec, "previous"), Some(format!("releases/{A}")));
    assert_eq!(state_of(&layout).current, Some(c(B)));
    assert!(
        !exec
            .at(&format!("/usr/local/kbf/releases/{A}/kbf"))
            .exists()
    );

    // Rolling back finds the previous release by its daemon.
    let out = rollback_on(&mut exec, &layout, Mode::Apply).expect("rollback");
    assert!(
        out.contains(&format!("links: current {B} -> {A}, previous {A} -> none")),
        "{out}"
    );
    assert_eq!(link(&exec, "current"), Some(format!("releases/{A}")));
}

/// Catches `/usr/local` being touched when the host has one: a Mac (or any host) whose
/// `/usr/local` is 0775 keeps that mode (its owner is the host's business; the trust
/// check refuses what others can write), the plan has no step for it, and a host without
/// one gets `mkdir /usr/local` as the first step.
///
/// Mutant seen red: `/usr/local` always planned (the 0775 becomes 0755).
#[test]
fn usr_local_is_made_when_missing_and_left_alone_when_there() {
    let (mut exec, root) = host("usr-local");
    fs::set_permissions(exec.at("/usr/local"), fs::Permissions::from_mode(0o775)).expect("mode");
    let from = source("usr-local-src", "kbf A", None);
    let out = install_on(&mut exec, &root, A, &from, Mode::Apply).expect("install");
    assert!(!out.contains("/usr/local ("), "{out}");
    assert_eq!(mode_of(&exec, "/usr/local"), 0o775);

    let (mut exec, root) = host("usr-local-missing");
    fs::remove_dir(exec.at("/usr/local")).expect("remove");
    let out = install_on(&mut exec, &root, A, &from, Mode::DryRun).expect("dry run");
    let first_step = out
        .lines()
        .find(|l| l.starts_with("DRY-RUN"))
        .expect("a step");
    let owner = me();
    assert_eq!(
        first_step,
        format!(
            "DRY-RUN: mkdir /usr/local (0755, owner {}:{})",
            owner.uid, owner.gid
        )
    );
    install_on(&mut exec, &root, A, &from, Mode::Apply).expect("install");
    assert_eq!(mode_of(&exec, "/usr/local"), 0o755);
}

/// Catches `stage` doing what `install` does: it must copy and check the release, record
/// it as the state's `staged` one (and change nothing else of the state), and move no link.
/// It proceeds while a deploy is pending, keeping that deploy's record; a dry run prints
/// the plan and changes nothing; and `gc` keeps the staged release.
///
/// Mutants seen red: `stage` switching the links; `stage` refusing while pending; the
/// staged commit not recorded (`gc` then removes the release).
#[test]
fn stage_copies_and_records_the_release_and_moves_no_link() {
    let (mut exec, root) = host("stage");
    let a = source("stage-a", "kbf A", Some(UNITS));
    let b = source("stage-b", "kbf B", None);
    install_on(&mut exec, &root, A, &a, Mode::Apply).expect("install A");
    Locked::acquire(&root)
        .expect("lock")
        .write(&State {
            pending_confirm: Some(PendingConfirm {
                commit: c(A),
                previous: None,
                deadline: Some(1),
                order_id: None,
                starts_left: None,
            }),
            ..state_of(&root)
        })
        .expect("pending");

    let before = tree(&exec.root);
    let mut out = Vec::new();
    stage(
        &mut exec,
        &root,
        me(),
        &Install {
            commit: c(B),
            from: b.clone(),
        },
        Mode::DryRun,
        &mut out,
    )
    .expect("dry run");
    assert_eq!(tree(&exec.root), before);
    assert!(
        String::from_utf8(out)
            .expect("utf-8")
            .contains(&format!("release {B}:"))
    );

    let mut out = Vec::new();
    stage(
        &mut exec,
        &root,
        me(),
        &Install {
            commit: c(B),
            from: b,
        },
        Mode::Apply,
        &mut out,
    )
    .expect("stage while pending");
    let out = String::from_utf8(out).expect("utf-8");
    assert!(out.ends_with(&format!("state: {B} staged\n")), "{out}");
    assert_eq!(
        fs::read_to_string(exec.at(&format!("/usr/local/kbf/releases/{B}/kbf"))).expect("copied"),
        "kbf B"
    );
    let s = state_of(&root);
    assert_eq!((s.current, s.staged), (Some(c(A)), Some(c(B))));
    assert_eq!(s.pending_confirm.map(|p| p.commit), Some(c(A)));
    assert_eq!(link(&exec, "current"), Some(format!("releases/{A}")));
    assert_eq!(link(&exec, "previous"), None);

    // The staged release survives a gc that removes everything else.
    Locked::acquire(&root)
        .expect("lock")
        .write(&State {
            pending_confirm: None,
            ..state_of(&root)
        })
        .expect("clear");
    gc_on(&mut exec, &root, 0).expect("gc");
    assert!(
        exec.at(&format!("/usr/local/kbf/releases/{B}/kbf"))
            .exists()
    );
}

/// Catches a stage that replaces a release or leaves a half copy: a release directory with
/// other bytes is refused and keeps its bytes, and a source with nothing to copy is
/// refused before the lock.
#[test]
fn stage_never_replaces_a_release() {
    let (mut exec, root) = host("stage-differs");
    let a = source("stage-differs-a", "kbf A", None);
    let other = source("stage-differs-b", "kbf other", None);
    let install = |from: &Path| Install {
        commit: c(A),
        from: from.to_owned(),
    };
    stage(
        &mut exec,
        &root,
        me(),
        &install(&a),
        Mode::Apply,
        &mut Vec::new(),
    )
    .expect("stage");
    let err = stage(
        &mut exec,
        &root,
        me(),
        &install(&other),
        Mode::Apply,
        &mut Vec::new(),
    )
    .expect_err("other bytes");
    assert!(matches!(err, ReleaseError::Differs { .. }), "{err}");
    assert_eq!(
        fs::read_to_string(exec.at(&format!("/usr/local/kbf/releases/{A}/kbf"))).expect("kept"),
        "kbf A"
    );
    let empty = scratch("deploy-release-src-stage-empty");
    let err = stage(
        &mut exec,
        &root,
        me(),
        &install(&empty),
        Mode::Apply,
        &mut Vec::new(),
    )
    .expect_err("no binary");
    assert!(err.to_string().starts_with("cannot read "), "{err}");
}

/// A host whose disk fills: it applies `left` file writes, and the next one fails with
/// ENOSPC. Everything else is the scratch host's.
struct DiskFull<'a> {
    host: &'a mut ScratchHost,
    left: usize,
}

impl Exec for DiskFull<'_> {
    fn euid(&self) -> u32 {
        self.host.euid()
    }

    fn inspect(&self, path: &HostPath) -> std::io::Result<Option<Entry>> {
        self.host.inspect(path)
    }

    fn stat(&self, path: &HostPath) -> std::io::Result<Option<Meta>> {
        self.host.stat(path)
    }

    fn query(&self, cmd: &Cmd) -> std::io::Result<Output> {
        self.host.query(cmd)
    }

    fn http_status(&self, url: &HttpUrl) -> std::io::Result<u16> {
        self.host.http_status(url)
    }

    fn apply(&mut self, step: &Step) -> std::io::Result<()> {
        if matches!(step, Step::WriteFile { .. }) {
            if self.left == 0 {
                return Err(std::io::Error::from_raw_os_error(28));
            }
            self.left -= 1;
        }
        self.host.apply(step)
    }

    fn check_binary(&self) -> Result<(), Untrusted> {
        self.host.check_binary()
    }
}

/// Catches a stage that fills the disk and leaves a half-written release directory:
/// the retry would then find the directory there with other content and refuse it for
/// ever, and `gc` would keep a broken release. After the failure nothing of the release
/// is there and the state is unchanged; the retry on a healthy disk succeeds. A release
/// that was already installed whole is never removed by a failed repeat of its stage.
///
/// Mutants seen red: the partial directory left in place; the directory removed whether
/// or not the copy made it (the installed release goes with a failed repeat).
#[test]
fn a_stage_that_fills_the_disk_leaves_no_partial_release_and_can_be_retried() {
    let (mut exec, root) = host("disk-full");
    let from = source("disk-full", "kbf B", Some(UNITS));
    fs::write(from.join("kbf-daemon"), "daemon B").expect("a second binary");
    let dir = exec.at(&format!("/usr/local/kbf/releases/{B}"));
    let want = Install {
        commit: c(B),
        from: from.clone(),
    };

    // The first binary is written, the second is not.
    let err = stage(
        &mut DiskFull {
            host: &mut exec,
            left: 1,
        },
        &root,
        me(),
        &want,
        Mode::Apply,
        &mut Vec::new(),
    )
    .expect_err("the disk is full");
    assert!(err.to_string().contains("os error 28"), "{err}");
    assert!(!dir.exists(), "{:?}", tree(&exec.root));
    assert!(
        state::read_unlocked(&root).expect("read").is_none() || state_of(&root).staged.is_none()
    );

    // The retry, on a disk with room (the same host type, so that one copy of the code
    // under test runs on both its paths).
    stage(
        &mut DiskFull {
            host: &mut exec,
            left: 100,
        },
        &root,
        me(),
        &want,
        Mode::Apply,
        &mut Vec::new(),
    )
    .expect("retry");
    assert_eq!(fs::read_to_string(dir.join("kbf")).expect("kbf"), "kbf B");
    assert_eq!(
        fs::read_to_string(dir.join("kbf-daemon")).expect("daemon"),
        "daemon B"
    );
    assert_eq!(state_of(&root).staged, Some(c(B)));

    // A repeat of a stage that was whole, failing on a full disk, removes nothing. (The
    // binary's mode is changed so that the repeat has a file to write again.)
    fs::set_permissions(dir.join("kbf"), fs::Permissions::from_mode(0o600)).expect("chmod");
    stage(
        &mut DiskFull {
            host: &mut exec,
            left: 0,
        },
        &root,
        me(),
        &want,
        Mode::Apply,
        &mut Vec::new(),
    )
    .expect_err("the disk is full again");
    assert_eq!(fs::read_to_string(dir.join("kbf")).expect("kbf"), "kbf B");
    assert!(dir.join("units").join("kbf.slice").exists());
}
