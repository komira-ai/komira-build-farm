//! The OCI backend against a stand-in `crun` (`support/fake_oci.rs`): what the bundle
//! carries, what runs and in what order, the image store, and the node checks. What
//! the Podman path and this one both promise is in `conformance`; the real `crun` is
//! `oci_crun.rs`'s.
//!
//! A module of `fake_podman.rs`, not a test binary of its own, for the reason
//! `fake_memory` gives: the coverage job counts a function of the driver in the one test
//! binary that covers it best, so the runtime code both backends share must be run by
//! one binary.

use std::time::{Duration, Instant};

use crate::support::fake::{Fake, image, sha256};
use crate::support::fake_oci::FakeOci;
use crate::support::registry::{Blob, FakeRegistry, Image, Item};
use crate::support::{Spec, blob, exists};
use kbf_daemon::RuntimeError;
use kbf_driver_container::{ContainerLimits, IdFiles, StartError};
use serde_json::{Value, json};

fn names(value: &Value) -> Vec<String> {
    value
        .as_array()
        .expect("an array")
        .iter()
        .map(|v| v.as_str().expect("a string").to_owned())
        .collect()
}

/// Catches Podman (or any process but `crun` and its helpers) on the action path: a
/// lease must cost two helper runs (the directories handed over and back), the run and
/// the `delete --force` of the clean, and the stand-in `podman`, beside the `crun`
/// link, must not be run at all.
#[tokio::test]
async fn an_action_costs_four_runtime_processes_and_no_podman() {
    let fake = FakeOci::new("three-processes");
    let result = fake
        .run(1, &Spec::new(&image(), "unused"), &["exit", "0"])
        .await
        .expect("ran");
    assert_eq!(result.exit_code, 0);
    assert_eq!(
        fake.calls(),
        ["helper give", "run", "helper give", "delete"]
    );
    assert_eq!(fake.podman_calls(), Vec::<String>::new());
    fake.assert_clean(1);
}

/// For comparison, what the Podman path costs for the same action on the same
/// stand-ins: more processes than the OCI backend's four (the figure the design
/// measured on a node is nine with the input cache). The ratio is structure only; the
/// seconds each takes are a node's to measure.
#[tokio::test]
async fn the_podman_path_runs_more_processes_for_the_same_action() {
    let fake = Fake::new("podman-contrast");
    fake.run(1, &Spec::new(&image(), "unused"), &["exit", "0"])
        .await
        .expect("ran");
    let calls = fake.calls();
    assert!(calls.len() > 4, "{calls:?}");
    assert!(calls.contains(&"create".to_owned()) && calls.contains(&"start".to_owned()));
}

/// Times the OCI lifecycle (bundle, helper, run, helper, clean) against the stand-in
/// `crun`: the driver's own overhead around a runtime that costs a few milliseconds.
/// Structure only: it asserts the lifecycle stays in the same order of magnitude and
/// prints the figure; what a real `crun`, a real overlay and real namespaces add is
/// to be measured on a build host (see the PR description's list).
#[tokio::test]
async fn the_driver_adds_little_around_the_runtime() {
    let fake = FakeOci::new("overhead");
    let spec = Spec::new(&image(), "unused");
    // One to warm the process tables and the page cache.
    fake.run(1, &spec, &["exit", "0"]).await.expect("warm-up");
    let runs: u64 = 20;
    let started = Instant::now();
    for seq in 2..2 + runs {
        fake.run(seq, &spec, &["exit", "0"]).await.expect("ran");
    }
    let per_lease = started.elapsed() / u32::try_from(runs).expect("small");
    eprintln!("OCI lifecycle against the stand-in crun: {per_lease:?} per lease");
    assert!(
        per_lease < Duration::from_secs(2),
        "{per_lease:?} per lease"
    );
}

