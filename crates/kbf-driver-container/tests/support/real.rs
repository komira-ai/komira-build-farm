//! One real-`crun` test's cgroup, scratch directory and runtime ([`Cell`]), shared by
//! the test binaries that run the driver against rootless `crun`
//! (`cargo xtask podman-tests` sets up what they need). The image is fetched from its
//! registry by the driver's own image store, once per machine: every cell's
//! `oci-images` is the one store, which the first cell fills.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Duration;

use kbf_daemon::{Runtime, RuntimeError, Work};
use kbf_driver_container::{MemoryCas, PodmanConfig, PodmanRuntime};
use kbf_proto::reapi::{ActionResult, Digest};
use kbf_types::{LeaseId, Resources};

use super::{Spec, blob, exists, store_action, work};

pub fn var(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| {
        panic!("{name} is not set: run these tests through cargo xtask podman-tests")
    })
}

/// One test's cgroup parent (under the delegated cgroup), scratch and runtime.
pub struct Cell {
    /// The lease term this test's leases use: each test gets its own, so the leases of
    /// two cells never share a name.
    pub term: u64,
    pub cgroup: PathBuf,
    pub scratch: PathBuf,
    pub config: PodmanConfig,
    pub cas: Arc<MemoryCas>,
    pub runtime: Arc<PodmanRuntime<MemoryCas>>,
}

static TERMS: AtomicU64 = AtomicU64::new(1);

/// The cgroup v2 mount.
pub const MOUNT: &str = "/sys/fs/cgroup";
/// What the test setup writes to `actions/memory.max`: room for every test here, and
/// below a hosted runner's memory, so the capacity test sees the cap and not MemTotal.
pub const ACTIONS_MEMORY_MAX: u64 = 4 << 30;
/// What the test setup writes to `supervisor/memory.min`: 256 MiB, `kbf-daemon`'s
/// default.
pub const SUPERVISOR_MEMORY_MIN: u64 = 256 << 20;

/// The unit's cgroup, set up by the code `kbf-daemon` runs, once per test process.
/// Each test binary runs in a unit of its own (`cargo xtask podman-tests`), so each
/// sets up a fresh one; a second start from the `supervisor/` leaf is tested on a fake
/// cgroup tree (`src/delegate_tests.rs`).
pub fn delegation() -> &'static kbf_driver_container::Delegation {
    static DELEGATION: OnceLock<kbf_driver_container::Delegation> = OnceLock::new();
    DELEGATION.get_or_init(|| {
        let own = std::fs::read_to_string("/proc/self/cgroup").expect("read /proc/self/cgroup");
        kbf_driver_container::delegate(
            Path::new(MOUNT),
            &own,
            SUPERVISOR_MEMORY_MIN,
            Some(ACTIONS_MEMORY_MAX),
        )
        .unwrap_or_else(|e| {
            panic!("set up the delegated cgroup (run through cargo xtask podman-tests): {e}")
        })
    })
}

/// Each cgroup under `dir` with the processes in it, for a failure message.
pub fn describe(dir: &Path) -> String {
    let mut out = String::new();
    let mut pending = vec![dir.to_owned()];
    while let Some(d) = pending.pop() {
        let procs = std::fs::read_to_string(d.join("cgroup.procs")).unwrap_or_default();
        let commands: Vec<String> = procs
            .split_whitespace()
            .map(|pid| {
                std::fs::read_to_string(format!("/proc/{pid}/cmdline"))
                    .unwrap_or_default()
                    .replace('\0', " ")
            })
            .collect();
        out.push_str(&format!("\n  {}: {commands:?}", d.display()));
        for entry in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                pending.push(entry.path());
            }
        }
    }
    out
}

/// Shows the driver's logs (its clean errors among them) in a failing test's output.
fn trace() {
    super::swap_kill::trace();
}

/// One cell at a time is made, and the image store it shares is filled by the first:
/// a cell's start-up sweeps the store for what a killed daemon left, which must not
/// find another cell's unpack in progress.
static MAKING: Mutex<()> = Mutex::new(());

