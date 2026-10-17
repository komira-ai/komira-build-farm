//! One test's stand-in `crun`, fake cgroup mount, scratch directory and OCI-backed
//! runtime, shared by the test binaries that run the OCI backend without real `crun`.
//!
//! The stand-in is the program `kbf-fake-crun` (`tests/bin/fake_crun.rs`, which lists
//! its knobs and records). By default the image is already unpacked in the node's image
//! store (a directory a real unpack would have made), so a lease finds it with one
//! `stat`; [`FakeOci::fetching`] leaves it to be fetched from a registry in this process
//! and unpacked, which the stand-in `crun` does by running the daemon's own
//! `__unpack-layers` mode. A `podman` beside the `crun` link is the same program as a
//! stub that fails and records that it ran: [`FakeOci::podman_calls`] is how a test
//! checks that nothing ever runs it, and the configuration's `podman` names it.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use kbf_daemon::{Runtime, RuntimeError};
use kbf_driver_container::{
    ContainerRuntime, IdFiles, MemoryCas, PodmanConfig, PodmanRuntime, StartError,
};
use kbf_types::Resources;

use super::fake::{MANIFEST, sha256};
use super::registry::{FakeRegistry, Image};
use super::{Spec, exists, store_action, work};

/// The stand-in `crun` the tests link: `kbf-fake-crun`.
pub fn program() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_kbf-fake-crun"))
}

/// For `FakeOci::with`: replaces the `crun` link by a private copy of the program, which
/// a knob may then remove or make not executable (`vanish-after-give`,
/// `unexec-after-give`) without touching the program every test shares.
pub fn private_runtime(config: &mut PodmanConfig) {
    let at = config.oci.runtime.clone();
    std::fs::remove_file(&at).expect("remove the link");
    std::fs::copy(program(), &at).expect("copy the program");
}

/// One test's fake `crun`, fake cgroup mount, scratch directory and runtime.
pub struct FakeOci {
    pub dir: PathBuf,
    /// The stand-in `crun`'s state and knobs.
    pub state: PathBuf,
    /// The registry in this process, for a fake made with [`FakeOci::fetching`].
    pub registry: Option<FakeRegistry>,
    pub cgroup: PathBuf,
    pub scratch: PathBuf,
    pub config: PodmanConfig,
    pub cas: Arc<MemoryCas>,
    pub runtime: Arc<PodmanRuntime<MemoryCas>>,
    nonce: String,
}

/// The id files of a daemon user with one range of 65,536 subordinate ids at 100000.
fn id_files(dir: &Path) -> IdFiles {
    let files = IdFiles::in_dir(dir);
    std::fs::create_dir_all(dir).expect("mkdir ids");
    std::fs::write(&files.passwd, "root:x:0:0::/:/bin/sh\n").expect("passwd");
    let uid = rustix::process::getuid().as_raw();
    for file in [&files.subuid, &files.subgid] {
        std::fs::write(file, format!("{uid}:100000:65536\n")).expect("subid file");
    }
    files
}

/// A directory of `newuidmap` and `newgidmap` that satisfy the start-up check.
fn id_tools(dir: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt as _;
    let tools = dir.join("id-tools");
    std::fs::create_dir_all(&tools).expect("mkdir tools");
    for name in ["newuidmap", "newgidmap"] {
        let path = tools.join(name);
        std::fs::write(&path, b"").expect("tool");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o4755)).expect("chmod");
    }
    tools
}

impl FakeOci {
    pub fn new(name: &str) -> Self {
        Self::with(name, |_| {})
    }

    /// [`FakeOci::new`], with `configure` applied to the runtime's configuration last.
    pub fn with(name: &str, configure: impl FnOnce(&mut PodmanConfig)) -> Self {
        Self::build(name, None, configure)
    }

    /// A fake whose image is not in the node's store: `registry` serves it, and the
    /// first lease (or check) fetches and unpacks it.
    pub fn fetching(name: &str, registry: FakeRegistry) -> Self {
        Self::build(name, Some(registry), |_| {})
    }