/// Catches each flag Podman is given at `create` going missing from the bundle: the
/// argv, the working directory, the Command's environment (and the two variables
/// Podman adds), the user, the limits, the cgroup, the hostname, the read-only image.
#[tokio::test]
async fn the_bundle_carries_what_podman_create_was_given() {
    let fake = FakeOci::with("bundle", |c| {
        c.limits = ContainerLimits {
            pids: 101,
            shm_mib: 102,
            nofile: 103,
            nproc: 104,
        };
    });
    let mut spec = Spec::new(&image(), "echo \"a b\"");
    spec.working_directory = "pkg".to_owned();
    spec.env = vec![("PATH".to_owned(), "/bin".to_owned())];
    fake.run(1, &spec, &["exit", "0"]).await.expect("ran");
    let config = fake.config_json();
    let process = &config["process"];
    assert_eq!(process["args"], json!(spec.argv));
    assert_eq!(process["cwd"], "/kbf/root/pkg");
    assert_eq!(process["terminal"], false);
    assert_eq!(process["user"]["uid"], 0);
    assert_eq!(process["user"]["gid"], 0);
    assert_eq!(process["oomScoreAdj"], 0);
    assert_eq!(
        process["env"],
        json!(["PATH=/bin", "HOSTNAME=localhost", "HOME=/root"])
    );
    let rlimits = process["rlimits"].as_array().expect("rlimits");
    let limit = |kind: &str| {
        let found = rlimits.iter().find(|r| r["type"] == kind).expect(kind);
        (found["soft"].clone(), found["hard"].clone())
    };
    assert_eq!(limit("RLIMIT_NOFILE"), (json!(103), json!(103)));
    assert_eq!(limit("RLIMIT_NPROC"), (json!(104), json!(104)));
    assert_eq!(config["hostname"], "localhost");
    assert_eq!(config["root"]["readonly"], true);
    let rootfs = fake.image_dir().join("rootfs");
    assert_eq!(config["root"]["path"], rootfs.to_str().expect("utf-8"));
    let linux = &config["linux"];
    assert_eq!(linux["cgroupsPath"], "/actions/kbf-lease-1-1/ctr");
    assert_eq!(linux["resources"]["pids"]["limit"], 101);
    assert_eq!(linux["resources"]["unified"]["memory.oom.group"], "1");
    let shm = config["mounts"]
        .as_array()
        .expect("mounts")
        .iter()
        .find(|m| m["destination"] == "/dev/shm")
        .expect("/dev/shm");
    assert!(
        names(&shm["options"]).contains(&"size=102m".to_owned()),
        "{shm}"
    );
}

/// Catches Podman's `--env` handling lost: a Command that sets `HOME` or `HOSTNAME`
/// keeps its own value, and nothing of the node's environment reaches the action.
#[tokio::test]
async fn the_commands_home_and_hostname_win() {
    let fake = FakeOci::new("env");
    let mut spec = Spec::new(&image(), "unused");
    spec.env = vec![
        ("HOME".to_owned(), "/work".to_owned()),
        ("HOSTNAME".to_owned(), "builder".to_owned()),
    ];
    fake.run(1, &spec, &["exit", "0"]).await.expect("ran");
    assert_eq!(
        fake.config_json()["process"]["env"],
        json!(["HOME=/work", "HOSTNAME=builder"])
    );
}

/// Catches the isolation Podman gave regressing: the network namespace left shared
/// (`--network=none`), a namespace joined by path instead of made, the daemon user
/// mapped into the container (`--userns=nomap`), a capability beyond Podman's eleven,
/// the filter or the masked paths missing.
#[tokio::test]
async fn the_container_is_isolated_as_podmans_is() {
    let fake = FakeOci::new("isolation");
    fake.run(1, &Spec::new(&image(), "unused"), &["exit", "0"])
        .await
        .expect("ran");
    let linux = fake.config_json()["linux"].clone();
    let mut kinds = Vec::new();
    for ns in linux["namespaces"].as_array().expect("namespaces") {
        assert!(ns.get("path").is_none(), "{ns} joins a namespace");
        kinds.push(ns["type"].as_str().expect("type").to_owned());
    }
    kinds.sort();
    assert_eq!(
        kinds,
        ["cgroup", "ipc", "mount", "network", "pid", "user", "uts"]
    );
    // The daemon's own id is mapped in neither direction.
    let uid = u64::from(rustix::process::getuid().as_raw());
    for key in ["uidMappings", "gidMappings"] {
        let maps = linux[key].as_array().expect(key);
        assert_eq!(maps.len(), 1, "{linux}");
        let (host, size) = (
            maps[0]["hostID"].as_u64().unwrap(),
            maps[0]["size"].as_u64().unwrap(),
        );
        assert_eq!(maps[0]["containerID"], 0);
        assert_eq!((host, size), (100_000, 65_536));
        assert!(
            !(host..host + size).contains(&uid),
            "maps the daemon's own id"
        );
    }
    let caps = fake.config_json()["process"]["capabilities"].clone();
    for set in ["bounding", "effective", "permitted"] {
        let mut held = names(&caps[set]);
        held.sort();
        assert_eq!(held.len(), 11, "{set}: {held:?}");
        assert!(!held.contains(&"CAP_SYS_ADMIN".to_owned()), "{held:?}");
        assert!(!held.contains(&"CAP_NET_RAW".to_owned()), "{held:?}");
    }
    assert_eq!(caps["inheritable"], json!([]));
    assert_eq!(caps["ambient"], json!([]));
    assert_eq!(linux["seccomp"]["defaultAction"], "SCMP_ACT_ERRNO");
    assert!(names(&linux["maskedPaths"]).contains(&"/proc/kcore".to_owned()));
    assert!(names(&linux["readonlyPaths"]).contains(&"/proc/sys".to_owned()));
    // Memory and CPU limits are the lease cgroup's, as on the Podman path.
    assert!(linux["resources"].get("memory").is_none());
    assert!(linux["resources"].get("cpu").is_none());
}