/// The image store every cell shares, under the system temporary directory (whose
/// ancestors every id can search, which the backend's containers need). It is kept
/// between runs: its files are owned by subordinate ids, and an image is read-only.
fn shared_images() -> PathBuf {
    let uid = rustix::process::getuid().as_raw();
    std::env::temp_dir().join(format!("kbf-oci-images-{uid}"))
}

impl Cell {
    /// A cell whose runtime is the OCI backend, running `crun` (`KBF_TEST_CRUN`, or
    /// `crun` on the `PATH`): its scratch directory is under the system temporary
    /// directory, whose ancestors every id can search (the backend's containers do not
    /// run as the test's user, and refuse a scratch directory they cannot reach).
    pub fn new(name: &str) -> Self {
        Self::with(name, |_| {})
    }

    /// [`Cell::new`], with `configure` applied to the runtime's configuration last.
    pub fn with(name: &str, configure: impl FnOnce(&mut PodmanConfig)) -> Self {
        let uid = rustix::process::getuid().as_raw();
        let scratch = std::env::temp_dir().join(format!("kbf-oci-{uid}-{name}"));
        super::force_remove(&scratch);
        std::fs::create_dir_all(&scratch).expect("create scratch");
        // The helper containers run this test binary in its `__chown-dirs` and
        // `__unpack-layers` modes (its `chown_dirs_test_hook!`); a copy in the scratch
        // directory, since the checkout's ancestors may be closed to the container's ids.
        let daemon = scratch.join("kbf-daemon");
        let exe = std::env::current_exe().expect("test binary");
        if std::fs::hard_link(&exe, &daemon).is_err() {
            std::fs::copy(&exe, &daemon).expect("copy");
        }
        // The one image store, as this cell's.
        let images = shared_images();
        std::fs::create_dir_all(&images).expect("the shared image store");
        std::os::unix::fs::symlink(&images, scratch.join("oci-images")).expect("link the store");
        Self::in_scratch(name, scratch, |config| {
            config.container_runtime = kbf_driver_container::ContainerRuntime::Oci;
            config.oci.runtime = crun();
            config.oci.daemon = daemon;
            configure(config);
        })
    }

    /// The same as [`Cell::new`] (every cell is the OCI backend now).
    pub fn oci(name: &str) -> Self {
        Self::new(name)
    }

    /// The same as [`Cell::with`].
    pub fn oci_with(name: &str, configure: impl FnOnce(&mut PodmanConfig)) -> Self {
        Self::with(name, configure)
    }

    fn in_scratch(name: &str, scratch: PathBuf, configure: impl FnOnce(&mut PodmanConfig)) -> Self {
        trace();
        let term = TERMS.fetch_add(1, Ordering::Relaxed);
        let parent = format!("{}/{name}", delegation().actions);
        let cgroup = Path::new(MOUNT).join(parent.trim_start_matches('/'));
        if exists(&cgroup) {
            std::fs::remove_dir(&cgroup).expect("remove a stale test cgroup");
        }
        std::fs::create_dir(&cgroup).expect("create the test cgroup");
        std::fs::write(cgroup.join("cgroup.subtree_control"), "+cpu +memory +pids")
            .expect("enable controllers");
        let mut config = PodmanConfig::new(scratch.clone(), parent, format!("kbf-test-{name}"));
        config.default_timeout = Duration::from_secs(120);
        config.kill_grace = Duration::from_secs(2);
        configure(&mut config);
        let cas = Arc::new(MemoryCas::new());
        let making = MAKING.lock().unwrap_or_else(PoisonError::into_inner);
        let runtime =
            Arc::new(PodmanRuntime::new(config.clone(), Arc::clone(&cas)).expect("runtime"));
        fetch_test_image(&runtime);
        drop(making);
        Self {
            term,
            cgroup,
            scratch,
            config,
            cas,
            runtime,
        }
    }

