//! Shared by the driver's integration tests: actions stored in a [`MemoryCas`], and
//! scratch directories under Cargo's per-target temporary directory.

#![allow(dead_code)] // Each test binary uses its own part of this module.

pub mod fake;

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use kbf_daemon::Work;
use kbf_driver_container::MemoryCas;
use kbf_proto::reapi::command::EnvironmentVariable;
use kbf_proto::reapi::platform::Property;
use kbf_proto::reapi::{
    Action, Command, Digest, Directory, DirectoryNode, FileNode, Platform, SymlinkNode, Tree,
};
use kbf_types::{LeaseId, Resources};
use prost::Message;

/// One input file: path, contents, executable.
pub type Input = (&'static str, &'static [u8], bool);

/// An action to store.
#[derive(Clone, Debug)]
pub struct Spec {
    pub image: String,
    pub argv: Vec<String>,
    pub env: Vec<(String, String)>,
    pub working_directory: String,
    pub outputs: Vec<String>,
    pub inputs: Vec<Input>,
    pub symlinks: Vec<(&'static str, &'static str)>,
    pub timeout: Option<Duration>,
}

impl Spec {
    pub fn new(image: &str, script: &str) -> Self {
        Self {
            image: image.to_owned(),
            argv: vec!["sh".to_owned(), "-c".to_owned(), script.to_owned()],
            env: Vec::new(),
            working_directory: String::new(),
            outputs: Vec::new(),
            inputs: Vec::new(),
            symlinks: Vec::new(),
            timeout: None,
        }
    }
}

#[derive(Default)]
struct Node {
    files: BTreeMap<String, (Vec<u8>, bool)>,
    dirs: BTreeMap<String, Node>,
    symlinks: BTreeMap<String, String>,
}

impl Node {
    fn at(&mut self, path: &str) -> (&mut Node, String) {
        let mut parts: Vec<&str> = path.split('/').collect();
        let leaf = parts.pop().expect("a name").to_owned();
        let mut node = self;
        for part in parts {
            node = node.dirs.entry(part.to_owned()).or_default();
        }
        (node, leaf)
    }

    fn store(&self, cas: &MemoryCas) -> Digest {
        let directory = Directory {
            files: self
                .files
                .iter()
                .map(|(name, (bytes, executable))| FileNode {
                    name: name.clone(),
                    digest: Some(cas.insert(bytes.clone())),
                    is_executable: *executable,
                    ..FileNode::default()
                })
                .collect(),
            directories: self
                .dirs
                .iter()
                .map(|(name, node)| DirectoryNode {
                    name: name.clone(),
                    digest: Some(node.store(cas)),
                })
                .collect(),
            symlinks: self
                .symlinks
                .iter()
                .map(|(name, target)| SymlinkNode {
                    name: name.clone(),
                    target: target.clone(),
                    ..SymlinkNode::default()
                })
                .collect(),
            ..Directory::default()
        };
        cas.insert(directory.encode_to_vec())
    }
}

/// Stores the input tree of `inputs` and `symlinks`; returns the root digest.
pub fn store_tree(
    cas: &MemoryCas,
    inputs: &[Input],
    symlinks: &[(&'static str, &'static str)],
) -> Digest {
    let mut root = Node::default();
    for (path, bytes, executable) in inputs {
        let (node, leaf) = root.at(path);
        node.files.insert(leaf, (bytes.to_vec(), *executable));
    }
    for (path, target) in symlinks {
        let (node, leaf) = root.at(path);
        node.symlinks.insert(leaf, (*target).to_owned());
    }
    root.store(cas)
}

/// Stores the action `spec` describes; returns the Action's digest.
pub fn store_action(cas: &MemoryCas, spec: &Spec) -> Digest {
    let command = Command {
        arguments: spec.argv.clone(),
        environment_variables: spec
            .env
            .iter()
            .map(|(name, value)| EnvironmentVariable {
                name: name.clone(),
                value: value.clone(),
            })
            .collect(),
        output_paths: spec.outputs.clone(),
        working_directory: spec.working_directory.clone(),
        ..Command::default()
    };
    let action = Action {
        command_digest: Some(cas.insert(command.encode_to_vec())),
        input_root_digest: Some(store_tree(cas, &spec.inputs, &spec.symlinks)),
        timeout: spec.timeout.map(|t| prost_types::Duration {
            seconds: i64::try_from(t.as_secs()).expect("seconds"),
            nanos: i32::try_from(t.subsec_nanos()).expect("nanos"),
        }),
        platform: Some(Platform {
            properties: vec![Property {
                name: "container-image".to_owned(),
                value: spec.image.clone(),
            }],
        }),
        ..Action::default()
    };
    cas.insert(action.encode_to_vec())
}

