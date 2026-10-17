//! `config.json` for one action's container, built with `oci-spec`'s typed builders.
//!
//! Each isolation setting of an action's container has its line here (the table is in
//! [`action_config`]'s documentation); the capability set, masked paths and seccomp
//! filter are [`hardening`](super::hardening).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use oci_spec::OciSpecError;
use oci_spec::runtime::{
    LinuxBuilder, LinuxCapabilitiesBuilder, LinuxNamespace, LinuxNamespaceBuilder,
    LinuxNamespaceType, LinuxPidsBuilder, LinuxResourcesBuilder, Mount, MountBuilder, PosixRlimit,
    PosixRlimitBuilder, PosixRlimitType, ProcessBuilder, RootBuilder, Spec, SpecBuilder,
    UserBuilder,
};

use super::hardening::{
    CAPABILITIES, MASKED_PATHS, READONLY_PATHS, SeccompError, held_capabilities, seccomp,
};
use crate::container::{ContainerSpec, EXEC_ROOT};
use crate::subids::DaemonIds;

/// The OCI version the file declares (what `crun spec` writes).
const OCI_VERSION: &str = "1.0.0";

/// The hostname of every action's container, the same on every node.
pub(crate) const HOSTNAME: &str = "localhost";

/// The lease directories an action's container writes to, and the destination of each:
/// disk-backed (a `tmpfs` would count against the lease's memory), owned by the
/// container's root, discarded with the lease. The image's root is read-only; these are
/// the places an action writes besides `/kbf/root`.
pub(crate) const WRITABLE: [(&str, &str); 4] = [
    ("tmp", "/tmp"),
    ("var-tmp", "/var/tmp"),
    ("run", "/run"),
    ("home", "/root"),
];

/// The directory of the lease holding [`WRITABLE`]'s directories.
pub(crate) const WRITABLE_DIR: &str = "rw";

/// The mount options of the exec-root overlay, pinned: no metacopy, redirects only
/// recorded and never followed (`nofollow`), no index or xino (a stub or redirect planted
/// in the upper layer cannot change what collection reads), and `userxattr` because the
/// overlay is mounted inside a user namespace. The kernel refuses `userxattr` together
/// with `redirect_dir=off` or `on` ("conflicting options"): `nofollow` is the one
/// redirect mode `userxattr` takes, and what it implies.
pub(crate) const OVERLAY_OPTIONS: &str =
    "userxattr,metacopy=off,redirect_dir=nofollow,index=off,xino=off";