/// Catches the exec root mounted writable from the host (an action could change the
/// inputs), the pinned overlay options lost, `/etc/hosts` writable, or the writable
/// places missing: the lease's own directories at `/tmp`, `/var/tmp`, `/run`, `/root`.
#[tokio::test]
async fn the_exec_root_is_an_overlay_and_the_image_is_read_only() {
    let fake = FakeOci::new("mounts");
    fake.run(1, &Spec::new(&image(), "unused"), &["exit", "0"])
        .await
        .expect("ran");
    let config = fake.config_json();
    let mounts = config["mounts"].as_array().expect("mounts");
    let at = |dest: &str| {
        mounts
            .iter()
            .find(|m| m["destination"] == dest)
            .unwrap_or_else(|| panic!("no mount at {dest}"))
    };
    let lease = fake.lease_dir(1);
    let overlay = at("/kbf/root");
    assert_eq!(overlay["type"], "overlay");
    let options = names(&overlay["options"]);
    let pinned = "userxattr,metacopy=off,redirect_dir=nofollow,index=off,xino=off";
    assert_eq!(
        options[0],
        format!(
            "lowerdir={0}/root,upperdir={0}/upper,workdir={0}/work,{pinned}",
            lease.display()
        )
    );
    assert!(options.contains(&"nosuid".to_owned()) && options.contains(&"nodev".to_owned()));
    for (dest, name) in [
        ("/tmp", "tmp"),
        ("/var/tmp", "var-tmp"),
        ("/run", "run"),
        ("/root", "home"),
    ] {
        let mount = at(dest);
        assert_eq!(
            mount["source"],
            lease.join("rw").join(name).to_str().unwrap()
        );
        assert!(
            names(&mount["options"]).contains(&"rw".to_owned()),
            "{mount}"
        );
    }
    for dest in ["/etc/hosts", "/etc/hostname"] {
        assert!(names(&at(dest)["options"]).contains(&"ro".to_owned()));
    }
    // The overlay comes before anything mounted inside it.
    let order: Vec<_> = mounts
        .iter()
        .map(|m| m["destination"].as_str().unwrap())
        .collect();
    let pos = |d: &str| order.iter().position(|x| *x == d).unwrap();
    assert!(pos("/proc") < pos("/kbf/root"));
}

/// Catches the lease's directories being handed over without the extra writable
/// directories (`/tmp` of the action would then be owned by nobody it can write as),
/// and the order of the hand-over: the input root first, then upper, work and `rw`.
#[tokio::test]
async fn the_hand_over_covers_every_directory_the_action_writes() {
    let fake = FakeOci::new("hand-over");
    fake.run(1, &Spec::new(&image(), "unused"), &["exit", "0"])
        .await
        .expect("ran");
    let lease = fake.lease_dir(1);
    let l = lease.display();
    let paths = format!("{l}/root {l}/upper {l}/work {l}/rw");
    let events: Vec<_> = fake
        .events()
        .into_iter()
        .filter(|e| e.starts_with("chown"))
        .collect();
    assert_eq!(
        events,
        [format!("chown 1:1 {paths}"), format!("chown 0:0 {paths}")]
    );
}

/// Catches the runtime's failure to start being mistaken for the action's own exit 1,
/// and the reverse: an action that exits 1 with no error record from `crun` is the
/// action's result.
#[tokio::test]
async fn an_exit_one_is_the_actions_unless_crun_logged_an_error() {
    let fake = FakeOci::new("exit-one");
    let result = fake
        .run(1, &Spec::new(&image(), "unused"), &["exit", "1"])
        .await
        .expect("an action's exit 1 is a result");
    assert_eq!(result.exit_code, 1);
    fake.knob(
        "crun-error",
        "mount `/x` to `/kbf/root`: No such file or directory",
    );
    let outcome = fake
        .run(2, &Spec::new(&image(), "unused"), &["exit", "0"])
        .await;
    let Err(RuntimeError::Failed(why)) = outcome else {
        panic!("{outcome:?}");
    };
    assert!(why.contains("crun: mount `/x`"), "{why}");
    assert!(why.contains("did not run to an exit"), "{why}");
    fake.assert_clean(2);
}

