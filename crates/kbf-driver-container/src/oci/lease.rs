//! What the lease code asks of [`Oci`], answered with a bundle, `crun run` and helper
//! containers: whether the image is usable, who may write the lease's directories,
//! making and starting the container, how it ended, signalling it, removing what it left.

use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;

use super::crun::{RunEnd, log_path};
use super::{JobError, Mapping, Oci, at, helper, spec};
use crate::container::{CONTAINER_OWNER, ContainerSpec, Dirs, Ended};
use crate::image::{ImageCheckError, ImageRef};
use crate::program::{Said, crun_words};
use crate::running::Running;

/// The mode of the lease directory and of the small files in it the container reads:
/// the container's ids are not the daemon user's, so they reach them as "others".
const OPEN_DIR: u32 = 0o755;
const OPEN_FILE: u32 = 0o644;

/// The `hosts` file of every action's container: loopback only, as `--network=none`.
const HOSTS: &str = "127.0.0.1 localhost\n::1 localhost\n";

/// Writes `bytes` to `path`, readable by others.
async fn write_open(path: &Path, bytes: &[u8]) -> Result<(), JobError> {
    at(path, tokio::fs::write(path, bytes).await)?;
    let mode = std::fs::Permissions::from_mode(OPEN_FILE);
    at(path, tokio::fs::set_permissions(path, mode).await)
}

/// Makes `path` (and its parents), searchable by others.
async fn open_dir(path: &Path) -> Result<(), JobError> {
    at(path, tokio::fs::create_dir_all(path).await)?;
    let mode = std::fs::Permissions::from_mode(OPEN_DIR);
    at(path, tokio::fs::set_permissions(path, mode).await)
}

impl Oci {
    /// The directories of `dirs` to hand over and the `rw` directory among them.
    async fn hand_over(&self, owner: &str, dirs: &Dirs<'_>) -> Result<(), JobError> {
        let rw = dirs.lease.join(spec::WRITABLE_DIR);
        // The directories are the daemon's to make and open until they are given to the
        // container; given back, they are the container's ids' until the clean.
        if owner == CONTAINER_OWNER {
            open_dir(dirs.lease).await?;
            open_dir(&rw).await?;
            for (name, _) in spec::WRITABLE {
                open_dir(&rw.join(name)).await?;
            }
        }
        let mut whole = vec![dirs.upper, dirs.work, rw.as_path()];
        // With the cache the input root's files are links to its entries: only its
        // directories are given. Without, everything.
        let only_dirs = dirs.helper.is_some().then_some(dirs.root);
        if only_dirs.is_none() {
            whole.insert(0, dirs.root);
        }
        let job = helper::give_job(&self.daemon, owner, only_dirs, &whole);
        self.run_jobs("give", Mapping::Setup, &[job]).await
    }

    /// Writes the bundle of `spec`: `config.json` and the `hosts` and `hostname` files it
    /// binds.
    async fn write_bundle(&self, spec: &ContainerSpec) -> Result<(), JobError> {
        open_dir(&spec.dir).await?;
        let etc = spec.dir.join("etc");
        open_dir(&etc).await?;
        write_open(&etc.join("hosts"), HOSTS.as_bytes()).await?;
        let hostname = format!("{}\n", spec::HOSTNAME);
        write_open(&etc.join("hostname"), hostname.as_bytes()).await?;
        let rootfs = self.images.rootfs_of(&spec.image_digest);
        let placement = spec::Placement {
            rootfs: &rootfs,
            cgroup: &spec.cgroup_parent,
            ids: &self.ids,
            arch: self.arch,
        };
        let bytes = spec::render(&spec::action_config(spec, &placement)?)?;
        let bundle = spec.dir.join("bundle");
        at(&bundle, tokio::fs::create_dir_all(&bundle).await)?;
        let path = bundle.join("config.json");
        at(&path, tokio::fs::write(&path, bytes).await)
    }
}

impl Oci {
    /// The image is unpacked, once per digest and off the action path; a lease finds it
    /// there with one `stat`.
    pub(crate) async fn check_pinned(&self, image: &ImageRef) -> Result<(), ImageCheckError> {
        self.images.ensure(self, image).await
    }

    /// One helper container runs this daemon's `__chown-dirs` mode over the directories
    /// (the lease's `rw` directory, which holds `/tmp` and the other places the action
    /// writes, among them).
    pub(crate) async fn give(&self, owner: &str, dirs: &Dirs<'_>) -> Result<(), String> {
        Ok(self.hand_over(owner, dirs).await?)
    }

    pub(crate) async fn create(&self, spec: &ContainerSpec) -> Result<(), String> {
        Ok(self.write_bundle(spec).await?)
    }

    /// `crun run` on a thread of its own; it ends when the container does.
    pub(crate) fn start(
        &self,
        spec: &ContainerSpec,
        stdout: std::fs::File,
        stderr: std::fs::File,
    ) -> Result<Running, String> {
        let crun = self.crun.clone();
        let (dir, name) = (spec.dir.clone(), spec.name.clone());
        let bundle = dir.join("bundle");
        let thread = format!("crun-{name}");
        let run = async move { crun.run(&dir, &name, &bundle, stdout, stderr).await };
        Running::spawn(thread, run)
    }

    /// `crun run`'s exit status is the action's, unless `crun` itself failed: told by an
    /// error record in its log.
    pub(crate) async fn ended(
        &self,
        spec: &ContainerSpec,
        running: &Running,
    ) -> Result<Ended, String> {
        match running.run_end() {
            RunEnd::Exited(0) => Ok(Ended::Exited(0)),
            RunEnd::Exited(code) => {
                let log = tokio::fs::read_to_string(log_path(&spec.dir)).await;
                if crun_words(&log.unwrap_or_default()).is_empty() {
                    Ok(Ended::Exited(code))
                } else {
                    Ok(Ended::NotRun(format!("exited {code}")))
                }
            }
            RunEnd::Failed(why) => Err(why),
        }
    }

    pub(crate) fn said(&self, spec: &ContainerSpec, stderr: &Path) -> Said {
        Said::crun(&log_path(&spec.dir), stderr)
    }

    pub(crate) async fn terminate(&self, name: &str) -> Result<(), String> {
        self.crun.kill(&self.lease(name), name, 15).await
    }

    /// `crun delete --force`: nothing to do when `crun run` ended normally (it deletes
    /// its container as it exits), and the removal of the container, its cgroup and
    /// its state when it did not. Always run rather than guessed from the state
    /// directory's layout, which is `crun`'s to change.
    pub(crate) fn remove(&self, name: &str) -> Result<(), String> {
        self.crun.delete_blocking(&self.lease(name), name)
    }

    pub(crate) fn remove_dir(&self, dir: &Path) -> Result<(), String> {
        Ok(self.remove_dir_blocking(dir)?)
    }
}
