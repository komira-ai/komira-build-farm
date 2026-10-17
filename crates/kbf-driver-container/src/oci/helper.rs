//! Short-lived helper containers: the "setup namespace" work, done by `crun` itself.
//!
//! A directory the daemon user owns can be given to the action's root (a subordinate
//! id), read back, or deleted once it holds files owned by subordinate ids, only from
//! inside a user namespace that maps both the daemon user and the subordinate range.
//! A helper is that namespace: container id 0 is the daemon user, container ids
//! `1..=65536` are the subordinate range ([`DaemonIds::setup_mappings`]), so an action's
//! root (host subordinate id 0) is helper id 1, the `1:1` it is given directories as.
//! `crun` creates it; AppArmor's grant for user namespaces is `crun`'s own profile,
//! so the daemon needs no profile of its own.
//!
//! A helper runs one program, never a shell: this daemon's own binary in its
//! `__chown-dirs` mode (descriptor-relative, never following a symlink) or its
//! `__unpack-layers` mode, or the node's `find`, from a read-only bind of the host's
//! `/usr`. Its
//! root is a small directory of mount points ([`prepare_rootfs`]) shared by all helpers.
//! The directories it works on are bound at `/t/0`, `/t/1`, ... so no per-lease path
//! is ever created in the shared root.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use oci_spec::runtime::{
    LinuxBuilder, LinuxCapabilitiesBuilder, LinuxNamespaceBuilder, LinuxNamespaceType,
    ProcessBuilder, RootBuilder, Spec, SpecBuilder, UserBuilder,
};

use super::Mapping;
use super::hardening::held_capabilities;
use super::spec::{bind, built, filesystem};
use crate::subids::DaemonIds;

/// How many directories the helper root has mount points for (`/t/0` to `/t/7`); more
/// work too, with the mount point made inside the shared root by `crun`.
const BIND_POINTS: usize = 8;

/// Where the daemon's own binary is bound, for `__chown-dirs`.
const DAEMON_BINARY: &str = "/kbf/daemon";

/// The job that gives `whole` trees (every entry) and `dirs` (directories only, if any)
/// to `owner` (`uid:gid` in the helper's namespace), with this daemon's binary `daemon`.
pub(crate) fn give_job(daemon: &Path, owner: &str, dirs: Option<&Path>, whole: &[&Path]) -> Job {
    let mut binds = Vec::new();
    let mut args = vec![
        DAEMON_BINARY.to_owned(),
        crate::chown_dirs::ARG.to_owned(),
        owner.to_owned(),
    ];
    args.push(match dirs {
        Some(dir) => {
            binds.push(Bind {
                source: dir.to_owned(),
                writable: true,
            });
            bound_at(0)
        }
        None => "-".to_owned(),
    });
    args.push(crate::chown_dirs::WHOLE.to_owned());
    for tree in whole {
        args.push(bound_at(binds.len()));
        binds.push(Bind {
            source: (*tree).to_owned(),
            writable: true,
        });
    }
    Job {
        binds,
        daemon: Some(daemon.to_owned()),
        args,
    }
}

/// The job that deletes what is below `dir` (not `dir`).
pub(crate) fn remove_job(dir: &Path) -> Job {
    Job {
        binds: vec![Bind {
            source: dir.to_owned(),
            writable: true,
        }],
        daemon: None,
        args: ["/usr/bin/find", "/t/0", "-mindepth", "1", "-delete"]
            .map(str::to_owned)
            .to_vec(),
    }
}

/// The job that unpacks the layer files `layers` of the directory `blobs` into
/// `stage/rootfs` ([`crate::unpack_layers`]), with this daemon's binary `daemon`, in a
/// container with an action's user namespace (so the files are owned by the ids the
/// action sees). `stage` is open to the container's root, which makes the root
/// directory in it; `blobs` is read-only.
pub(crate) fn unpack_job(daemon: &Path, stage: &Path, blobs: &Path, layers: &[String]) -> Job {
    let mut args = vec![
        DAEMON_BINARY.to_owned(),
        crate::unpack_layers::ARG.to_owned(),
        bound_at(0),
        bound_at(1),
    ];
    args.extend(layers.iter().cloned());
    Job {
        binds: vec![
            Bind {
                source: stage.to_owned(),
                writable: true,
            },
            Bind {
                source: blobs.to_owned(),
                writable: false,
            },
        ],
        daemon: Some(daemon.to_owned()),
        args,
    }
}

/// A directory bound into a helper.
pub(crate) struct Bind {
    pub source: PathBuf,
    pub writable: bool,
}

/// What a helper runs: the directories it works on and the command.
pub(crate) struct Job {
    pub binds: Vec<Bind>,
    /// The daemon's binary, bound read-only at [`DAEMON_BINARY`], if the job runs it.
    pub daemon: Option<PathBuf>,
    pub args: Vec<String>,
}

/// The path `binds[i]` appears at inside a helper.
pub(crate) fn bound_at(index: usize) -> String {
    format!("/t/{index}")
}

/// A read-only or read-write bind of `source` at `destination`.
fn bind_at(source: &Path, destination: &str, writable: bool) -> oci_spec::runtime::Mount {
    let access = if writable { "rw" } else { "ro" };
    bind(source, destination, &["rbind", access, "nosuid"])
}