/// Catches a program crun cannot find being the farm's failure, not the action's:
/// exit 127 and kbf's message on the action's stderr, as on the Podman path.
#[tokio::test]
async fn a_program_crun_cannot_find_is_the_actions_exit_127() {
    let fake = FakeOci::new("not-found");
    fake.knob(
        "crun-error",
        "executable file `sh` not found in $PATH: No such file or directory",
    );
    let mut spec = Spec::new(&image(), "unused");
    spec.env = vec![("PATH".to_owned(), "/bin".to_owned())];
    let result = fake.run(1, &spec, &["exit", "0"]).await.expect("a result");
    assert_eq!(result.exit_code, 127);
    let stderr = String::from_utf8(blob(&fake.cas, result.stderr_digest.as_ref())).expect("utf-8");
    assert!(
        stderr.contains("kbf: the action's program `sh` was not found"),
        "{stderr}"
    );
    assert!(stderr.contains("crun: executable file `sh`"), "{stderr}");
    fake.assert_clean(1);
}

/// Catches the clean not deleting the container: `crun delete --force` runs for every
/// lease, after the run and the hand-back, and removes what a killed `crun` left of the
/// container (its state, here).
#[tokio::test]
async fn crun_delete_force_closes_every_lease() {
    let fake = FakeOci::new("delete");
    let spec = Spec::new(&image(), "unused");
    fake.run(1, &spec, &["exit", "0"]).await.expect("ran");
    assert_eq!(fake.calls().last().map(String::as_str), Some("delete"));
    assert!(fake.state.join("removed").exists());
    // A `crun` that was killed leaves its state; the delete removes it.
    fake.knob("leave-state", "");
    fake.run(2, &spec, &["exit", "0"]).await.expect("ran");
    assert_eq!(fake.calls().iter().filter(|c| *c == "delete").count(), 2);
    fake.assert_clean(2);
}

/// Catches a scratch directory the daemon's user cannot delete (no permission left on
/// a directory the action made) being left behind: the removal falls back to a helper
/// container, which deletes the contents; and a helper that claims success but leaves
/// the directory is caught.
#[tokio::test]
async fn unremovable_scratch_falls_back_to_a_helper_container() {
    let fake = FakeOci::new("helper-rm");
    let spec = Spec::new(&image(), "unused");
    let locked = [
        "mkdir",
        "$UPPER/locked",
        ";",
        "write-lines",
        "$UPPER/locked/f",
        ";",
        "chmod",
        "000",
        "$UPPER/locked",
    ];
    let result = fake.run(1, &spec, &locked).await.expect("ran");
    assert_eq!(result.exit_code, 0);
    assert!(
        fake.calls().contains(&"helper rm".to_owned()),
        "{:?}",
        fake.calls()
    );
    fake.assert_clean(1);

    fake.knob("rm-noop", "");
    let outcome = fake.run(2, &spec, &locked).await;
    assert!(
        matches!(outcome, Err(RuntimeError::Failed(ref why)) if why.contains("still exists")),
        "{outcome:?}"
    );
    std::fs::remove_file(fake.state.join("rm-noop")).expect("rm knob");
    fake.knob("rm-fails", "");
    let outcome = fake.run(3, &spec, &locked).await;
    assert!(
        matches!(outcome, Err(RuntimeError::Failed(ref why)) if why.contains("rm refused")),
        "{outcome:?}"
    );
}

/// Catches a hand-over that fails being swallowed: the lease fails, naming what the
/// helper said, in either direction, and cleans up.
#[tokio::test]
async fn a_failed_hand_over_fails_the_lease() {
    for owner in ["1:1", "0:0"] {
        let fake = FakeOci::new(&format!("give-fails-{}", owner.replace(':', "-")));
        fake.knob("chown-fails", owner);
        let outcome = fake
            .run(1, &Spec::new(&image(), "unused"), &["exit", "0"])
            .await;
        let Err(RuntimeError::Failed(why)) = outcome else {
            panic!("{owner}: {outcome:?}");
        };
        assert!(why.contains("chown refused"), "{owner}: {why}");
        fake.assert_clean(1);
    }
}

/// A small image: two layers, the second adding a file and deleting one of the first's.
fn tiny_image() -> Image {
    Image::of(vec![
        Blob::gzip_tar(&[
            Item::File {
                path: "bin/tool",
                data: b"tool",
                mode: 0o755,
            },
            Item::File {
                path: "etc/os-release",
                data: b"tiny",
                mode: 0o644,
            },
            Item::File {
                path: "etc/removed",
                data: b"gone",
                mode: 0o644,
            },
        ]),
        Blob::plain_tar(&[
            Item::File {
                path: "etc/added",
                data: b"added",
                mode: 0o644,
            },
            Item::File {
                path: "etc/.wh.removed",
                data: b"",
                mode: 0o644,
            },
        ]),
    ])
}

fn unpacks(fake: &FakeOci) -> usize {
    fake.calls()
        .iter()
        .filter(|c| *c == "helper unpack")
        .count()
}

fn manifest_requests(fake: &FakeOci) -> usize {
    fake.registry_requests()
        .iter()
        .filter(|r| r.contains("/manifests/"))
        .count()
}

