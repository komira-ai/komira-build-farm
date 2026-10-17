//! Unit tests of the OCI backend's start-up checks, helper jobs and image store sweep:
//! what needs no `crun` (`/usr/bin/true` and `/usr/bin/false` stand in where a program
//! only has to exit 0 or 1). What a lease does with a runtime is `tests/fake_oci.rs`'s.

use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;

use serde_json::json;

use super::*;
use crate::subids::IdFiles;

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::current_exe()
        .expect("test binary")
        .parent()
        .expect("deps directory")
        .join("kbf-driver-container-unit")
        .join(format!("oci-{name}-{}", std::process::id()));
    let _ = crate::remove::remove_tree(&dir);
    std::fs::create_dir_all(&dir).expect("scratch");
    dir
}

/// A configuration whose runtime exits `ok` (0 or 1) whatever it is asked, with ids in
/// files under `dir`.
fn config(dir: &Path, ok: bool) -> OciConfig {
    let files = IdFiles::in_dir(&dir.join("ids"));
    std::fs::create_dir_all(dir.join("ids")).expect("ids");
    std::fs::write(&files.passwd, "root:x:0:0::/:/bin/sh\n").expect("passwd");
    let uid = rustix::process::getuid().as_raw();
    std::fs::write(&files.subuid, format!("{uid}:100000:65536\n")).expect("subuid");
    std::fs::write(&files.subgid, format!("{uid}:200000:65536\n")).expect("subgid");
    OciConfig {
        runtime: PathBuf::from(if ok {
            "/usr/bin/true"
        } else {
            "/usr/bin/false"
        }),
        daemon: std::env::current_exe().expect("test binary"),
        id_files: files,
        check_search: false,
        id_tool_path: Some(id_tools(dir, 0o4755, 0o4755).into_os_string()),
        // A registry address is configured, as a test of the fetch has it.
        plain_http_registries: vec!["127.0.0.1:1".to_owned()],
    }
}

/// A directory under `dir` with `newuidmap` and `newgidmap` of the given modes.
fn id_tools(dir: &Path, uid_mode: u32, gid_mode: u32) -> PathBuf {
    let tools = dir.join(format!("tools-{uid_mode:o}-{gid_mode:o}"));
    std::fs::create_dir_all(&tools).expect("tools");
    for (name, mode) in [("newuidmap", uid_mode), ("newgidmap", gid_mode)] {
        let path = tools.join(name);
        std::fs::write(&path, b"#!/bin/true\n").expect("tool");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).expect("chmod");
    }
    tools
}

fn start(config: &OciConfig, scratch: &Path) -> Result<Arc<Oci>, OciError> {
    Oci::on(config, scratch, "/actions", "x86_64")
}

/// Catches the helpers running the daemon's binary from where it was installed: a helper
/// with an action's ids cannot search a 0750 home directory the binary may sit in, so the
/// daemon copies itself under the scratch directory and runs that copy.
#[test]
fn helpers_run_a_copy_of_the_daemon_under_the_scratch_directory() {
    let dir = scratch("daemon-copy");
    let oci = start(&config(&dir, true), &dir.join("s")).expect("starts");
    assert_eq!(oci.daemon, dir.join("s/oci-bin/kbf-daemon"));
    let mode = std::fs::metadata(&oci.daemon)
        .expect("copy")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o755);
    let original = std::fs::read(std::env::current_exe().expect("test binary")).expect("read");
    assert_eq!(std::fs::read(&oci.daemon).expect("copy"), original);
}

/// Catches an error that does not name the path it was about, and one that is lost.
#[test]
fn an_io_error_names_its_path() {
    let missing = Path::new("/nonexistent-kbf/a");
    let error = at(missing, std::fs::read(missing)).expect_err("absent");
    let text = String::from(error);
    assert!(text.starts_with("/nonexistent-kbf/a: "), "{text}");
    assert_eq!(at(missing, Ok(7)).expect("passes"), 7);
    let said = String::from(JobError::from("crun said no".to_owned()));
    assert_eq!(said, "crun said no");
}

/// What starting the backend on `scratch` says, as text: `Ok` for a backend that started.
fn started(config: &OciConfig, scratch: &Path) -> String {
    match start(config, scratch) {
        Ok(_) => "started".to_owned(),
        Err(e) => e.to_string(),
    }
}

