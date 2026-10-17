//! A CAS in memory, actions stored in it, a runtime over it, and checks on processes.

#![allow(dead_code)] // Each test binary uses its own part of this module.

// The keepers the driver starts come from the running binary: this test binary.
kbf_daemon::keeper_test_hook!();
// So do the actions of [`Spec::act`]: this test binary run as kbf-testkit's `kbf-act`.
kbf_testkit::act_test_hook!();
// And the `as-user` step of an action run as a lease-user pool's slot user
// (`tests/slots.rs`).
kbf_driver_native::as_user_test_hook!();

#[cfg(target_os = "macos")]
pub mod pool;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use kbf_daemon::cas::{Cas, CasError, digest_of, label};
use kbf_daemon::{Runtime, RuntimeError, Work};
use kbf_driver_native::{NativeConfig, NativeRuntime, procs};
use kbf_proto::reapi::command::EnvironmentVariable;
use kbf_proto::reapi::platform::Property;
use kbf_proto::reapi::{
    Action, ActionResult, Command, Digest, Directory, DirectoryNode, FileNode, Platform, Tree,
};
use kbf_types::{LeaseId, Resources};
use prost::Message;

/// How long a test waits for something that should happen promptly.
pub const PROMPT: Duration = Duration::from_secs(10);

/// A CAS in memory; `delay` holds every `get` back (to kill a lease mid-fetch).
#[derive(Debug, Default)]
pub struct MemoryCas {
    pub blobs: Mutex<BTreeMap<String, Vec<u8>>>,
    pub delay: Option<Duration>,
    /// Every put fails as an unreachable CAS would.
    pub refuse_puts: bool,
    /// The digests `get` was asked for, by label, with how often.
    pub reads: Mutex<BTreeMap<String, u64>>,
}

impl MemoryCas {
    pub fn insert(&self, bytes: impl Into<Vec<u8>>) -> Digest {
        let bytes = bytes.into();
        let digest = digest_of(&bytes);
        self.lock().insert(label(&digest), bytes);
        digest
    }

    pub fn blob(&self, digest: &Digest) -> Vec<u8> {
        self.lock()
            .get(&label(digest))
            .cloned()
            .unwrap_or_else(|| panic!("blob {} is stored", label(digest)))
    }