/// Catches the image being fetched or unpacked on the action path more than once, a
/// root that is not the image's, and anything but the node's own code running: the
/// first lease of an image that is not in the store fetches it from the registry,
/// unpacks it in a helper (layers in order, the whiteout applied, the mount points made)
/// and runs the action on it, with the failing `podman` stub never run; the next lease
/// finds the directory with a `stat`, asks the registry nothing and unpacks nothing.
#[tokio::test]
async fn an_image_is_fetched_and_unpacked_once_and_then_found_with_a_stat() {
    let fake = FakeOci::fetching("fetch", FakeRegistry::start().await);
    let image = tiny_image();
    let spec = Spec::new(&fake.publish(&image), "unused");
    fake.run(1, &spec, &["exit", "0"]).await.expect("ran");
    let rootfs = fake.image_dir_of(&image.digest).join("rootfs");
    let read = |rel: &str| std::fs::read_to_string(rootfs.join(rel)).unwrap_or_default();
    assert_eq!(read("bin/tool"), "tool");
    assert_eq!(read("etc/os-release"), "tiny");
    assert_eq!(read("etc/added"), "added");
    assert!(
        !exists(&rootfs.join("etc/removed")),
        "the whiteout was not applied"
    );
    assert!(rootfs.join("kbf/root").is_dir() && rootfs.join("etc/hosts").is_file());
    assert_eq!(unpacks(&fake), 1, "{:?}", fake.calls());
    assert_eq!(fake.podman_calls(), Vec::<String>::new());
    // The blobs and the stage are gone; only the unpacked image remains.
    assert_eq!(
        fake.image_store(),
        [image.digest.trim_start_matches("sha256:")],
    );

    let requests = fake.registry_requests().len();
    fake.run(2, &spec, &["exit", "0"]).await.expect("ran again");
    assert_eq!(fake.registry_requests().len(), requests);
    assert_eq!(unpacks(&fake), 1);
    assert_eq!(fake.podman_calls(), Vec::<String>::new());
}

/// Catches concurrent first uses of one image fetching it once each (the single
/// flight): five checks at once (what five first leases, or the `--image` checks at
/// start, ask), one manifest request and one unpack.
#[tokio::test]
async fn concurrent_first_uses_share_one_fetch() {
    let fake = FakeOci::fetching("fetch-flight", FakeRegistry::start().await);
    let reference = fake.publish(&tiny_image());
    let image = kbf_driver_container::ImageRef::parse(&reference).expect("image");
    let checks = (0..5).map(|_| fake.runtime.check_pinned(&image));
    for checked in futures::future::join_all(checks).await {
        checked.expect("usable");
    }
    assert_eq!(
        manifest_requests(&fake),
        1,
        "{:?}",
        fake.registry_requests()
    );
    assert_eq!(unpacks(&fake), 1, "{:?}", fake.calls());
}

/// Catches an image that cannot be fetched or unpacked failing the lease as the
/// action's fault, a half-unpacked image being left for the next lease to use, and the
/// stage or the blobs being left behind: the registry cannot find the image, the
/// registry is down, the unpack helper fails; the action never runs, and a later lease
/// with the fault gone fetches the image properly.
#[tokio::test]
async fn a_failed_fetch_or_unpack_fails_the_lease_and_leaves_no_image() {
    let fake = FakeOci::fetching("fetch-fails", FakeRegistry::start().await);
    let image = tiny_image();
    let spec = Spec::new(&fake.publish(&image), "unused");
    let failed = |outcome: Result<_, RuntimeError>, want: &str| {
        let Err(RuntimeError::Failed(why)) = outcome else {
            panic!("{want}: {outcome:?}");
        };
        assert!(why.contains(want), "{want}: {why}");
    };
    let clean = |fake: &FakeOci| {
        assert!(
            !exists(&fake.image_dir_of(&image.digest)),
            "an image directory was left"
        );
        assert_eq!(fake.image_store(), Vec::<String>::new());
        assert!(!fake.calls().contains(&"run".to_owned()), "an action ran");
    };

    fake.registry
        .as_ref()
        .unwrap()
        .behave(|b| b.status = Some(404));
    failed(fake.run(1, &spec, &["exit", "0"]).await, "404");
    clean(&fake);
    fake.registry.as_ref().unwrap().behave(|b| b.status = None);

    fake.knob("unpack-fails", "");
    failed(fake.run(2, &spec, &["exit", "0"]).await, "unpack refused");
    clean(&fake);
    std::fs::remove_file(fake.state.join("unpack-fails")).expect("knob");

    fake.run(3, &spec, &["exit", "0"])
        .await
        .expect("ran once the fault was gone");
    assert!(exists(&fake.image_dir_of(&image.digest).join("rootfs")));
}