    fn build(
        name: &str,
        registry: Option<FakeRegistry>,
        configure: impl FnOnce(&mut PodmanConfig),
    ) -> Self {
        let dir = super::scratch(&format!("fake-oci-{name}"));
        let state = dir.join("crun-state");
        let cgroup = dir.join("cgroup");
        let scratch = dir.join("scratch");
        for d in [&state, &cgroup.join("actions"), &scratch] {
            std::fs::create_dir_all(d).expect("mkdir");
        }
        let crun = dir.join("crun");
        std::os::unix::fs::symlink(program(), &crun).expect("link crun");
        let podman = dir.join("podman");
        std::os::unix::fs::symlink(program(), &podman).expect("link podman");
        super::fake::link_act(&dir);
        if registry.is_none() {
            // The unpacked image: where a real unpack of the pinned image would leave it.
            let hex = sha256(MANIFEST);
            let rootfs = scratch
                .join("oci-images")
                .join(hex.trim_start_matches("sha256:"))
                .join("rootfs");
            std::fs::create_dir_all(rootfs.join("kbf/root")).expect("rootfs");
        }
        let since_epoch = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after 1970");
        let nonce = format!("{}-{}:", std::process::id(), since_epoch.as_nanos());
        std::fs::write(state.join("nonce"), &nonce).expect("nonce");
        let mut config =
            PodmanConfig::new(scratch.clone(), "/actions".to_owned(), "node-1".to_owned());
        config.podman = podman;
        config.container_runtime = ContainerRuntime::Oci;
        config.oci.runtime = crun;
        config.oci.id_files = id_files(&dir.join("ids"));
        config.oci.id_tool_path = Some(id_tools(&dir).into_os_string());
        if let Some(registry) = &registry {
            config.oci.plain_http_registries = vec![registry.addr.clone()];
        }
        // The stand-in runs as the test's user, so no other id needs to reach the lease,
        // and a home directory closed to others is fine.
        config.oci.check_search = false;
        config.cgroup_root = cgroup.clone();
        config.default_timeout = Duration::from_secs(60);
        config.kill_grace = Duration::from_millis(300);
        configure(&mut config);
        let cas = Arc::new(MemoryCas::new());
        let runtime =
            Arc::new(PodmanRuntime::new(config.clone(), Arc::clone(&cas)).expect("runtime"));
        Self {
            dir,
            state,
            registry,
            cgroup,
            scratch,
            config,
            cas,
            runtime,
            nonce,
        }
    }

    pub fn knob(&self, name: &str, contents: &str) {
        std::fs::write(self.state.join(name), contents).expect("write knob");
    }

