//! One test's stand-in `podman` (`fixtures/fake-podman.sh`), fake cgroup mount, scratch
//! directory and runtime, shared by the test binaries that run the driver's steps
//! without real Podman.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use kbf_daemon::{Runtime, RuntimeError};
use kbf_driver_container::{MemoryCas, PodmanConfig, PodmanRuntime};
use kbf_types::Resources;

use super::{Spec, exists, store_action, work};

/// The per-architecture manifest every fake image store holds.
pub const MANIFEST: &[u8] =
    br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json"}"#;
/// An image index over it.
pub const INDEX: &[u8] =
    br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.index.v1+json","manifests":[]}"#;

pub fn sha256(bytes: &[u8]) -> String {
    format!("sha256:{}", kbf_daemon::cas::digest_of(bytes).hash)
}

pub fn image_by(digest: &str) -> String {
    format!("docker://registry.test/tools/busybox@{digest}")
}

pub fn image() -> String {
    image_by(&sha256(MANIFEST))
}

/// One test's fake Podman, fake cgroup mount, scratch directory and runtime.
pub struct Fake {
    pub dir: PathBuf,
    pub state: PathBuf,
    pub cgroup: PathBuf,
    pub scratch: PathBuf,
    pub cas: Arc<MemoryCas>,
    pub runtime: Arc<PodmanRuntime<MemoryCas>>,
    /// Written to `state/nonce`; the fake puts it in the action's `FAKE_LEASE`.
    nonce: String,
}

impl Fake {
    pub fn new(name: &str) -> Self {
        Self::with(name, |_| {})
    }

    /// [`Fake::new`], with `configure` applied to the runtime's configuration last.
    pub fn with(name: &str, configure: impl FnOnce(&mut PodmanConfig)) -> Self {
        let dir = super::scratch(&format!("fake-{name}"));
        let state = dir.join("state");
        let cgroup = dir.join("cgroup");
        let scratch = dir.join("scratch");
        for d in [&state, &cgroup.join("actions"), &scratch] {
            std::fs::create_dir_all(d).expect("mkdir");
        }
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-podman.sh");
        // A symlink, not a written script: exec'ing a file this process just wrote can
        // fail with ETXTBSY while another test thread forks.
        let program = dir.join("podman");
        std::os::unix::fs::symlink(&fixture, &program).expect("link podman");
        std::fs::write(state.join("image-id"), "img1\n").expect("image id");
        // Unique to this Fake, so what a failed earlier run left in the same checkout
        // is not taken for this run's action.
        let since_epoch = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after 1970");
        let nonce = format!("{}-{}:", std::process::id(), since_epoch.as_nanos());
        std::fs::write(state.join("nonce"), &nonce).expect("nonce");
        let mut config = PodmanConfig::new(scratch.clone(), "/actions".to_owned());
        config.podman = program;
        config.cgroup_root = cgroup.clone();
        config.default_timeout = Duration::from_secs(60);
        config.kill_grace = Duration::from_millis(300);
        configure(&mut config);
        let cas = Arc::new(MemoryCas::new());
        let runtime = Arc::new(PodmanRuntime::new(config, Arc::clone(&cas)).expect("runtime"));
        let fake = Self {
            dir,
            state,
            cgroup,
            scratch,
            cas,
            runtime,
            nonce,
        };
        fake.store_manifest(&sha256(MANIFEST), MANIFEST);
        fake
    }

    /// Puts `bytes` in the fake image store as image img1's manifest under `digest`.
    pub fn store_manifest(&self, digest: &str, bytes: &[u8]) {
        let dir = self.state.join("store/fake-images/img1");
        std::fs::create_dir_all(&dir).expect("mkdir store");
        let file = kbf_driver_container::image::manifest_file(digest);
        std::fs::write(dir.join(file), bytes).expect("write manifest");
    }

    pub fn knob(&self, name: &str, contents: &str) {
        std::fs::write(self.state.join(name), contents).expect("write knob");
    }

    pub fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(self.state.join("calls"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// What the fake recorded, in order (see `fixtures/fake-podman.sh`).
    pub fn events(&self) -> Vec<String> {
        std::fs::read_to_string(self.state.join("events"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    pub fn lease_dir(&self, seq: u64) -> PathBuf {
        self.scratch.join(format!("kbf-lease-1-{seq}"))
    }

    pub fn lease_cgroup(&self, seq: u64) -> PathBuf {
        self.cgroup.join(format!("actions/kbf-lease-1-{seq}"))
    }

    /// Asserts the lease left nothing behind.
    pub fn assert_clean(&self, seq: u64) {
        assert!(!exists(&self.lease_dir(seq)), "scratch directory left");
        assert!(!exists(&self.lease_cgroup(seq)), "lease cgroup left");
        self.assert_start_reaped_before_rm();
        self.assert_action_gone(seq);
    }

    /// Asserts nothing the lease's action started still runs. The fake gives the action
    /// `FAKE_LEASE=<this Fake's nonce><lease cgroup name>` and its children inherit it,
    /// so that is the fake's stand-in for membership of the lease cgroup. Waits up to
    /// five seconds, since a SIGKILL takes effect when its target next runs; a zombie
    /// (no environment left) has ended.
    pub fn assert_action_gone(&self, seq: u64) {
        let marker = format!("FAKE_LEASE={}/actions/kbf-lease-1-{seq}", self.nonce);
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

    /// Asserts no `podman rm` so far ran while a `podman start` the driver ran was
    /// still there (running, or exited but not reaped).
    pub fn assert_start_reaped_before_rm(&self) {
        assert!(
            !self.state.join("start-not-reaped-at-rm").exists(),
            "podman rm ran before podman start was reaped"
        );
    }

    pub async fn run(
        &self,
        seq: u64,
        spec: &Spec,
        script: &str,
    ) -> Result<kbf_proto::reapi::ActionResult, RuntimeError> {
        self.knob("action.sh", script);
        let action = store_action(&self.cas, spec);
        self.runtime
            .run(work(seq, action, Resources::new(2000, 1 << 30)))
            .await
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
}

/// The pids of this user's processes whose environment holds `entry` (`NAME=value`).
/// A process that ends while it is read, or is not ours to read, is skipped; a `/proc`
/// that cannot be listed fails the test rather than reading as "nothing runs".
fn running_with_env(entry: &[u8]) -> Vec<u32> {
    std::fs::read_dir("/proc")
        .expect("list /proc")
        .filter_map(|p| p.ok()?.file_name().to_str()?.parse::<u32>().ok())
        .filter(|pid| {
            std::fs::read(format!("/proc/{pid}/environ"))
                .is_ok_and(|env| env.split(|b| *b == 0).any(|e| e == entry))
        })
        .collect()
}

impl Drop for Fake {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            super::force_remove(&self.dir);
        }
    }
}