/// Catches an image whose bytes are not its digest's running (the tampered layer: the
/// registry serves other bytes under a layer's digest) and a manifest that is not the
/// pinned one: the lease fails naming the digest, nothing is unpacked or left, and no
/// action runs.
#[tokio::test]
async fn a_tampered_image_is_refused_before_it_is_unpacked() {
    let fake = FakeOci::fetching("fetch-tampered", FakeRegistry::start().await);
    let image = tiny_image();
    let spec = Spec::new(&fake.publish(&image), "unused");
    let layer = sha256(&image.layers[1].bytes);
    let mut bytes = image.layers[1].bytes.clone();
    let last = bytes.len() - 1;
    bytes[last] ^= 0xff;
    fake.registry
        .as_ref()
        .unwrap()
        .publish_blob_as(&layer, &bytes);
    let outcome = fake.run(1, &spec, &["exit", "0"]).await;
    let Err(RuntimeError::Failed(why)) = outcome else {
        panic!("{outcome:?}");
    };
    assert!(why.contains(&layer) && why.contains("hashes to"), "{why}");
    assert_eq!(unpacks(&fake), 0);
    assert_eq!(fake.image_store(), Vec::<String>::new());
    assert!(!fake.calls().contains(&"run".to_owned()));

    // The registry answers the pinned digest with other bytes.
    let other = Image::of(vec![Blob::plain_tar(&[Item::Dir {
        path: "evil",
        mode: 0o755,
    }])]);
    fake.registry.as_ref().unwrap().publish_manifest_as(
        &image.digest,
        "application/vnd.oci.image.manifest.v1+json",
        &other.manifest,
    );
    let outcome = fake.run(2, &spec, &["exit", "0"]).await;
    let Err(RuntimeError::Failed(why)) = outcome else {
        panic!("{outcome:?}");
    };
    assert!(
        why.contains("hashes to") && why.contains(&image.digest),
        "{why}"
    );
    assert_eq!(unpacks(&fake), 0);
}

/// Catches an image index digest running (the "index accepted" mutant): the registry
/// serves an index under the pinned digest; the refusal is the action's (`Invalid`), is
/// made before anything is fetched but the manifest, and unpacks nothing.
#[tokio::test]
async fn an_index_digest_is_refused_before_it_is_unpacked() {
    let fake = FakeOci::fetching("fetch-index", FakeRegistry::start().await);
    let digest = sha256(crate::support::fake::INDEX);
    let registry = fake.registry.as_ref().unwrap();
    registry.publish_manifest_as(
        &digest,
        "application/vnd.oci.image.index.v1+json",
        crate::support::fake::INDEX,
    );
    let reference = format!("docker://{}/test/tiny@{digest}", registry.addr);
    let outcome = fake
        .run(1, &Spec::new(&reference, "unused"), &["exit", "0"])
        .await;
    assert!(
        matches!(outcome, Err(RuntimeError::Invalid(_))),
        "{outcome:?}"
    );
    assert_eq!(unpacks(&fake), 0);
    assert_eq!(
        fake.registry_requests().len(),
        1,
        "{:?}",
        fake.registry_requests()
    );
    assert_eq!(fake.image_store(), Vec::<String>::new());
}

/// Catches the node checks being skipped: the OCI backend refuses to start on a node
/// whose daemon user has no single subordinate range of 65,536 ids, whose runtime is
/// not there, or whose scratch directory has an ancestor others cannot search.
#[test]
fn the_node_is_checked_at_start() {
    let fake = FakeOci::new("node-checks");
    let start = |configure: &dyn Fn(&mut kbf_driver_container::PodmanConfig)| {
        let mut config = fake.config.clone();
        configure(&mut config);
        kbf_driver_container::PodmanRuntime::new(config, std::sync::Arc::clone(&fake.cas)).map(drop)
    };
    start(&|_| {}).expect("a good node starts");

    // Ids: no range at all, then ranges that are enough only together.
    let ids = fake.dir.join("ids-bad");
    let files = IdFiles::in_dir(&ids);
    std::fs::create_dir_all(&ids).unwrap();
    std::fs::write(&files.passwd, "root:x:0:0::/:/bin/sh\n").unwrap();
    let uid = rustix::process::getuid().as_raw();
    std::fs::write(&files.subuid, "").unwrap();
    std::fs::write(&files.subgid, "").unwrap();
    let Err(StartError::Oci(why)) = start(&|c| c.oci.id_files = files.clone()) else {
        panic!("no range was accepted");
    };
    assert!(why.contains("has no range"), "{why}");
    let split = format!("{uid}:100000:40000\n{uid}:200000:40000\n");
    std::fs::write(&files.subuid, &split).unwrap();
    std::fs::write(&files.subgid, &split).unwrap();
    let Err(StartError::Oci(why)) = start(&|c| c.oci.id_files = files.clone()) else {
        panic!("a split range was accepted");
    };
    assert!(why.contains("no single range"), "{why}");

    // The runtime.
    let Err(StartError::Oci(why)) = start(&|c| c.oci.runtime = fake.dir.join("absent")) else {
        panic!("a missing runtime was accepted");
    };
    assert!(why.contains("absent"), "{why}");

    // A scratch directory under one others cannot search (with the check on).
    use std::os::unix::fs::PermissionsExt as _;
    let closed = fake.dir.join("closed");
    std::fs::create_dir_all(closed.join("scratch")).unwrap();
    std::fs::set_permissions(&closed, std::fs::Permissions::from_mode(0o700)).unwrap();
    let Err(StartError::Oci(why)) = start(&|c| {
        c.oci.check_search = true;
        c.scratch = closed.join("scratch");
    }) else {
        panic!("a closed ancestor was accepted");
    };
    assert!(why.contains("not searchable by others"), "{why}");
    assert!(why.contains(closed.to_str().unwrap()), "{why}");
}