/// The `config.json` of a helper running `job` over `rootfs` (see [`prepare_rootfs`]),
/// in the user namespace `mapping` names.
pub(crate) fn config(
    rootfs: &Path,
    ids: &DaemonIds,
    mapping: Mapping,
    cgroup: &str,
    job: &Job,
) -> Spec {
    let mut mounts = vec![
        filesystem(
            "/dev",
            "tmpfs",
            &["nosuid", "strictatime", "mode=755", "size=1m"],
        ),
        bind_at(Path::new("/usr"), "/usr", false),
        bind_at(Path::new("/etc/ld.so.cache"), "/etc/ld.so.cache", false),
    ];
    if let Some(daemon) = &job.daemon {
        mounts.push(bind_at(daemon, DAEMON_BINARY, false));
    }
    for (i, bind) in job.binds.iter().enumerate() {
        mounts.push(bind_at(&bind.source, &bound_at(i), bind.writable));
    }
    let held = held_capabilities();
    let capabilities = built(
        LinuxCapabilitiesBuilder::default()
            .bounding(held.clone())
            .effective(held.clone())
            .permitted(held)
            // Left out, a set is `oci-spec`'s default (AUDIT_WRITE, KILL, NET_BIND_SERVICE):
            // an ambient capability the permitted set lacks, which `capset` refuses.
            .inheritable(HashSet::new())
            .ambient(HashSet::new())
            .build(),
    );
    let process = built(
        ProcessBuilder::default()
            .terminal(false)
            .user(built(UserBuilder::default().uid(0u32).gid(0u32).build()))
            .args(job.args.clone())
            .env(vec!["PATH=/usr/bin:/usr/sbin:/bin:/sbin".to_owned()])
            .cwd(PathBuf::from("/"))
            .capabilities(capabilities)
            .no_new_privileges(true)
            .build(),
    );
    let (uids, gids) = ids.mappings(mapping);
    let namespaces = [LinuxNamespaceType::User, LinuxNamespaceType::Mount]
        .map(|typ| built(LinuxNamespaceBuilder::default().typ(typ).build()));
    let mut linux = built(
        LinuxBuilder::default()
            .uid_mappings(uids)
            .gid_mappings(gids)
            .namespaces(namespaces.to_vec())
            .cgroups_path(PathBuf::from(cgroup))
            .masked_paths(Vec::<String>::new())
            .readonly_paths(Vec::<String>::new())
            .build(),
    );
    // `oci-spec` fills what a builder leaves out with its own defaults (a resources
    // section, a hostname, annotations); a helper has no cgroup, no UTS namespace to
    // set a hostname in, and nothing to mask.
    linux.set_resources(None);
    let root = built(RootBuilder::default().path(rootfs).readonly(false).build());
    let mut spec = built(
        SpecBuilder::default()
            .version("1.0.0")
            .root(root)
            .process(process)
            .mounts(mounts)
            .linux(linux)
            .build(),
    );
    spec.set_hostname(None);
    spec.set_annotations(None);
    spec
}

/// The links the helper root holds so the programs and libraries of a merged-`/usr`
/// host are where their paths say (`/bin/sh`-style paths and the loader's): each into
/// the read-only bind of the host's `/usr`. A host whose `/bin` or `/lib` is a real
/// directory is not supported.
const USR_LINKS: [(&str, &str); 4] = [
    ("bin", "usr/bin"),
    ("sbin", "usr/sbin"),
    ("lib", "usr/lib"),
    ("lib64", "usr/lib64"),
];

/// Makes the helper root `dir`: the mount points of [`config`] and the links of
/// [`USR_LINKS`]. Safe to call again and from several leases at once (a mount point
/// or link that exists is left as it is).
///
/// # Errors
/// A directory or link cannot be made.
pub(crate) fn prepare_rootfs(dir: &Path) -> std::io::Result<()> {
    for point in ["dev", "usr", "etc", "kbf"] {
        std::fs::create_dir_all(dir.join(point))?;
    }
    for i in 0..BIND_POINTS {
        std::fs::create_dir_all(dir.join(bound_at(i).trim_start_matches('/')))?;
    }
    for file in ["etc/ld.so.cache", "kbf/daemon"] {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join(file))?;
    }
    for (name, target) in USR_LINKS {
        match std::os::unix::fs::symlink(target, dir.join(name)) {
            Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => return Err(e),
            _ => {}
        }
    }
    Ok(())
}

/// Copies the daemon's binary to `<scratch>/oci-bin/kbf-daemon` (mode 0755, replaced
/// atomically) and returns that path. A helper with an action's ids cannot search the
/// directories the binary was installed or built under (a home directory is 0750), but the
/// scratch directory was checked to be searchable by others.
pub(crate) fn install_daemon(scratch: &Path, daemon: &Path) -> std::io::Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = scratch.join("oci-bin");
    std::fs::create_dir_all(&dir)?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755))?;
    let staged = dir.join(format!(".kbf-daemon-{}", std::process::id()));
    std::fs::copy(daemon, &staged)?;
    std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))?;
    let target = dir.join("kbf-daemon");
    std::fs::rename(&staged, &target)?;
    Ok(target)
}
