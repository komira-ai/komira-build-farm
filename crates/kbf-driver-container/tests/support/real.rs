//! One real-Podman test's cgroup, scratch directory and runtime ([`Cell`]), shared by
//! the test binaries that run the driver against rootless Podman
//! (`tools/ci/podman-tests.sh` sets up what they need).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use kbf_daemon::{Runtime, RuntimeError, Work};
use kbf_driver_container::{MemoryCas, PodmanConfig, PodmanRuntime};
use kbf_proto::reapi::{ActionResult, Digest};
use kbf_types::{LeaseId, Resources};

use super::{Spec, blob, exists, store_action, work};

pub fn var(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| {
        panic!("{name} is not set: run these tests through tools/ci/podman-tests.sh")
    })
}

/// One test's cgroup parent (under the delegated cgroup), scratch and runtime.
pub struct Cell {
    /// The lease term this test's leases use. Container names are unique per Podman
    /// store, and the tests share one, so each test gets its own term.
    pub term: u64,
    pub cgroup: PathBuf,
    pub scratch: PathBuf,
    pub config: PodmanConfig,
    pub cas: Arc<MemoryCas>,
    pub runtime: Arc<PodmanRuntime<MemoryCas>>,
}

static TERMS: AtomicU64 = AtomicU64::new(1);

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
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();
}

impl Cell {
    pub fn new(name: &str) -> Self {
        Self::with(name, |_| {})
    }

    /// [`Cell::new`], with `configure` applied to the runtime's configuration last.
    pub fn with(name: &str, configure: impl FnOnce(&mut PodmanConfig)) -> Self {
        trace();
        let term = TERMS.fetch_add(1, Ordering::Relaxed);
        let parent = format!("{}/{name}", var("KBF_TEST_CGROUP"));
        let cgroup = Path::new("/sys/fs/cgroup").join(parent.trim_start_matches('/'));
        if exists(&cgroup) {
            std::fs::remove_dir(&cgroup).expect("remove a stale test cgroup");
        }
        std::fs::create_dir(&cgroup).expect("create the test cgroup");
        std::fs::write(cgroup.join("cgroup.subtree_control"), "+cpu +memory +pids")
            .expect("enable controllers");
        let scratch = super::scratch(&format!("podman-{name}"));
        // Each test its own owner: they share one Podman store, and a runtime removes
        // its owner's containers when it starts.
        let mut config = PodmanConfig::new(scratch.clone(), parent, format!("kbf-test-{name}"));
        config.default_timeout = Duration::from_secs(120);
        config.kill_grace = Duration::from_secs(2);
        configure(&mut config);
        let cas = Arc::new(MemoryCas::new());
        let runtime =
            Arc::new(PodmanRuntime::new(config.clone(), Arc::clone(&cas)).expect("runtime"));
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

    /// Asserts lease `seq` left no container, cgroup or scratch directory.
    pub fn assert_clean(&self, seq: u64) {
        let name = self.name(seq);
        assert!(!exists(&self.scratch.join(&name)), "{name}: scratch left");
        let lease = self.cgroup.join(&name);
        assert!(
            !exists(&lease),
            "{name}: lease cgroup left: {}",
            describe(&lease)
        );
        let names = podman(&["ps", "--all", "--format={{.Names}}"]);
        assert!(!names.lines().any(|n| n == name), "container left: {names}");
    }

    pub fn stdout(&self, result: &ActionResult) -> String {
        String::from_utf8(blob(&self.cas, result.stdout_digest.as_ref())).expect("utf-8")
    }
}

impl Drop for Cell {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            let _ = std::fs::remove_dir(&self.cgroup);
            super::force_remove(&self.scratch);
        }
    }
}

/// Runs podman (the test's own checks) and returns its stdout.
pub fn podman(args: &[&str]) -> String {
    let output = Command::new("podman")
        .args(args)
        .output()
        .expect("run podman");
    assert!(
        output.status.success(),
        "podman {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// An action running `script` with the image's `/bin/sh`, named by its path: the
/// action's environment is the `Command`'s alone (`--unsetenv-all`), and these name no
/// `PATH` to look a bare `sh` up in.
pub fn sh(script: &str) -> Spec {
    let mut spec = Spec::new(&var("KBF_TEST_IMAGE"), script);
    spec.argv[0] = "/bin/sh".to_owned();
    spec
}
