use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::rc::Rc;

use super::*;
use crate::linux::host::SYSTEMCTL;
use kbf_host::plan::{Mode as PlanMode, execute};
use kbf_host::step::Step;
use kbf_host::testkit::{ScratchHost, reply};
use kbf_host::user::{GETENT, ID};

const EXAMPLE: &str = include_str!("../../templates/linux/config.example");

fn me() -> (u32, u32) {
    (
        rustix::process::getuid().as_raw(),
        rustix::process::getgid().as_raw(),
    )
}

fn runner() -> Owner {
    let (uid, gid) = me();
    Owner { uid, gid }
}

/// What the fake host's tools see.
struct World {
    users: BTreeMap<String, (u32, u32)>,
    enabled: BTreeSet<String>,
    active: BTreeSet<String>,
    /// What `kbf check-args` exits with.
    check_args: i32,
}

fn install(mode: Mode, bin_dir: Option<&str>) -> InstallMode {
    InstallMode {
        config: LinuxConfig::parse(EXAMPLE).expect("valid settings"),
        mode,
        bin_dir: bin_dir.map(|d| HostPath::new(d).expect("path")),
    }
}

fn write(exec: &ScratchHost, path: &str, text: &str, mode: u32) {
    let full = exec.at(path);
    fs::create_dir_all(full.parent().expect("parent")).expect("dirs");
    let _ = fs::remove_file(&full);
    fs::write(&full, text).expect("write");
    fs::set_permissions(&full, fs::Permissions::from_mode(mode)).expect("chmod");
}

/// A scratch host that passes every check of every built mode: both users are the
/// test's own uid, kbf is installed, every file a unit reads is there (keys 0600).
fn host(name: &str) -> (ScratchHost, Rc<RefCell<World>>) {
    let mut exec = ScratchHost::new(name);
    let (uid, gid) = me();
    let world = Rc::new(RefCell::new(World {
        users: BTreeMap::from([
            ("kbf".to_owned(), (uid, gid)),
            ("kbf-server".to_owned(), (uid, gid)),
        ]),
        enabled: BTreeSet::new(),
        active: BTreeSet::new(),
        check_args: 0,
    }));
    let w = Rc::clone(&world);
    exec.answer = Box::new(move |cmd| {
        let mut w = w.borrow_mut();
        let args: Vec<&str> = cmd.args.iter().map(String::as_str).collect();
        Ok(match (cmd.program.as_str(), &args[..]) {
            (GETENT, ["passwd", "--", name]) => match w.users.get(*name) {
                Some((u, g)) => reply(
                    0,
                    &format!("{name}:x:{u}:{g}::/srv/{name}:/bin/false\n"),
                    "",
                ),
                None => reply(2, "", ""),
            },
            (ID, ["-G", "--", name]) => reply(0, &format!("{}\n", w.users[*name].1), ""),
            (SYSTEMCTL, ["is-active", "--quiet", u]) => {
                reply(i32::from(!w.active.contains(*u)), "", "")
            }
            (SYSTEMCTL, ["is-enabled", "--quiet", u]) => {
                reply(i32::from(!w.enabled.contains(*u)), "", "")
            }
            (SYSTEMCTL, ["enable", u]) => {
                w.enabled.insert((*u).to_owned());
                reply(0, "", "")
            }
            (SYSTEMCTL, ["restart" | "start", u]) => {
                w.active.insert((*u).to_owned());
                reply(0, "", "")
            }
            (_, ["check-args", ..]) => reply(w.check_args, "", ""),
            _ => reply(0, "", ""),
        })
    });
    write(&exec, "/etc/subuid", "kbf:100000:65536\n", 0o644);
    write(&exec, "/etc/subgid", "kbf:100000:65536\n", 0o644);
    write(&exec, "/var/lib/systemd/linger/kbf", "", 0o644);
    write(
        &exec,
        "/etc/kbf/s3.env",
        "AWS_ACCESS_KEY_ID=k\nAWS_SECRET_ACCESS_KEY=s\n",
        0o600,
    );
    for key in [
        "storage.key",
        "api-token",
        "reapi.key",
        "worker.key",
        "node.key",
    ] {
        write(&exec, &format!("/etc/kbf/{key}"), "secret", 0o600);
    }
    for public in [
        "ca.pem",
        "storage.pem",
        "reapi.pem",
        "worker.pem",
        "node.pem",
        "deny-list",
    ] {
        write(&exec, &format!("/etc/kbf/{public}"), "pem", 0o644);
    }
    write(&exec, "/usr/local/bin/kbf", "kbf-1", 0o755);
    (exec, world)
}