    /// How many times `get` was asked for `digest`.
    pub fn reads_of(&self, digest: &Digest) -> u64 {
        let reads = self.reads.lock().unwrap_or_else(PoisonError::into_inner);
        reads.get(&label(digest)).copied().unwrap_or(0)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Vec<u8>>> {
        self.blobs.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Cas for MemoryCas {
    type Lease = std::sync::Arc<Self>;

    fn lease(self: &std::sync::Arc<Self>, _: &kbf_types::LeaseId) -> std::sync::Arc<Self> {
        std::sync::Arc::clone(self)
    }

    async fn get(&self, digest: &Digest) -> Result<Vec<u8>, CasError> {
        *(self.reads.lock().unwrap_or_else(PoisonError::into_inner))
            .entry(label(digest))
            .or_default() += 1;
        if let Some(delay) = self.delay {
            tokio::time::sleep(delay).await;
        }
        self.lock()
            .get(&label(digest))
            .cloned()
            .ok_or_else(|| CasError::Missing(label(digest)))
    }

    async fn put(&self, bytes: Vec<u8>) -> Result<Digest, CasError> {
        if self.refuse_puts {
            return Err(CasError::Unavailable(
                label(&digest_of(&bytes)),
                "the CAS is down".to_owned(),
            ));
        }
        Ok(self.insert(bytes))
    }
}

/// A link to this test binary named `kbf-act`, made once: run through it, the binary
/// runs kbf-testkit's act steps (`kbf_testkit::act`) instead of the tests.
pub fn act() -> PathBuf {
    static LINK: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    LINK.get_or_init(|| {
        let link = scratch("act").join(kbf_testkit::act::NAME);
        kbf_testkit::act::link_self(&link).expect("link");
        link
    })
    .clone()
}

/// An action to store, with an empty input root unless changed.
#[derive(Clone, Debug)]
pub struct Spec {
    pub argv: Vec<String>,
    pub env: Vec<(String, String)>,
    pub outputs: Vec<String>,
    pub working_directory: String,
    pub inputs: Vec<(String, Vec<u8>)>,
    pub properties: Vec<(String, String)>,
    pub timeout: Option<Duration>,
}

impl Spec {
    /// `sh -c <script>`: for the macOS tests of what a user's own shell command, and
    /// the system tools it runs, may do under the sandbox.
    #[cfg(target_os = "macos")]
    pub fn sh(script: &str) -> Self {
        Self::argv(&["/bin/sh", "-c", script])
    }

    /// The act steps `steps`, split at spaces (see `kbf_testkit::act`).
    pub fn act(steps: &str) -> Self {
        let act = act();
        let mut argv = vec![act.to_str().expect("UTF-8")];
        argv.extend(steps.split(' '));
        Self::argv(&argv)
    }

    pub fn argv(argv: &[&str]) -> Self {
        Self {
            argv: argv.iter().map(|a| (*a).to_owned()).collect(),
            env: Vec::new(),
            outputs: Vec::new(),
            working_directory: String::new(),
            inputs: Vec::new(),
            properties: Vec::new(),
            timeout: None,
        }
    }

    pub fn env(mut self, name: &str, value: &str) -> Self {
        self.env.push((name.to_owned(), value.to_owned()));
        self
    }

    pub fn outputs(mut self, outputs: &[&str]) -> Self {
        self.outputs = outputs.iter().map(|o| (*o).to_owned()).collect();
        self
    }

    pub fn property(mut self, name: &str, value: &str) -> Self {
        self.properties.push((name.to_owned(), value.to_owned()));
        self
    }

    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Stores the action in `cas` and returns its digest.
    pub fn store(&self, cas: &MemoryCas) -> Digest {
        let root = directory_of(cas, &self.inputs);
        let command = Command {
            arguments: self.argv.clone(),
            environment_variables: self
                .env
                .iter()
                .map(|(name, value)| EnvironmentVariable {
                    name: name.clone(),
                    value: value.clone(),
                })
                .collect(),
            output_paths: self.outputs.clone(),
            working_directory: self.working_directory.clone(),
            ..Command::default()
        };
        let action = Action {
            command_digest: Some(cas.insert(command.encode_to_vec())),
            input_root_digest: Some(cas.insert(root.encode_to_vec())),
            timeout: self.timeout.map(|t| prost_types::Duration {
                seconds: i64::try_from(t.as_secs()).expect("seconds"),
                nanos: i32::try_from(t.subsec_nanos()).expect("nanos"),
            }),
            platform: (!self.properties.is_empty()).then(|| Platform {
                properties: self
                    .properties
                    .iter()
                    .map(|(name, value)| Property {
                        name: name.clone(),
                        value: value.clone(),
                    })
                    .collect(),
            }),
            ..Action::default()
        };
        cas.insert(action.encode_to_vec())
    }
}

/// The Directory of `inputs` (every file executable): a name with `/` is in the
/// subdirectory it names, stored in `cas` as well.
fn directory_of(cas: &MemoryCas, inputs: &[(String, Vec<u8>)]) -> Directory {
    let mut files = Vec::new();
    let mut below: BTreeMap<&str, Vec<(String, Vec<u8>)>> = BTreeMap::new();
    for (name, bytes) in inputs {
        match name.split_once('/') {
            None => files.push(FileNode {
                name: name.clone(),
                digest: Some(cas.insert(bytes.clone())),
                is_executable: true,
                ..FileNode::default()
            }),
            Some((dir, rest)) => below
                .entry(dir)
                .or_default()
                .push((rest.to_owned(), bytes.clone())),
        }
    }
    let directories = below
        .into_iter()
        .map(|(name, inner)| DirectoryNode {
            name: name.to_owned(),
            digest: Some(cas.insert(directory_of(cas, &inner).encode_to_vec())),
        })
        .collect();
    Directory {
        files,
        directories,
        ..Directory::default()
    }
}

/// A fresh scratch directory for one test, under Cargo's per-target temporary
/// directory.
pub fn scratch(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("kbf-driver-native")
        .join(format!("{name}-{}", std::process::id()));
    if std::fs::symlink_metadata(&dir).is_ok() {
        kbf_outputs::remove_tree(&dir).expect("clear old scratch");
    }
    std::fs::create_dir_all(&dir).expect("scratch");
    dir
}

/// A configuration for tests: a fast poll, everything else the default. Also turns on
/// logging to the test's output, so the driver's warnings are written (and run).
pub fn config(scratch: &Path) -> NativeConfig {
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();
    let mut config = NativeConfig::new(scratch.join("leases"));
    config.poll = Duration::from_millis(20);
    config
}

pub fn runtime(config: NativeConfig, cas: &Arc<MemoryCas>) -> NativeRuntime<MemoryCas> {
    NativeRuntime::new(config, Arc::clone(cas)).expect("runtime")
}

pub fn work(seq: u64, action: Digest, memory_bytes: u64) -> Work {
    Work {
        lease_id: LeaseId::new(1, seq),
        kind: "action".to_owned(),
        action_digest: action,
        resources: Resources::new(1000, memory_bytes),
    }
}

/// Runs `spec` as lease `1.seq` with nothing booked.
pub async fn run(
    runtime: &NativeRuntime<MemoryCas>,
    cas: &MemoryCas,
    seq: u64,
    spec: &Spec,
) -> Result<ActionResult, RuntimeError> {
    let action = spec.store(cas);
    tokio::time::timeout(PROMPT * 3, runtime.run(work(seq, action, 0)))
        .await
        .expect("the run ends")
}

pub fn stdout(cas: &MemoryCas, result: &ActionResult) -> String {
    let digest = result.stdout_digest.as_ref().expect("stdout digest");
    String::from_utf8(cas.blob(digest)).expect("UTF-8")
}

pub fn stderr(cas: &MemoryCas, result: &ActionResult) -> String {
    let digest = result.stderr_digest.as_ref().expect("stderr digest");
    String::from_utf8(cas.blob(digest)).expect("UTF-8")
}

pub fn tree(cas: &MemoryCas, digest: &Digest) -> Tree {
    Tree::decode(cas.blob(digest).as_slice()).expect("a Tree")
}

/// Whether `pid` is a live (not zombie) process.
pub fn alive(pid: i32) -> bool {
    procs::snapshot()
        .expect("snapshot")
        .iter()
        .any(|p| p.pid == pid && !p.zombie)
}

/// Reads the pid a test action wrote to `file`, waiting for it to appear.
pub async fn pid_in(file: &Path) -> i32 {
    let deadline = tokio::time::Instant::now() + PROMPT;
    loop {
        if let Ok(text) = std::fs::read_to_string(file)
            && let Ok(pid) = text.trim().parse()
        {
            return pid;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no pid in {}",
            file.display()
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Whether the scratch root holds no lease directory, no run record and no output
/// spool: nothing but the empty `runs` and `spool` directories the runtime makes.
pub fn no_leases(config: &NativeConfig) -> bool {
    let empty = |dir: &Path| {
        std::fs::read_dir(dir)
            .expect("a directory")
            .next()
            .is_none()
    };
    let runs = config.scratch.join("runs");
    // The output spools' directory: its files are unlinked as soon as they are made.
    let spool = config.scratch.join("spool");
    // The directory a start's simulator sweep runs `simctl` in, kept once made.
    let sweep = config.scratch.join("simulator-sweep");
    let others = std::fs::read_dir(&config.scratch)
        .expect("scratch root")
        .filter(|entry| {
            entry.as_ref().map_or(true, |e| {
                e.path() != runs && e.path() != spool && e.path() != sweep
            })
        })
        .count();
    others == 0 && empty(&runs) && empty(&spool)
}

/// `getconf <name>` as the test's user (the driver's user) sees it, every link
/// resolved: on macOS, `DARWIN_USER_TEMP_DIR` is the folder Foundation and `xcrun`
/// write in whatever `TMPDIR` says.
pub fn user_folder(name: &str) -> PathBuf {
    let out = std::process::Command::new("/usr/bin/getconf")
        .arg(name)
        .output()
        .expect("getconf");
    assert!(out.status.success(), "getconf {name}");
    let path = String::from_utf8(out.stdout).expect("UTF-8");
    std::fs::canonicalize(path.trim()).expect("the folder exists")
}

/// The file writes the macOS sandbox refused in the last ten minutes, one line per
/// operation and path, from the unified log: what a sandboxed tool that failed
/// wanted to write. For failure messages only; a note instead where the log cannot
/// be read.
pub fn sandbox_denials() -> String {
    let out = std::process::Command::new("/usr/bin/log")
        .args([
            "show",
            "--last",
            "10m",
            "--style",
            "compact",
            "--predicate",
            "eventMessage CONTAINS \"deny(1) file-write\"",
        ])
        .output();
    let Ok(out) = out else {
        return "sandbox denials: the log cannot be read".to_owned();
    };
    let mut lines: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| line.split_once("deny(1) "))
        .map(|(_, rest)| rest.to_owned())
        .collect();
    lines.sort();
    lines.dedup();
    format!("sandbox denials:\n{}", lines.join("\n"))
}