    fn lines(&self, name: &str) -> Vec<String> {
        std::fs::read_to_string(self.state.join(name))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// The verbs the stand-in `crun` was run with, in order (`run`, `kill`, `delete`,
    /// `helper give`, `helper rm`, `helper mkdir`, `helper tar`, `helper touch`).
    pub fn calls(&self) -> Vec<String> {
        self.lines("calls")
    }

    /// What the stand-in recorded, in order (see `tests/bin/fake_crun.rs`).
    pub fn events(&self) -> Vec<String> {
        self.lines("events")
    }

    /// The times the `podman` stub was run: never, on any path.
    pub fn podman_calls(&self) -> Vec<String> {
        std::fs::read_to_string(self.dir.join("podman-ran"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// The registry's requests so far.
    pub fn registry_requests(&self) -> Vec<String> {
        self.registry
            .as_ref()
            .map(FakeRegistry::requests)
            .unwrap_or_default()
    }

    /// The `config.json` of the last action's bundle.
    pub fn config_json(&self) -> serde_json::Value {
        let text = std::fs::read_to_string(self.state.join("config.json")).expect("config.json");
        serde_json::from_str(&text).expect("json")
    }

    /// The image's directory in the node's image store.
    pub fn image_dir(&self) -> PathBuf {
        self.image_dir_of(&sha256(MANIFEST))
    }

    /// The directory in the node's image store of the image with manifest `digest`.
    pub fn image_dir_of(&self, digest: &str) -> PathBuf {
        self.scratch
            .join("oci-images")
            .join(digest.trim_start_matches("sha256:"))
    }

    /// The names in the image store's directory.
    pub fn image_store(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(self.scratch.join("oci-images"))
            .map(|d| {
                d.flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }

    /// Publishes `image` on this fake's registry and returns the reference an action
    /// names it by.
    pub fn publish(&self, image: &Image) -> String {
        let registry = self.registry.as_ref().expect("a fake made with `fetching`");
        registry.publish(image);
        image.reference(&registry.addr, "test/tiny")
    }

    pub fn lease_dir(&self, seq: u64) -> PathBuf {
        self.scratch.join(format!("kbf-lease-1-{seq}"))
    }

    pub fn lease_cgroup(&self, seq: u64) -> PathBuf {
        self.cgroup.join(format!("actions/kbf-lease-1-{seq}"))
    }

    fn marker(&self, seq: u64) -> String {
        format!("FAKE_LEASE={}/actions/kbf-lease-1-{seq}", self.nonce)
    }

    /// Asserts the lease left nothing behind, and nothing of its action runs.
    pub fn assert_clean(&self, seq: u64) {
        assert!(!exists(&self.lease_dir(seq)), "scratch directory left");
        assert!(!exists(&self.lease_cgroup(seq)), "lease cgroup left");
        assert!(
            !self.state.join("run-not-reaped-at-delete").exists(),
            "crun delete ran before crun run was reaped"
        );
        self.assert_action_gone(seq);
    }

    pub fn assert_action_running(&self, seq: u64) {
        let marker = self.marker(seq);
        let deadline = Instant::now() + Duration::from_secs(5);
        while running_with_env(marker.as_bytes()).is_empty() {
            assert!(
                Instant::now() < deadline,
                "no process carries lease {seq}'s marker {marker}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    pub fn assert_action_gone(&self, seq: u64) {
        let marker = self.marker(seq);
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let left = running_with_env(marker.as_bytes());
            if left.is_empty() {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "lease {seq}'s action still runs after the clean: pids {left:?}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    pub fn action(&self, steps: &[impl AsRef<str>]) {
        self.knob("action", &super::fake::action_knob(steps));
    }

    pub async fn run(
        &self,
        seq: u64,
        spec: &Spec,
        steps: &[impl AsRef<str>],
    ) -> Result<kbf_proto::reapi::ActionResult, RuntimeError> {
        self.action(steps);
        let action = store_action(&self.cas, spec);
        self.runtime
            .run(work(seq, action, Resources::new(2000, 1 << 30)))
            .await
    }

    /// A second runtime on this fake's configuration, as a restarted daemon makes.
    pub fn restart(&self) -> Result<PodmanRuntime<MemoryCas>, StartError> {
        PodmanRuntime::new(self.config.clone(), Arc::clone(&self.cas))
    }

    pub async fn wait_for_start(&self) {
        for _ in 0..500 {
            if self.state.join("pid").exists() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the action never started");
    }

    fn wait_until_nothing_runs(&self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        let program = self.dir.join("crun");
        let program = program.as_os_str().as_encoded_bytes().to_vec();
        loop {
            let left = pids_where(|pid| {
                let in_argv = std::fs::read(format!("/proc/{pid}/cmdline"))
                    .is_ok_and(|argv| argv.split(|b| *b == 0).any(|a| a == program));
                in_argv
                    || has_env(pid, |e| {
                        e.starts_with(format!("FAKE_LEASE={}", self.nonce).as_bytes())
                    })
            });
            if left.is_empty() {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "{}: pids {left:?} still run ten seconds after the test",
                self.dir.display()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

fn running_with_env(entry: &[u8]) -> Vec<u32> {
    pids_where(|pid| has_env(pid, |e| e == entry))
}

fn has_env(pid: u32, test: impl Fn(&[u8]) -> bool) -> bool {
    std::fs::read(format!("/proc/{pid}/environ")).is_ok_and(|env| env.split(|b| *b == 0).any(test))
}

fn pids_where(test: impl Fn(u32) -> bool) -> Vec<u32> {
    std::fs::read_dir("/proc")
        .expect("list /proc")
        .filter_map(|p| p.ok()?.file_name().to_str()?.parse::<u32>().ok())
        .filter(|pid| test(*pid))
        .collect()
}

impl Drop for FakeOci {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            self.wait_until_nothing_runs();
            super::force_remove(&self.dir);
        }
    }
}