fn plan_here(exec: &ScratchHost, a: &InstallMode) -> Result<(Plan, String), HostError> {
    let mut out = Vec::new();
    let p = plan_with(exec, a, runner(), &mut out)?;
    Ok((p, String::from_utf8(out).expect("utf-8")))
}

fn steps(p: &Plan) -> Vec<String> {
    p.steps().iter().map(ToString::to_string).collect()
}

/// The steps after the unit files: the directories and the service steps.
fn tail(p: &Plan) -> Vec<String> {
    let all = steps(p);
    let at = all
        .iter()
        .rposition(|s| s.starts_with("write /etc/systemd/system/"))
        .map_or(0, |i| i + 1);
    all[at..].to_vec()
}

fn go(exec: &mut ScratchHost, a: &InstallMode, mode: PlanMode) {
    let (p, _) = plan_here(exec, a).expect("plan");
    execute(&p, RunAs::Root, mode, exec, &mut Vec::new()).expect("install-units --mode");
}

/// The storage node on a fresh host: the slice and its unit (the bytes
/// `render-units --mode storage` makes), its metadata and hot store directories its
/// user's alone, then reload, enable and restart; no `/readyz` wait (it has no operator
/// API). A dry run changes nothing; after the apply a re-run plans nothing. Catches a
/// unit written with other bytes, a directory left for kbf to make as the wrong user or
/// mode, the unit started by a dry run, and a restart on every run.
#[test]
fn the_storage_unit_is_installed_and_a_rerun_plans_nothing() {
    let (mut exec, world) = host("install-mode-storage");
    let (uid, gid) = me();
    let a = install(Mode::Storage, None);
    let (p, _) = plan_here(&exec, &a).expect("plan");
    let unit = modes::render(&a.config, None, Mode::Storage).expect("renders");
    let units: Vec<&Step> = p
        .steps()
        .iter()
        .filter(|s| matches!(s, Step::WriteFile { .. }))
        .collect();
    assert_eq!(units.len(), 2, "{:#?}", steps(&p));
    assert!(matches!(units[1], Step::WriteFile { path, bytes, .. }
        if path.as_str() == "/etc/systemd/system/kbf-storage.service" && *bytes == unit.as_bytes()));
    assert_eq!(
        tail(&p),
        [
            "mkdir /var/lib/kbf-server (0755)".to_owned(),
            format!("mkdir /var/lib/kbf-server/raft (0700, owner {uid}:{gid})"),
            format!("mkdir /var/lib/kbf-server/hot (0700, owner {uid}:{gid})"),
            "run /usr/bin/systemctl daemon-reload".to_owned(),
            "run /usr/bin/systemctl enable kbf-storage.service".to_owned(),
            "run /usr/bin/systemctl restart kbf-storage.service".to_owned(),
        ]
    );

    go(&mut exec, &a, PlanMode::DryRun);
    assert!(!exec.at("/etc/systemd/system/kbf-storage.service").exists());
    assert!(world.borrow().active.is_empty(), "a dry run started it");

    go(&mut exec, &a, PlanMode::Apply);
    assert_eq!(
        fs::read_to_string(exec.at("/etc/systemd/system/kbf-storage.service")).expect("unit"),
        unit
    );
    assert!(world.borrow().active.contains("kbf-storage.service"));
    let (p, out) = plan_here(&exec, &a).expect("plan");
    assert_eq!(steps(&p), Vec::<String>::new());
    assert!(
        out.contains("unit and binary unchanged: nothing restarted"),
        "{out}"
    );

    // Stopped and disabled since: enabled and started again, nothing written.
    world.borrow_mut().active.clear();
    world.borrow_mut().enabled.clear();
    let (p, _) = plan_here(&exec, &a).expect("plan");
    assert_eq!(
        steps(&p),
        [
            "run /usr/bin/systemctl enable kbf-storage.service",
            "run /usr/bin/systemctl start kbf-storage.service",
        ]
    );
}