/// Why a configuration could not be built.
#[derive(Debug, thiserror::Error)]
pub(crate) enum SpecError {
    #[error(transparent)]
    Seccomp(#[from] SeccompError),
    #[error("config.json: {0}")]
    Json(#[from] serde_json::Error),
}

/// What a builder of `oci-spec` makes. Every type here has a default for each field, so
/// `build()` cannot fail; the default is only what a failure would give.
pub(crate) fn built<T: Default>(made: Result<T, OciSpecError>) -> T {
    made.unwrap_or_default()
}

pub(crate) fn strings(options: &[&str]) -> Vec<String> {
    options.iter().map(|s| (*s).to_owned()).collect()
}

/// A bind mount of `source` at `destination`.
pub(crate) fn bind(source: &Path, destination: &str, options: &[&str]) -> Mount {
    built(
        MountBuilder::default()
            .destination(destination)
            .typ("bind")
            .source(source)
            .options(strings(options))
            .build(),
    )
}

/// A mount of the filesystem `typ` at `destination`.
pub(crate) fn filesystem(destination: &str, typ: &str, options: &[&str]) -> Mount {
    built(
        MountBuilder::default()
            .destination(destination)
            .typ(typ)
            .source(typ)
            .options(strings(options))
            .build(),
    )
}

/// Where a lease's container is made and what it sees of the node.
pub(crate) struct Placement<'a> {
    /// The unpacked image the container's root is.
    pub rootfs: &'a Path,
    /// The lease's cgroup, as the cgroup filesystem names it (`/.../kbf-lease-<n>`);
    /// the container's own cgroup is a child of it.
    pub cgroup: &'a str,
    /// The daemon user's ids and the subordinate ranges the container maps to.
    pub ids: &'a DaemonIds,
    /// Go's name for the node's architecture, for the seccomp profile.
    pub arch: &'a str,
}

/// The mounts of an action: the standard ones of `crun spec --rootless`, the hosts file,
/// the exec-root overlay, the shared trees read-only inside it, and [`WRITABLE`].
fn mounts(spec: &ContainerSpec) -> Vec<Mount> {
    let shm = format!("size={}m", spec.limits.shm_mib);
    let open = ["rbind", "ro", "nosuid", "noexec", "nodev"];
    let mut mounts = vec![
        filesystem("/proc", "proc", &["nosuid", "noexec", "nodev"]),
        filesystem(
            "/dev",
            "tmpfs",
            &["nosuid", "strictatime", "mode=755", "size=65536k"],
        ),
        filesystem(
            "/dev/pts",
            "devpts",
            &[
                "nosuid",
                "noexec",
                "newinstance",
                "ptmxmode=0666",
                "mode=0620",
            ],
        ),
        built(
            MountBuilder::default()
                .destination("/dev/shm")
                .typ("tmpfs")
                .source("shm")
                .options(strings(&["nosuid", "noexec", "nodev", "mode=1777", &shm]))
                .build(),
        ),
        filesystem("/dev/mqueue", "mqueue", &["nosuid", "noexec", "nodev"]),
        filesystem("/sys", "sysfs", &["nosuid", "noexec", "nodev", "ro"]),
        filesystem(
            "/sys/fs/cgroup",
            "cgroup",
            &["nosuid", "noexec", "nodev", "relatime", "ro"],
        ),
        bind(&spec.dir.join("etc/hosts"), "/etc/hosts", &open),
        bind(&spec.dir.join("etc/hostname"), "/etc/hostname", &open),
    ];
    let overlay = format!(
        "lowerdir={},upperdir={},workdir={},{OVERLAY_OPTIONS}",
        spec.input_root.display(),
        spec.upper.display(),
        spec.overlay_work.display(),
    );
    mounts.push(built(
        MountBuilder::default()
            .destination(EXEC_ROOT)
            .typ("overlay")
            .source("overlay")
            .options(vec![overlay, "nosuid".to_owned(), "nodev".to_owned()])
            .build(),
    ));
    // After the overlay, which they are mounted inside: each read-only, with the flags
    // a child user namespace cannot relax.
    for shared in &spec.shared {
        let destination = format!("{EXEC_ROOT}/{}", shared.path);
        mounts.push(bind(
            &shared.source,
            &destination,
            &["rbind", "ro", "nosuid", "nodev"],
        ));
    }
    for (name, destination) in WRITABLE {
        let source = spec.dir.join(WRITABLE_DIR).join(name);
        mounts.push(bind(
            &source,
            destination,
            &["rbind", "rw", "nosuid", "nodev"],
        ));
    }
    mounts
}

fn rlimit(typ: PosixRlimitType, value: u64) -> PosixRlimit {
    built(
        PosixRlimitBuilder::default()
            .typ(typ)
            .hard(value)
            .soft(value)
            .build(),
    )
}

/// The namespaces a container gets, each new (the network namespace has loopback only).
pub(crate) fn namespaces() -> Vec<LinuxNamespace> {
    use LinuxNamespaceType::{Cgroup, Ipc, Mount, Network, Pid, User, Uts};
    [Pid, Network, Ipc, Uts, User, Cgroup, Mount]
        .into_iter()
        .map(|typ| built(LinuxNamespaceBuilder::default().typ(typ).build()))
        .collect()
}

/// The process environment: the Command's variables, then `HOSTNAME` and `HOME` unless
/// it sets them: `HOSTNAME=localhost` and the home of uid 0, `/root`, which are the same
/// on every node.
pub(crate) fn environment(env: &[(String, String)]) -> Vec<String> {
    let mut out: Vec<String> = env.iter().map(|(n, v)| format!("{n}={v}")).collect();
    for (name, default) in [("HOSTNAME", HOSTNAME), ("HOME", "/root")] {
        if !env.iter().any(|(n, _)| n == name) {
            out.push(format!("{name}={default}"));
        }
    }
    out
}

/// The `config.json` of the container for `spec`.
///
/// | Isolation                                | `config.json`                                     |
/// |------------------------------------------|---------------------------------------------------|
/// | no network                               | a new network namespace, nothing configured (crun brings `lo` up) |
/// | no id of the daemon user, root inside    | `uidMappings`/`gidMappings` container `0..65536` to the daemon user's first subordinate range; `process.user` 0:0 |
/// | only the Command's environment           | `process.env` is the Command's variables only      |
/// | the Command's argv and directory         | `process.args`, `process.cwd`                      |
/// | the lease cgroup, `memory.oom.group=1`   | `cgroupsPath` a child of the lease cgroup, `unified` `memory.oom.group`       |
/// | a pids limit                             | `resources.pids.limit`                             |
/// | `nofile` and `nproc` ulimits             | `process.rlimits`                                  |
/// | `/dev/shm` size                          | the `/dev/shm` tmpfs `size=`                       |
/// | the kernel's default OOM score           | `process.oomScoreAdj`                              |
/// | hostname `localhost`                     | `hostname`                                         |
/// | the input root, writes in `upper`        | an `overlay` mount at `/kbf/root`                  |
///
/// No memory or CPU limit is set here: the lease cgroup carries them.
///
/// # Errors
/// The seccomp profile cannot be resolved for the node's architecture.
pub(crate) fn action_config(
    spec: &ContainerSpec,
    placement: &Placement<'_>,
) -> Result<Spec, SpecError> {
    let held = held_capabilities();
    let capabilities = built(
        LinuxCapabilitiesBuilder::default()
            .bounding(held.clone())
            .effective(held.clone())
            .permitted(held)
            .inheritable(HashSet::new())
            .ambient(HashSet::new())
            .build(),
    );
    let cwd = if spec.working_directory.is_empty() {
        EXEC_ROOT.to_owned()
    } else {
        format!("{EXEC_ROOT}/{}", spec.working_directory)
    };
    let process = built(
        ProcessBuilder::default()
            .terminal(false)
            .user(built(UserBuilder::default().uid(0u32).gid(0u32).build()))
            .args(spec.argv.clone())
            .env(environment(&spec.env))
            .cwd(PathBuf::from(cwd))
            .capabilities(capabilities)
            .rlimits(vec![
                rlimit(PosixRlimitType::RlimitNofile, spec.limits.nofile),
                rlimit(PosixRlimitType::RlimitNproc, spec.limits.nproc),
            ])
            .no_new_privileges(false)
            .oom_score_adj(0)
            .build(),
    );
    let (uids, gids) = placement.ids.action_mappings();
    let pids = built(
        LinuxPidsBuilder::default()
            .limit(i64::try_from(spec.limits.pids).unwrap_or(i64::MAX))
            .build(),
    );
    let unified = HashMap::from([("memory.oom.group".to_owned(), "1".to_owned())]);
    let resources = built(
        LinuxResourcesBuilder::default()
            .pids(pids)
            .unified(unified)
            .build(),
    );
    let linux = built(
        LinuxBuilder::default()
            .uid_mappings(uids)
            .gid_mappings(gids)
            .namespaces(namespaces())
            .cgroups_path(PathBuf::from(format!("{}/ctr", placement.cgroup)))
            .resources(resources)
            .seccomp(seccomp(placement.arch, &CAPABILITIES)?)
            .masked_paths(strings(&MASKED_PATHS))
            .readonly_paths(strings(&READONLY_PATHS))
            .build(),
    );
    let root = built(
        RootBuilder::default()
            .path(placement.rootfs)
            .readonly(true)
            .build(),
    );
    Ok(built(
        SpecBuilder::default()
            .version(OCI_VERSION)
            .root(root)
            .hostname(HOSTNAME)
            .process(process)
            .mounts(mounts(spec))
            .linux(linux)
            .build(),
    ))
}

/// The bytes of `config.json`: the spec as pretty JSON with keys in sorted order and
/// each capability set sorted (`oci-spec` holds them in hash sets, whose order changes
/// from run to run), and a newline: the same bytes for the same spec, so a golden file
/// can pin them.
///
/// # Errors
/// The spec cannot be written as JSON (it can always).
pub(crate) fn render(config: &Spec) -> Result<Vec<u8>, SpecError> {
    let mut value = serde_json::to_value(config)?;
    value["process"]["capabilities"]
        .as_object_mut()
        .into_iter()
        .flat_map(|sets| sets.values_mut())
        .filter_map(|set| set.as_array_mut())
        .for_each(|set| set.sort_by(|a, b| a.as_str().cmp(&b.as_str())));
    let mut bytes = serde_json::to_vec_pretty(&value)?;
    bytes.push(b'\n');
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use serde_json::{Value, json};

    use super::*;
    use crate::container::tests::spec;
    use kbf_daemon::trees::SharedMount;

    fn ids() -> DaemonIds {
        DaemonIds {
            uid: 990,
            gid: 991,
            sub_uid: 100_000,
            sub_gid: 200_000,
        }
    }

    fn config_of(spec: &ContainerSpec) -> Value {
        let rootfs = PathBuf::from("/scratch/oci-images/aaaa/rootfs");
        let placement = Placement {
            rootfs: &rootfs,
            cgroup: "/kbf.slice/actions/kbf-lease-1-2",
            ids: &ids(),
            arch: "amd64",
        };
        let config = action_config(spec, &placement).expect("config");
        serde_json::from_slice(&render(&config).expect("render")).expect("json")
    }

    /// The whole `config.json` of the reference action, as a file: any field added,
    /// dropped or changed (a limit, a mount option, a namespace, the seccomp profile)
    /// is a diff to read (the seccomp entries are counted, not listed). Regenerate by
    /// replacing the file with the actual text the failure prints.
    #[test]
    fn the_config_matches_its_golden_file() {
        let mut config = config_of(&spec());
        // The filter's several hundred entries are pinned by `hardening`'s own tests;
        // the file shows how many there are and everything around them.
        let seccomp = &mut config["linux"]["seccomp"];
        let entries = seccomp["syscalls"].as_array().expect("syscalls").len();
        seccomp["syscalls"] = json!(format!("{entries} entries"));
        let actual = serde_json::to_string_pretty(&config).expect("json") + "\n";
        let golden = include_str!("../../tests/fixtures/oci-action-config.golden.json");
        assert!(
            actual == golden,
            "config.json differs from tests/fixtures/oci-action-config.golden.json; actual:\n{actual}"
        );
    }

    /// Catches `config.json` bytes that change between runs for the same spec: the
    /// capability sets are hash sets in `oci-spec`, so each build orders them
    /// differently unless `render` sorts them.
    #[test]
    fn the_bytes_are_the_same_every_time() {
        let rootfs = PathBuf::from("/scratch/oci-images/aaaa/rootfs");
        let build = || {
            let placement = Placement {
                rootfs: &rootfs,
                cgroup: "/kbf.slice/actions/kbf-lease-1-2",
                ids: &ids(),
                arch: "amd64",
            };
            render(&action_config(&spec(), &placement).expect("config")).expect("render")
        };
        let first = build();
        for _ in 0..30 {
            assert_eq!(build(), first);
        }
    }

    /// Catches a shared tree mounted writable, mounted outside the exec root, or mounted
    /// before the overlay it sits in (the overlay would hide it).
    #[test]
    fn shared_mounts_are_read_only_inside_the_overlay() {
        let mut spec = spec();
        spec.shared = vec![
            SharedMount {
                source: PathBuf::from("/scratch/trees/aa"),
                path: "external/toolchain".to_owned(),
            },
            SharedMount {
                source: PathBuf::from("/scratch/trees/bb"),
                path: "lib".to_owned(),
            },
        ];
        let config = config_of(&spec);
        let mounts = config["mounts"].as_array().expect("mounts");
        let at = |dest: &str| mounts.iter().position(|m| m["destination"] == dest);
        let overlay = at("/kbf/root").expect("overlay");
        for (dest, source) in [
            ("/kbf/root/external/toolchain", "/scratch/trees/aa"),
            ("/kbf/root/lib", "/scratch/trees/bb"),
        ] {
            let position = at(dest).expect("a mount of the shared tree");
            assert!(overlay < position, "{dest} before the overlay");
            let mount = &mounts[position];
            assert_eq!(mount["source"], source);
            assert_eq!(mount["type"], "bind");
            let options: Vec<_> = mount["options"]
                .as_array()
                .expect("options")
                .iter()
                .map(|o| o.as_str().expect("str"))
                .collect();
            assert!(options.contains(&"ro"), "{dest}: {options:?}");
            assert!(options.contains(&"nosuid"));
            assert!(options.contains(&"nodev"));
            assert!(!options.contains(&"rw"), "{dest}: {options:?}");
        }
        // No shared mount, no extra mount: the reference config has none.
        assert!(
            config_of(&self::spec())["mounts"]
                .as_array()
                .expect("mounts")
                .iter()
                .all(|m| !m["destination"].as_str().unwrap().starts_with("/kbf/root/"))
        );
    }

    /// Catches the Command's variables being reordered or dropped, and the two defaults
    /// being added when the Command sets them (or not added when it does not).
    #[test]
    fn the_environment_is_the_commands_then_the_two_defaults() {
        let var = |n: &str, v: &str| (n.to_owned(), v.to_owned());
        assert_eq!(environment(&[]), ["HOSTNAME=localhost", "HOME=/root"]);
        assert_eq!(
            environment(&[var("B", "2"), var("A", "1")]),
            ["B=2", "A=1", "HOSTNAME=localhost", "HOME=/root"]
        );
        assert_eq!(
            environment(&[var("HOME", "/w")]),
            ["HOME=/w", "HOSTNAME=localhost"]
        );
        assert_eq!(
            environment(&[var("HOSTNAME", "h"), var("HOME", "/w")]),
            ["HOSTNAME=h", "HOME=/w"]
        );
    }

    /// Catches the daemon user's id mapped into the container (any host id of a mapping
    /// equal to or spanning it), the mapping shrinking below a full 16-bit range, and
    /// the group mapping using the user range.
    #[test]
    fn the_container_maps_the_subordinate_range_only() {
        let ids = ids();
        let (uids, gids) = ids.action_mappings();
        let spans = |maps: &[oci_spec::runtime::LinuxIdMapping]| {
            maps.iter()
                .map(|m| (m.container_id(), m.host_id(), m.size()))
                .collect::<Vec<_>>()
        };
        assert_eq!(spans(&uids), [(0, 100_000, 65_536)]);
        assert_eq!(spans(&gids), [(0, 200_000, 65_536)]);
        // A helper's namespace maps the daemon user as its root and the range after it.
        let (uids, gids) = ids.setup_mappings();
        assert_eq!(spans(&uids), [(0, 990, 1), (1, 100_000, 65_536)]);
        assert_eq!(spans(&gids), [(0, 991, 1), (1, 200_000, 65_536)]);
    }

    /// Catches the container's cgroup not being below the lease's (its limits would not
    /// apply) and the memory-or-CPU limits being set on the container instead of the lease.
    #[test]
    fn the_cgroup_is_a_child_of_the_lease_with_no_memory_or_cpu_limit() {
        let config = config_of(&spec());
        assert_eq!(
            config["linux"]["cgroupsPath"],
            "/kbf.slice/actions/kbf-lease-1-2/ctr"
        );
        let resources = &config["linux"]["resources"];
        assert_eq!(resources["pids"], json!({"limit": 101}));
        assert!(resources.get("memory").is_none());
        assert!(resources.get("cpu").is_none());
    }
}