/// Catches a backend that starts on a node that cannot run it, and an error that does not
/// say which check failed: an architecture with no seccomp profile, a missing runtime or
/// daemon binary, a scratch directory or helper root that cannot be made, a closed
/// ancestor of the scratch directory, and ids the node does not have.
#[test]
fn the_backend_refuses_a_node_that_cannot_run_it() {
    let dir = scratch("refuses");
    let good = config(&dir, true);
    assert_eq!(started(&good, &dir.join("scratch")), "started");

    let on = Oci::on(&good, &dir.join("s1"), "/actions", "riscv64");
    let refused = on.err().map(|e| e.to_string());
    let says = "this machine's architecture (riscv64) has no seccomp profile";
    assert_eq!(refused.as_deref(), Some(says));

    let mut absent = good.clone();
    absent.runtime = dir.join("absent-crun");
    let said = started(&absent, &dir.join("s2"));
    assert!(
        said.starts_with(&format!("{} is not a file", absent.runtime.display())),
        "{said}"
    );
    let mut absent = good.clone();
    absent.daemon = dir.join("absent-daemon");
    let said = started(&absent, &dir.join("s3"));
    assert!(said.contains("absent-daemon is not a file"), "{said}");

    std::fs::write(dir.join("a-file"), b"x").expect("file");
    let said = started(&good, &dir.join("a-file/scratch"));
    assert!(said.contains("a-file/scratch: "), "{said}");

    let scratch = dir.join("s4");
    std::fs::create_dir_all(&scratch).expect("scratch");
    std::fs::write(scratch.join("oci-helper"), b"in the way").expect("file");
    let said = started(&good, &scratch);
    assert!(said.contains("oci-helper: "), "{said}");

    let closed = dir.join("closed");
    std::fs::create_dir_all(closed.join("scratch")).expect("scratch");
    std::fs::set_permissions(&closed, std::fs::Permissions::from_mode(0o700)).expect("chmod");
    let mut searching = good.clone();
    searching.check_search = true;
    let said = started(&searching, &closed.join("scratch"));
    assert!(
        said.contains("closed is not searchable by others"),
        "{said}"
    );
    std::fs::set_permissions(&closed, std::fs::Permissions::from_mode(0o755)).expect("chmod");

    std::fs::write(&good.id_files.subuid, "").expect("empty");
    let said = started(&good, &dir.join("s5"));
    assert!(said.contains("has no range"), "{said}");
}

/// Catches a node whose `crun` could not map the container's ids starting anyway, so
/// that the first action fails instead of the start: each of the two tools missing from
/// `PATH` (and no `PATH` at all), present but neither setuid nor holding file
/// capabilities, or setuid but not executable, is refused naming the tool and the
/// file; a setuid pair is accepted from the second `PATH` directory, and the first
/// directory without the tool is passed over.
#[test]
fn the_backend_refuses_a_node_without_usable_id_tools() {
    let dir = scratch("id-tools");
    let good = config(&dir, true);
    let empty = dir.join("empty");
    std::fs::create_dir_all(&empty).expect("empty");
    let with_path = |path: Option<std::ffi::OsString>| OciConfig {
        id_tool_path: path,
        ..good.clone()
    };
    let joined = |dirs: &[&Path]| std::env::join_paths(dirs).expect("PATH");
    let said = started(&with_path(None), &dir.join("s1"));
    assert!(said.starts_with("newuidmap: not found on PATH"), "{said}");
    let said = started(&with_path(Some(empty.clone().into())), &dir.join("s2"));
    assert!(said.starts_with("newuidmap: not found on PATH"), "{said}");
    assert!(said.contains("package uidmap"), "{said}");

    let half = id_tools(&dir, 0o4755, 0o4755);
    std::fs::remove_file(half.join("newgidmap")).expect("remove");
    let said = started(&with_path(Some(half.into())), &dir.join("s3"));
    assert!(said.starts_with("newgidmap: not found on PATH"), "{said}");

    let plain = id_tools(&dir, 0o755, 0o4755);
    let said = started(&with_path(Some(plain.clone().into())), &dir.join("s4"));
    assert!(
        said.starts_with("newuidmap: ")
            && said.contains("is neither setuid nor holds file capabilities"),
        "{said}"
    );
    assert!(
        said.contains(&plain.join("newuidmap").display().to_string()),
        "{said}"
    );

    let closed = id_tools(&dir, 0o4755, 0o4600);
    let said = started(&with_path(Some(closed.into())), &dir.join("s5"));
    assert!(
        said.starts_with("newgidmap: ") && said.contains("is not executable"),
        "{said}"
    );

    let both = joined(&[&empty, &id_tools(&dir, 0o4755, 0o4755)]);
    assert_eq!(started(&with_path(Some(both)), &dir.join("s6")), "started");
}

/// Catches the search check refusing a scratch directory that is fine: one under the
/// system's temporary directory (every ancestor searchable) is accepted.
#[test]
fn a_scratch_directory_others_can_reach_is_accepted() {
    let dir = scratch("searchable");
    let under_tmp = std::env::temp_dir().join(format!("kbf-oci-unit-{}", std::process::id()));
    let mut config = config(&dir, true);
    config.check_search = true;
    let said = started(&config, &under_tmp);
    std::fs::remove_dir_all(&under_tmp).expect("remove");
    assert_eq!(said, "started");
}