/// [`run`] as the verb runs it, root the owner: a dry run prints each step as a
/// `DRY-RUN:` line, the unit's file among them, and changes nothing.
#[test]
fn a_dry_run_prints_the_plan_and_changes_nothing() {
    let (mut exec, world) = host("install-mode-dry-run");
    let mut out = Vec::new();
    run(
        &mut exec,
        &install(Mode::Daemon, None),
        PlanMode::DryRun,
        &mut out,
    )
    .expect("dry run");
    let out = String::from_utf8(out).expect("utf-8");
    assert!(
        out.contains("DRY-RUN: write /etc/systemd/system/kbf-daemon.service (0644, owner 0:0, "),
        "{out}"
    );
    assert!(!exec.at("/etc/systemd/system/kbf-daemon.service").exists());
    assert!(world.borrow().active.is_empty());
}

/// Catches a unit run as root (no `User=`, an empty one, `root` or uid 0, or one
/// outside `[Service]`) installed: kbf never runs as root.
#[test]
fn a_unit_without_a_user_of_its_own_is_refused() {
    for text in [
        "[Service]\nExecStart=/usr/local/bin/kbf storage\n",
        "[Service]\nUser=\n",
        "[Service]\nUser=root\n",
        "[Service]\nUser=0\n",
        "[Unit]\nUser=kbf\n",
    ] {
        let unit = UnitFile::parse(text).expect("unit");
        let e = unit_user(&unit, "kbf-storage.service").expect_err(text);
        assert!(
            matches!(&e, HostError::RunsAsRoot(n) if n == "kbf-storage.service"),
            "{e}"
        );
    }
    let unit = UnitFile::parse("[Service]\nUser=kbf-server\n").expect("unit");
    assert_eq!(unit_user(&unit, "u").expect("a user"), "kbf-server");
}

/// all-in-one: its data directory before the cold store under it, and the wait for its
/// operator API's `/readyz` after the start; with `--bin-dir` the new kbf first, the
/// old one kept. Catches the cold store's directory planned before its parent's owner
/// and mode, a start not waited for, a new kbf not installed, and a unit installed
/// without the stop settings that keep its drain whole.
#[test]
fn the_all_in_one_unit_waits_for_readyz_and_installs_a_new_kbf() {
    let (exec, _) = host("install-mode-aio");
    write(&exec, "/opt/new/kbf", "kbf-2", 0o755);
    let (uid, gid) = me();
    let (p, _) = plan_here(&exec, &install(Mode::AllInOne, Some("/opt/new"))).expect("plan");
    let all = steps(&p);
    assert_eq!(
        all[..2],
        [
            format!("write /usr/local/bin/kbf.previous (0755, owner {uid}:{gid}, 5 bytes)"),
            format!("write /usr/local/bin/kbf (0755, owner {uid}:{gid}, 5 bytes)"),
        ]
    );
    assert_eq!(
        tail(&p),
        [
            format!("mkdir /var/lib/kbf (0755, owner {uid}:{gid})"),
            "mkdir /var/lib/kbf-server (0755)".to_owned(),
            format!("mkdir /var/lib/kbf-server/raft (0700, owner {uid}:{gid})"),
            format!("mkdir /var/lib/kbf/cold (0700, owner {uid}:{gid})"),
            "run /usr/bin/systemctl daemon-reload".to_owned(),
            "run /usr/bin/systemctl enable kbf-all-in-one.service".to_owned(),
            "run /usr/bin/systemctl restart kbf-all-in-one.service".to_owned(),
            "wait up to 60s for http://127.0.0.1:8982/readyz".to_owned(),
        ]
    );
    // The unit installed is the template's: stopped with SIGTERM, never killed mid-drain.
    let unit = p
        .steps()
        .iter()
        .find_map(|s| match s {
            Step::WriteFile { path, bytes, .. }
                if path.as_str() == "/etc/systemd/system/kbf-all-in-one.service" =>
            {
                Some(String::from_utf8_lossy(bytes).into_owned())
            }
            _ => None,
        })
        .expect("the unit is written");
    assert!(
        unit.contains("\nKillSignal=SIGTERM\nTimeoutStopSec=infinity\n"),
        "{unit}"
    );
}