/// Catches a runtime that goes away mid-lease being a hang, a panic or the action's
/// result: the lease fails as the farm's, naming the program, and the scratch directory is
/// still removed although `crun delete` could not run.
#[tokio::test]
async fn a_runtime_that_goes_away_fails_the_lease_naming_it() {
    let fake = FakeOci::with("vanish", crate::support::fake_oci::private_runtime);
    fake.knob("vanish-after-give", "");
    let outcome = fake
        .run(1, &Spec::new(&image(), "unused"), &["exit", "0"])
        .await;
    let Err(RuntimeError::Failed(why)) = outcome else {
        panic!("{outcome:?}");
    };
    assert!(
        why.contains(fake.dir.join("crun").to_str().unwrap()),
        "{why}"
    );
    assert!(why.contains("Unable to locate"), "{why}");
    assert!(!exists(&fake.lease_dir(1)), "scratch directory left");
}

/// Catches a runtime that cannot be started (not executable) being taken for an action
/// that ran: the lease fails as the farm's with the system's words.
#[tokio::test]
async fn a_runtime_that_cannot_be_started_fails_the_lease() {
    let fake = FakeOci::with("unexec", crate::support::fake_oci::private_runtime);
    fake.knob("unexec-after-give", "");
    let outcome = fake
        .run(1, &Spec::new(&image(), "unused"), &["exit", "0"])
        .await;
    let Err(RuntimeError::Failed(why)) = outcome else {
        panic!("{outcome:?}");
    };
    assert!(why.starts_with("crun run: "), "{why}");
    assert!(why.contains("Permission denied"), "{why}");
}

/// Catches a `crun kill` that fails ending the kill path early or hanging: a timed-out
/// action is still stopped (by the lease cgroup's kill) and the lease cleaned.
#[tokio::test]
async fn the_kill_path_ends_even_when_crun_kill_fails() {
    let fake = FakeOci::with("kill-fails", |c| c.kill_grace = Duration::from_millis(500));
    fake.knob("kill-fails", "");
    let mut spec = Spec::new(&image(), "unused");
    spec.timeout = Some(Duration::from_secs(1));
    let outcome = fake.run(1, &spec, &["sleep", "30"]).await;
    assert!(
        matches!(outcome, Err(RuntimeError::TimedOut { .. })),
        "{outcome:?}"
    );
    fake.assert_clean(1);
}

/// Catches a failed `crun delete` being swallowed: the clean fails the lease, naming it.
#[tokio::test]
async fn a_failed_delete_fails_the_lease() {
    let fake = FakeOci::new("delete-fails");
    fake.knob("delete-fails", "");
    let outcome = fake
        .run(1, &Spec::new(&image(), "unused"), &["exit", "0"])
        .await;
    let Err(RuntimeError::Failed(why)) = outcome else {
        panic!("{outcome:?}");
    };
    assert!(why.starts_with("clean: "), "{why}");
    assert!(why.contains("crun delete"), "{why}");
}

/// Catches a clean failing a lease because `crun delete` lost a race with the `crun run`
/// of a killed container (which deletes its own state as it exits, and is all a restarted
/// daemon's sweep meets): the state being gone is what the delete was for.
#[tokio::test]
async fn a_delete_that_finds_the_state_gone_is_a_clean_lease() {
    let fake = FakeOci::new("delete-vanished");
    fake.knob("delete-vanished", "");
    fake.run(1, &Spec::new(&image(), "unused"), &["exit", "0"])
        .await
        .expect("ran");
}

/// Catches an action killed by a signal being reported as anything but `128 + n`, the
/// status `crun run` ends with. (137, SIGKILL, sends the lease to the OOM classification,
/// which reads the lease cgroup's `memory.events`: `fake_memory`'s.)
#[tokio::test]
async fn a_signal_death_is_128_plus_the_signal() {
    let fake = FakeOci::new("signal-code");
    // The stand-in reports `128 + n` for an action killed by signal `n`, as `crun run` does.
    let result = fake
        .run(1, &Spec::new(&image(), "unused"), &["kill-self", "15"])
        .await
        .expect("ran");
    assert_eq!(result.exit_code, 143);
    fake.assert_clean(1);
}