/// The Work for lease (1, `seq`) running `action`.
pub fn work(seq: u64, action: Digest, resources: Resources) -> Work {
    Work {
        lease_id: LeaseId::new(1, seq),
        kind: "action".to_owned(),
        action_digest: action,
        resources,
    }
}

/// The bytes of a blob the CAS holds.
pub fn blob(cas: &MemoryCas, digest: Option<&Digest>) -> Vec<u8> {
    cas.blob(digest.expect("a digest"))
        .expect("the blob is stored")
}

/// A decoded Tree the CAS holds.
pub fn tree(cas: &MemoryCas, digest: Option<&Digest>) -> Tree {
    Tree::decode(blob(cas, digest).as_slice()).expect("a Tree")
}

/// A fresh, empty directory for one test, under Cargo's temporary directory for this
/// target. A directory left by an earlier run is removed first.
pub fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    force_remove(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch");
    dir
}

/// Removes `dir`, first giving the owner every permission on every directory in it.
pub fn force_remove(dir: &Path) {
    if std::fs::symlink_metadata(dir).is_err() {
        return;
    }
    let mut pending = vec![dir.to_owned()];
    while let Some(d) = pending.pop() {
        let _ = std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o755));
        if let Ok(entries) = std::fs::read_dir(&d) {
            for entry in entries.flatten() {
                if entry.file_type().is_ok_and(|t| t.is_dir()) {
                    pending.push(entry.path());
                }
            }
        }
    }
    // `rm`, not `std::fs::remove_dir_all`: that recurses once per level, and a test
    // that failed may leave a tree tens of thousands of levels deep.
    let status = std::process::Command::new("rm")
        .arg("-rf")
        .arg("--")
        .arg(dir)
        .status()
        .expect("run rm");
    assert!(
        status.success(),
        "remove scratch {}: {status}",
        dir.display()
    );
}

/// Makes `levels` directories nested in `dir` (`d/d/.../d`), with the file `f` holding
/// `bottom` in the deepest. By descriptor: the deepest path is longer than `PATH_MAX`.
pub fn deep(dir: &Path, levels: usize, bottom: &[u8]) {
    use rustix::fs::{Mode, OFlags, mkdirat, openat};
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
    let mut here = openat(rustix::fs::CWD, dir, flags, Mode::empty()).expect("open");
    for _ in 0..levels {
        mkdirat(&here, "d", Mode::from_raw_mode(0o755)).expect("mkdir");
        here = openat(&here, "d", flags, Mode::empty()).expect("open");
    }
    let file = OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC;
    let fd = openat(&here, "f", file, Mode::from_raw_mode(0o644)).expect("create");
    std::io::Write::write_all(&mut std::fs::File::from(fd), bottom).expect("write");
}

/// Keeps CPUs busy until dropped: one spinning thread per CPU, at most four (a hosted
/// runner's count; more would take a shared machine's CPUs from its other work), so a
/// race between processes that is rare on an idle machine shows up within a stress
/// loop.
pub struct CpuHog {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    threads: Vec<std::thread::JoinHandle<()>>,
}

impl CpuHog {
    pub fn start() -> Self {
        use std::sync::atomic::{AtomicBool, Ordering};
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let cpus = std::thread::available_parallelism().map_or(2, |n| n.get().min(4));
        let threads = (0..cpus)
            .map(|_| {
                let stop = std::sync::Arc::clone(&stop);
                std::thread::spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        std::hint::spin_loop();
                    }
                })
            })
            .collect();
        Self { stop, threads }
    }
}

impl Drop for CpuHog {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

/// Whether anything exists at `path` (a dangling symlink counts).
pub fn exists(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}