    /// Lease (`term`, `seq`) running `action`.
    pub fn work(&self, seq: u64, action: Digest, resources: Resources) -> Work {
        let mut work = work(seq, action, resources);
        work.lease_id = LeaseId::new(self.term, seq);
        work
    }

    /// The container and lease cgroup name of lease `seq`.
    pub fn name(&self, seq: u64) -> String {
        format!("kbf-lease-{}-{seq}", self.term)
    }

    pub async fn run(&self, seq: u64, spec: &Spec) -> Result<ActionResult, RuntimeError> {
        let action = store_action(&self.cas, spec);
        self.runtime
            .run(self.work(seq, action, Resources::new(1000, 256 << 20)))
            .await
    }

    /// Asserts lease `seq` left no container (its `crun` state, in its scratch
    /// directory), cgroup or scratch directory.
    pub fn assert_clean(&self, seq: u64) {
        let name = self.name(seq);
        assert!(!exists(&self.scratch.join(&name)), "{name}: scratch left");
        let lease = self.cgroup.join(&name);
        assert!(
            !exists(&lease),
            "{name}: lease cgroup left: {}",
            describe(&lease)
        );
    }

    /// Whether lease `seq`'s container exists: `crun` keeps a state directory for it in
    /// the lease's scratch directory (`<lease>/crun/<id>`) and removes it when the
    /// container ends.
    pub fn container_exists(&self, seq: u64) -> bool {
        let name = self.name(seq);
        exists(&self.scratch.join(&name).join("crun").join(&name))
    }

    pub fn stdout(&self, result: &ActionResult) -> String {
        String::from_utf8(blob(&self.cas, result.stdout_digest.as_ref())).expect("utf-8")
    }

    pub fn stderr(&self, result: &ActionResult) -> String {
        String::from_utf8(blob(&self.cas, result.stderr_digest.as_ref())).expect("utf-8")
    }
}

impl Drop for Cell {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            let _ = std::fs::remove_dir(&self.cgroup);
            // What a lease leaves is the driver's to remove (the files in it are owned
            // by the container's ids); what is left here is the daemon user's own.
            super::force_remove(&self.scratch);
        }
    }
}

/// Makes sure the store holds the test image (`KBF_TEST_IMAGE`): the first cell of a
/// machine has the runtime fetch it from its registry, checking every byte against its
/// digest, and unpack it. The runtime's check is async and a cell is made inside a
/// test's runtime, so it runs on a thread of its own.
fn fetch_test_image(runtime: &Arc<PodmanRuntime<MemoryCas>>) {
    let image = kbf_driver_container::ImageRef::parse(&var("KBF_TEST_IMAGE")).expect("an image");
    let runtime = Arc::clone(runtime);
    let fetched = std::thread::spawn(move || {
        let built = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a runtime");
        built.block_on(runtime.check_pinned(&image))
    })
    .join()
    .expect("the fetch thread");
    if let Err(e) = fetched {
        panic!("the test image could not be fetched and unpacked: {e}");
    }
}

/// The `crun` the OCI tests run: `KBF_TEST_CRUN`, else the first `crun` on the `PATH`.
pub fn crun() -> PathBuf {
    if let Some(path) = std::env::var_os("KBF_TEST_CRUN") {
        return PathBuf::from(path);
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .map(|dir| dir.join("crun"))
        .find(|candidate| candidate.is_file())
        .expect("no crun on the PATH (set KBF_TEST_CRUN): run through cargo xtask podman-tests")
}

/// An action running `script` with the image's `/bin/sh`, named by its path: the
/// action's environment is the `Command`'s alone (`--unsetenv-all`), and these name no
/// `PATH` to look a bare `sh` up in.
pub fn sh(script: &str) -> Spec {
    let mut spec = Spec::new(&var("KBF_TEST_IMAGE"), script);
    spec.argv[0] = "/bin/sh".to_owned();
    spec
}