/// Catches the input cache not reaching the hand-over of the OCI backend: with it, the
/// input root's directories alone go first (`__chown-dirs`, never its files, which are
/// links to the cache's entries), then the other trees whole.
#[tokio::test]
async fn with_the_cache_only_the_input_roots_directories_change_owner() {
    let dir = crate::support::scratch("fake-oci-cache");
    let fake = FakeOci::with("cache", |config| {
        let at = dir.join("cache");
        std::fs::create_dir_all(&at).expect("cache dir");
        config.cache = Some(kbf_driver_container::CacheUse {
            config: kbf_daemon::cache::CacheConfig::new(at, config.scratch.clone(), 1 << 30),
            helper: std::path::PathBuf::from("/kbf/bin/kbf-daemon"),
            trees: None,
        });
    });
    assert!(fake.runtime.cached());
    let mut spec = Spec::new(&image(), "unused");
    spec.inputs = vec![("src/a.txt", b"input bytes", false)];
    fake.run(1, &spec, &["exit", "0"]).await.expect("ran");
    let l = fake.lease_dir(1);
    let l = l.display();
    let owners: Vec<_> = fake
        .events()
        .into_iter()
        .filter(|e| e.starts_with("chown"))
        .collect();
    assert_eq!(
        owners,
        [
            format!("chown-dirs 1:1 {l}/root"),
            format!("chown 1:1 {l}/upper {l}/work {l}/rw"),
            format!("chown-dirs 0:0 {l}/root"),
            format!("chown 0:0 {l}/upper {l}/work {l}/rw"),
        ]
    );
    crate::support::force_remove(&dir);
}

/// Catches the first helper of a lease failing in either way a runtime can be missing
/// being taken for success or hanging: a program that was removed after the daemon
/// started, and one that is there but cannot be run. Each fails the lease as the farm's,
/// naming the program or the system's words, and the scratch directory is removed.
#[tokio::test]
async fn a_runtime_that_is_gone_or_cannot_run_fails_the_first_hand_over() {
    let fake = FakeOci::new("gone-at-first");
    std::fs::remove_file(fake.dir.join("crun")).expect("remove the link");
    let outcome = fake
        .run(1, &Spec::new(&image(), "unused"), &["exit", "0"])
        .await;
    let Err(RuntimeError::Failed(why)) = outcome else {
        panic!("{outcome:?}");
    };
    assert!(why.contains("Unable to locate"), "{why}");
    assert!(!exists(&fake.lease_dir(1)), "scratch directory left");

    let fake = FakeOci::with("cannot-run", |config| {
        let at = config.oci.runtime.clone();
        std::fs::remove_file(&at).expect("remove the link");
        std::fs::write(&at, b"not a program").expect("a plain file");
    });
    let outcome = fake
        .run(1, &Spec::new(&image(), "unused"), &["exit", "0"])
        .await;
    let Err(RuntimeError::Failed(why)) = outcome else {
        panic!("{outcome:?}");
    };
    assert!(why.contains("(helper "), "{why}");
    assert!(why.contains("Permission denied"), "{why}");
}

/// Catches a kernel OOM kill of the container, which ends `crun run` with 137, not reaching
/// the lease's OOM classification: it is read from the lease cgroup's counters (the
/// container's cgroup is below it), as for Podman, and an exit 137 with no such event is
/// the action's own (`fake_memory`'s tests are the rest of the classification).
#[tokio::test]
async fn exit_137_with_a_kernel_oom_event_is_out_of_memory_with_crun_too() {
    let fake = FakeOci::new("oom");
    let spec = Spec::new(&image(), "unused");
    let write = |file: &str, lines: &[&str]| -> Vec<String> {
        let mut words = vec!["write-lines".to_owned(), format!("$CG/{file}")];
        words.extend(lines.iter().map(|l| (*l).to_owned()));
        words
    };
    let mut steps = write(
        "memory.events",
        &["low 0", "high 4", "max 1", "oom 1", "oom_kill 1"],
    );
    steps.push(";".to_owned());
    steps.extend(write("memory.peak", &["2000000000"]));
    steps.extend([";", "exit", "137"].map(str::to_owned));
    let steps: Vec<&str> = steps.iter().map(String::as_str).collect();
    let outcome = fake.run(1, &spec, &steps).await;
    let cap = (3u64 << 29) + (512 << 20);
    assert!(
        matches!(outcome, Err(RuntimeError::OutOfMemory { used: 2_000_000_000, limit }) if limit == cap),
        "{outcome:?}"
    );
    fake.assert_clean(1);
}