/// Catches an unpack a killed daemon left being kept (a stage directory the container's
/// ids own, a tar), a sweep that removes the images themselves or things that are not
/// stages, one that hides a failure, and a start that goes on after one.
#[test]
fn the_start_removes_the_unpacks_a_killed_daemon_left() {
    let dir = scratch("sweep");
    let scratch = dir.join("scratch");
    let images = scratch.join("oci-images");
    // Nothing yet: the images directory does not exist.
    assert_eq!(started(&config(&dir, true), &scratch), "started");
    std::fs::create_dir_all(images.join(".stage-aa-1-0")).expect("stage");
    std::fs::write(images.join(".stage-aa-1-0.tar"), b"tar").expect("tar");
    std::fs::create_dir_all(images.join("bb/rootfs")).expect("an image");
    std::fs::write(images.join("note"), b"x").expect("a file");
    // `true` stands in for the helper that deletes what is in the stage.
    assert_eq!(started(&config(&dir, true), &scratch), "started");
    assert!(!images.join(".stage-aa-1-0").exists());
    assert!(!images.join(".stage-aa-1-0.tar").exists());
    assert!(images.join("bb/rootfs").is_dir());
    assert!(images.join("note").is_file());

    std::fs::create_dir_all(images.join(".stage-cc-1-1")).expect("stage");
    let said = started(&config(&dir, false), &scratch);
    assert!(
        said.contains("removing unpacks a previous daemon left: "),
        "{said}"
    );
    assert!(
        images.join(".stage-cc-1-1").exists(),
        "kept when it could not be removed"
    );
}

/// Catches a helper root that is not idempotent (the second lease to prepare it fails on
/// the links the first made), one that cannot say it failed, and links that do not lead
/// into `/usr`.
#[test]
fn the_helper_root_is_made_once_and_again_without_error() {
    let dir = scratch("rootfs").join("root");
    helper::prepare_rootfs(&dir).expect("made");
    helper::prepare_rootfs(&dir).expect("made again");
    for (name, target) in [
        ("bin", "usr/bin"),
        ("sbin", "usr/sbin"),
        ("lib", "usr/lib"),
        ("lib64", "usr/lib64"),
    ] {
        assert_eq!(
            std::fs::read_link(dir.join(name)).expect(name),
            Path::new(target)
        );
    }
    for point in ["dev", "usr", "etc", "kbf", "t/0", "t/7"] {
        assert!(dir.join(point).is_dir(), "{point}");
    }
    assert!(dir.join("kbf/daemon").is_file());
    assert!(dir.join("etc/ld.so.cache").is_file());

    // A root that cannot be written to refuses, and says so.
    let closed = scratch("rootfs-closed");
    std::fs::create_dir_all(closed.join("root/dev")).expect("partly made");
    for point in ["usr", "etc", "kbf"] {
        std::fs::create_dir_all(closed.join("root").join(point)).expect("dir");
    }
    for i in 0..8 {
        std::fs::create_dir_all(closed.join(format!("root/t/{i}"))).expect("dir");
    }
    for file in ["etc/ld.so.cache", "kbf/daemon"] {
        std::fs::write(closed.join("root").join(file), b"").expect("file");
    }
    std::fs::set_permissions(closed.join("root"), std::fs::Permissions::from_mode(0o500))
        .expect("chmod");
    assert!(helper::prepare_rootfs(&closed.join("root")).is_err());
    std::fs::set_permissions(closed.join("root"), std::fs::Permissions::from_mode(0o700))
        .expect("chmod");
    std::fs::write(closed.join("a-file"), b"x").expect("file");
    assert!(helper::prepare_rootfs(&closed.join("a-file/root")).is_err());
}

