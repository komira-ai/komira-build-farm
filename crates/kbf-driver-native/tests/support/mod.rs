//! A CAS in memory, actions stored in it, a runtime over it, and checks on processes.

#![allow(dead_code)] // Each test binary uses its own part of this module.

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
    Action, ActionResult, Command, Digest, Directory, FileNode, Platform, Tree,
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

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Vec<u8>>> {
        self.blobs.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Cas for MemoryCas {
    async fn get(&self, digest: &Digest) -> Result<Vec<u8>, CasError> {
        if let Some(delay) = self.delay {
            tokio::time::sleep(delay).await;
        }
        self.lock()
            .get(&label(digest))
            .cloned()
            .ok_or_else(|| CasError::Missing(label(digest)))
    }

    async fn put(&self, bytes: Vec<u8>) -> Result<Digest, CasError> {
        Ok(self.insert(bytes))
    }
}

/// An action to store: `sh -c <script>` with an empty input root unless changed.
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
    pub fn sh(script: &str) -> Self {
        Self::argv(&["/bin/sh", "-c", script])
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
        let root = Directory {
            files: self
                .inputs
                .iter()
                .map(|(name, bytes)| FileNode {
                    name: name.clone(),
                    digest: Some(cas.insert(bytes.clone())),
                    is_executable: true,
                    ..FileNode::default()
                })
                .collect(),
            ..Directory::default()
        };
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

/// A configuration for tests: a fast poll, everything else the default.
pub fn config(scratch: &Path) -> NativeConfig {
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

/// Whether the scratch root holds no lease directory.
pub fn no_leases(config: &NativeConfig) -> bool {
    std::fs::read_dir(&config.scratch)
        .expect("scratch root")
        .next()
        .is_none()
}