/// The refusals, each before any step: a key file others can read or another user owns, a file the unit reads missing, a
/// unit kbf's own parser refuses (rule `parse`), a kbf that is not there, a build node
/// whose user does not linger, and a storage node with no S3 endpoint set.
/// Catches each of these installed anyway.
#[test]
fn an_install_is_refused_before_any_step() {
    let refused = |name: &str, mode: Mode, edit: &dyn Fn(&ScratchHost, &RefCell<World>)| {
        let (exec, world) = host(&format!("install-mode-{name}"));
        edit(&exec, &world);
        match plan_here(&exec, &install(mode, None)) {
            Ok((p, _)) => panic!("{name}: planned {:#?}", steps(&p)),
            Err(e) => e,
        }
    };
    let e = refused("readable-key", Mode::Storage, &|exec, _| {
        write(exec, "/etc/kbf/storage.key", "secret", 0o640);
    });
    assert!(
        matches!(&e, HostError::Readable { path, .. } if path.as_str() == "/etc/kbf/storage.key"),
        "{e}"
    );
    let e = refused("no-cert", Mode::Storage, &|exec, _| {
        fs::remove_file(exec.at("/etc/kbf/storage.pem")).expect("rm");
    });
    assert!(
        matches!(&e, HostError::Missing(path) if path.as_str() == "/etc/kbf/storage.pem"),
        "{e}"
    );
    // The daemon's bare --cert= and --key= are files too.
    let e = refused("no-node-cert", Mode::Daemon, &|exec, _| {
        fs::remove_file(exec.at("/etc/kbf/node.pem")).expect("rm");
    });
    assert!(
        matches!(&e, HostError::Missing(path) if path.as_str() == "/etc/kbf/node.pem"),
        "{e}"
    );
    let e = refused("node-key-readable", Mode::Daemon, &|exec, _| {
        write(exec, "/etc/kbf/node.key", "secret", 0o604);
    });
    assert!(
        matches!(&e, HostError::Readable { path, .. } if path.as_str() == "/etc/kbf/node.key"),
        "{e}"
    );
    let e = refused("no-s3-env", Mode::Storage, &|exec, _| {
        fs::remove_file(exec.at("/etc/kbf/s3.env")).expect("rm");
    });
    assert!(
        matches!(&e, HostError::Missing(path) if path.as_str() == "/etc/kbf/s3.env"),
        "{e}"
    );
    // The all-in-one runs as kbf; its keys belong to someone else here.
    let e = refused("not-owned", Mode::AllInOne, &|_, w| {
        let (uid, gid) = me();
        w.borrow_mut()
            .users
            .insert("kbf".to_owned(), (uid + 1, gid));
    });
    assert!(matches!(&e, HostError::NotOwned { .. }), "{e}");
    let e = refused("parse", Mode::Daemon, &|_, w| w.borrow_mut().check_args = 2);
    assert!(
        matches!(&e, HostError::Units(f) if f.iter().any(|f| f.to_string().starts_with("FAIL parse:"))),
        "{e}"
    );
    let e = refused("no-kbf", Mode::Storage, &|exec, _| {
        fs::remove_file(exec.at("/usr/local/bin/kbf")).expect("rm");
    });
    assert!(matches!(&e, HostError::NotExecutable(_)), "{e}");
    let e = refused("kbf-not-executable", Mode::Storage, &|exec, _| {
        write(exec, "/usr/local/bin/kbf", "kbf-1", 0o644);
    });
    assert!(matches!(&e, HostError::NotExecutable(_)), "{e}");
    let e = refused("no-linger", Mode::Daemon, &|exec, _| {
        fs::remove_file(exec.at("/var/lib/systemd/linger/kbf")).expect("rm");
    });
    assert!(matches!(&e, HostError::NoLinger { .. }), "{e}");
    // A storage node whose settings name no S3 endpoint: its unit is not rendered.
    let (exec, _) = host("install-mode-no-endpoint");
    let mut a = install(Mode::Storage, None);
    a.config.minio_s3_endpoint = None;
    let e = plan_here(&exec, &a).map(|_| ()).expect_err("refused");
    assert!(
        e.to_string()
            .starts_with("MINIO_S3_ENDPOINT must be set for --mode storage"),
        "{e}"
    );
}