/// Catches a helper's configuration running a shell, mapping the wrong namespace,
/// binding the daemon without being asked to, or binding a directory read-write that
/// the job does not write.
#[test]
fn a_helper_runs_one_program_in_the_namespace_it_is_given() {
    let ids = DaemonIds {
        uid: 990,
        gid: 991,
        sub_uid: 100_000,
        sub_gid: 200_000,
    };
    let root = Path::new("/scratch/oci-helper");
    let render = |mapping: Mapping, job: &helper::Job| -> serde_json::Value {
        let config = helper::config(root, &ids, mapping, "/actions/kbf-helper-x", job);
        serde_json::to_value(config).expect("json")
    };
    let give = helper::give_job(
        Path::new("/usr/local/bin/kbf-daemon"),
        "1:1",
        Some(Path::new("/scratch/l/root")),
        &[Path::new("/scratch/l/upper"), Path::new("/scratch/l/work")],
    );
    assert_eq!(
        give.args,
        [
            "/kbf/daemon",
            "__chown-dirs",
            "1:1",
            "/t/0",
            "--whole",
            "/t/1",
            "/t/2"
        ]
    );
    let config = render(Mapping::Setup, &give);
    assert_eq!(config["process"]["args"], json!(give.args));
    assert_eq!(config["process"]["noNewPrivileges"], true);
    // None of the defaults `oci-spec` fills in: a hostname needs a UTS namespace, masked
    // paths a /proc, resources a cgroup.
    for absent in ["hostname", "annotations"] {
        assert!(config.get(absent).is_none(), "{absent}");
    }
    // Its cgroup is named, under the actions' parent; crun makes none of its own.
    assert_eq!(config["linux"]["cgroupsPath"], "/actions/kbf-helper-x");
    for absent in ["resources", "seccomp"] {
        assert!(config["linux"].get(absent).is_none(), "{absent}");
    }
    // Every capability set is the given one or empty, as `capset` needs.
    let caps = &config["process"]["capabilities"];
    assert_eq!(caps["inheritable"], json!([]));
    assert_eq!(caps["ambient"], json!([]));
    assert_eq!(caps["permitted"].as_array().map(Vec::len), Some(11));
    assert_eq!(config["linux"]["maskedPaths"], json!([]));
    assert_eq!(config["linux"]["readonlyPaths"], json!([]));
    let uids = &config["linux"]["uidMappings"];
    assert_eq!(uids[0], json!({"containerID": 0, "hostID": 990, "size": 1}));
    assert_eq!(
        uids[1],
        json!({"containerID": 1, "hostID": 100_000, "size": 65_536})
    );
    let sources: Vec<_> = config["mounts"]
        .as_array()
        .expect("mounts")
        .iter()
        .map(|m| {
            (
                m["destination"].as_str().unwrap().to_owned(),
                m["options"][1].clone(),
            )
        })
        .collect();
    assert!(
        sources.contains(&("/kbf/daemon".to_owned(), json!("ro"))),
        "{sources:?}"
    );
    assert!(
        sources.contains(&("/t/2".to_owned(), json!("rw"))),
        "{sources:?}"
    );
    assert!(
        sources.contains(&("/usr".to_owned(), json!("ro"))),
        "{sources:?}"
    );

    // No cache: no directories-only tree; no daemon bind for a job that runs none.
    let whole = helper::give_job(
        Path::new("/d"),
        "0:0",
        None,
        &[Path::new("/scratch/l/root")],
    );
    assert_eq!(whole.args[3], "-");
    let remove = render(
        Mapping::Action,
        &helper::remove_job(Path::new("/scratch/l")),
    );
    assert_eq!(
        remove["linux"]["uidMappings"],
        json!([{"containerID": 0, "hostID": 100_000, "size": 65_536}])
    );
    assert!(
        remove["mounts"]
            .as_array()
            .unwrap()
            .iter()
            .all(|m| m["destination"] != "/kbf/daemon")
    );
    assert_eq!(
        remove["process"]["args"],
        json!(["/usr/bin/find", "/t/0", "-mindepth", "1", "-delete"])
    );
    // Only the daemon's own binary and the node's tools are ever run.
    let programs = ["/kbf/daemon", "/usr/bin/find"];
    for argv in [&give.args, &whole.args] {
        assert!(programs.contains(&argv[0].as_str()), "{argv:?}");
    }
}

/// Catches the unpack running anything but the daemon's own `__unpack-layers` mode, or
/// a step reading or writing the wrong directory: the stage is writable, the blobs are
/// not, the daemon's binary is bound, and the layers follow in order as names.
#[test]
fn the_unpack_is_one_daemon_mode_over_two_directories() {
    let layers = ["0.tar.gz".to_owned(), "1.tar".to_owned()];
    let job = helper::unpack_job(
        Path::new("/the/daemon"),
        Path::new("/s/stage"),
        Path::new("/s/blobs"),
        &layers,
    );
    assert_eq!(
        job.args,
        [
            "/kbf/daemon",
            "__unpack-layers",
            "/t/0",
            "/t/1",
            "0.tar.gz",
            "1.tar"
        ]
    );
    assert_eq!(job.daemon.as_deref(), Some(Path::new("/the/daemon")));
    assert_eq!(job.binds[0].source, Path::new("/s/stage"));
    assert!(job.binds[0].writable);
    assert_eq!(job.binds[1].source, Path::new("/s/blobs"));
    assert!(!job.binds[1].writable);
    assert_eq!(job.binds.len(), 2);
}