/// The files a unit reads, read as systemd reads them: an `EnvironmentFile=` with a
/// leading `-` is optional and an empty one resets the list (neither is looked for),
/// and a directory where a file is named is no file. Catches an optional file
/// required, and a directory taken for the file.
#[test]
fn optional_environment_files_are_not_required_and_a_directory_is_no_file() {
    let (exec, _) = host("install-mode-files");
    let (uid, gid) = me();
    let user = RunUser {
        name: "kbf".to_owned(),
        uid,
        gid,
        groups: vec![gid],
        home: "/srv/kbf".to_owned(),
    };
    let unit = UnitFile::parse(
        "[Service]\nEnvironmentFile=\nEnvironmentFile=-/etc/kbf/optional.env\nExecStart=/usr/local/bin/kbf storage --storage-tls-cert=/etc/kbf/storage.pem\n",
    )
    .expect("unit");
    files_read(&exec, &unit, &user).expect("the optional file is not looked for");
    fs::create_dir_all(exec.at("/etc/kbf/dir.env")).expect("dir");
    let unit = UnitFile::parse("[Service]\nEnvironmentFile=/etc/kbf/dir.env\n").expect("unit");
    let e = files_read(&exec, &unit, &user).expect_err("a directory");
    assert!(
        matches!(&e, HostError::Missing(p) if p.as_str() == "/etc/kbf/dir.env"),
        "{e}"
    );
}

/// The frontend on a fresh host: its unit, then the SQLite state store's directory made
/// the server user's alone before the unit starts, and the wait for `/readyz` on the
/// operator API it now serves (it was refused as not built). Catches the state directory
/// left for kbf to make (it runs as a user that cannot write under `/var/lib`), a
/// frontend unit started with no readiness wait, and the frontend refused again.
#[test]
fn the_frontend_unit_is_installed_with_its_state_directory() {
    let (exec, _) = host("install-mode-frontend");
    let (uid, gid) = me();
    let a = install(Mode::Frontend, None);
    let (p, _) = plan_here(&exec, &a).expect("plan");
    assert_eq!(
        tail(&p),
        [
            "mkdir /var/lib/kbf-server (0755)".to_owned(),
            format!("mkdir /var/lib/kbf-server/state (0700, owner {uid}:{gid})"),
            "run /usr/bin/systemctl daemon-reload".to_owned(),
            "run /usr/bin/systemctl enable kbf-frontend@.service".to_owned(),
            "run /usr/bin/systemctl start kbf-frontend@a.service".to_owned(),
            "wait up to 60s for http://127.0.0.1:8982/readyz".to_owned(),
        ]
    );
}

/// The frontend's unit is a template with two instances, and a serving one is never
/// restarted: a restart drains it and every new build is refused until the next start.
/// Catches an update that restarts (or stops) the serving instance instead of leaving the
/// handover to `kbf frontend handover`, an install that starts the second instance beside
/// the first (two frontends would ask each other for the listeners), a template enabled
/// under a name systemd refuses, and a host with neither instance up left without one.
#[test]
fn a_serving_frontend_instance_is_not_restarted_by_an_update() {
    let (mut exec, world) = host("install-mode-frontend-serving");
    let a = install(Mode::Frontend, None);
    go(&mut exec, &a, PlanMode::Apply);
    assert!(
        exec.at("/etc/systemd/system/kbf-frontend@.service")
            .exists(),
        "the template is written under its own name"
    );
    assert!(world.borrow().active.contains("kbf-frontend@a.service"));
    assert!(!world.borrow().active.contains("kbf-frontend@b.service"));

    // The second instance serves; the unit file differs from the host's.
    {
        let mut w = world.borrow_mut();
        w.active.clear();
        w.active.insert("kbf-frontend@b.service".to_owned());
        w.enabled.insert("kbf-frontend@a.service".to_owned());
    }
    write(
        &exec,
        "/etc/systemd/system/kbf-frontend@.service",
        "[Service]\n",
        0o644,
    );
    let (p, out) = plan_here(&exec, &a).expect("plan");
    let got = steps(&p);
    assert!(
        got.contains(&"run /usr/bin/systemctl daemon-reload".to_owned())
            && got.contains(&"run /usr/bin/systemctl enable kbf-frontend@.service".to_owned()),
        "{got:?}"
    );
    assert!(
        !got.iter()
            .any(|s| s.contains("restart") || s.contains(" start ") || s.contains("stop")),
        "{got:?}"
    );
    assert!(
        !got.iter().any(|s| s.starts_with("wait")),
        "nothing was started, so there is no /readyz to wait for: {got:?}"
    );
    assert!(
        out.contains(
            "kbf-frontend@b.service is serving and is not restarted: run `kbf frontend handover`"
        ),
        "{out}"
    );

    // Unchanged, enabled, and one instance up: nothing at all.
    go(&mut exec, &a, PlanMode::Apply);
    let (p, _) = plan_here(&exec, &a).expect("plan");
    assert_eq!(steps(&p), Vec::<String>::new());
}

/// The operator token of the frontend and the all-in-one: the host check reads the file
/// `KBF_API_TOKEN_FILE` names (the default, or another path), refuses it missing, readable
/// by group or other, or another user's, and install-units never makes it. Catches a unit
/// installed whose `--api-token-file` is absent (every write answers 401), a token the
/// check looks for at the default path after the key moved, and a token file created.
#[test]
fn the_operator_token_file_is_checked_where_the_key_names_it() {
    for mode in [Mode::Frontend, Mode::AllInOne] {
        let try_with = |name: &str, key: &str, edit: &dyn Fn(&ScratchHost, &RefCell<World>)| {
            let (exec, world) = host(&format!("install-mode-token-{name}"));
            edit(&exec, &world);
            let mut a = install(mode, None);
            let text = EXAMPLE.replace("KBF_API_TOKEN_FILE=\n", key);
            a.config = LinuxConfig::parse(&text).expect("valid settings");
            plan_here(&exec, &a).map(|_| ())
        };
        let e = try_with("missing", "KBF_API_TOKEN_FILE=\n", &|exec, _| {
            fs::remove_file(exec.at("/etc/kbf/api-token")).expect("rm");
        })
        .expect_err("no token");
        assert!(
            matches!(&e, HostError::Missing(p) if p.as_str() == "/etc/kbf/api-token"),
            "{mode}: {e}"
        );
        let e = try_with("group", "KBF_API_TOKEN_FILE=\n", &|exec, _| {
            write(exec, "/etc/kbf/api-token", "t", 0o640);
        })
        .expect_err("group-readable");
        assert!(
            matches!(&e, HostError::Readable { path, .. } if path.as_str() == "/etc/kbf/api-token"),
            "{mode}: {e}"
        );
        write_ok(&try_with("0400", "KBF_API_TOKEN_FILE=\n", &|exec, _| {
            write(exec, "/etc/kbf/api-token", "t", 0o400);
        }));
        // Another path: that file is the one looked for, and the default one is not.
        let moved = "KBF_API_TOKEN_FILE=/etc/kbf/operator.token\n";
        let e = try_with("moved-missing", moved, &|_, _| {}).expect_err("no token there");
        assert!(
            matches!(&e, HostError::Missing(p) if p.as_str() == "/etc/kbf/operator.token"),
            "{mode}: {e}"
        );
        write_ok(&try_with("moved", moved, &|exec, _| {
            fs::remove_file(exec.at("/etc/kbf/api-token")).expect("rm");
            write(exec, "/etc/kbf/operator.token", "t", 0o600);
        }));
        let e = try_with("moved-readable", moved, &|exec, _| {
            write(exec, "/etc/kbf/operator.token", "t", 0o604);
        })
        .expect_err("other-readable");
        assert!(
            matches!(&e, HostError::Readable { path, .. } if path.as_str() == "/etc/kbf/operator.token"),
            "{mode}: {e}"
        );
    }
    // Another user's token is refused; the plan never writes one.
    let (exec, world) = host("install-mode-token-owned");
    let (uid, gid) = me();
    world
        .borrow_mut()
        .users
        .insert("kbf-server".to_owned(), (uid + 1, gid));
    let e = plan_here(&exec, &install(Mode::Frontend, None))
        .map(|_| ())
        .expect_err("not the server user's");
    assert!(matches!(&e, HostError::NotOwned { .. }), "{e}");
    let (exec, _) = host("install-mode-token-never-made");
    let (p, _) = plan_here(&exec, &install(Mode::Frontend, None)).expect("plan");
    assert!(
        !steps(&p).iter().any(|s| s.contains("api-token")),
        "{:#?}",
        steps(&p)
    );
}

fn write_ok(r: &Result<(), HostError>) {
    assert!(r.is_ok(), "{r:?}");
}

/// The directories each unit's flags make, from the rendered units: the storage-and-daemon
/// unit's metadata log and hot store beside its data directory, the frontend's state
/// directory (the parent of the SQLite file, from the flag), the all-in-one's none
/// beyond its own. Catches a flag whose directory is left to kbf, which makes it as a
/// user that may not, and a state directory taken from the wrong flag.
#[test]
fn the_directories_come_from_the_flags_the_units_carry() {
    let dirs = |mode: Mode| {
        let c = LinuxConfig::parse(EXAMPLE).expect("valid settings");
        let text = modes::render(&c, Some(1500), mode).expect("renders");
        directories(&UnitFile::parse(&text).expect("a unit"))
            .expect("directories")
            .into_iter()
            .map(|(p, bits)| format!("{p} {bits:o}"))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        dirs(Mode::StorageAndDaemon),
        [
            "/var/lib/kbf 755",
            "/var/lib/kbf-server/raft 700",
            "/var/lib/kbf-server/hot 700",
        ]
    );
    assert_eq!(dirs(Mode::Frontend), ["/var/lib/kbf-server/state 700"]);
    assert_eq!(
        dirs(Mode::Storage),
        [
            "/var/lib/kbf-server/raft 700",
            "/var/lib/kbf-server/hot 700"
        ]
    );
}
