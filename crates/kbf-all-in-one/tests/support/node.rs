//! What the tests that run a node binary share: its certificates, the stand-in
//! `crun` and cgroup tree its container driver runs over, and the checks of what is
//! on its disk. Used by this crate's tests/binary.rs and by kbf-cli's
//! tests/storage_and_daemon.rs (through `#[path]`: a test can run only its own
//! package's binaries, so each package that runs a node has a test of its own).

#![allow(dead_code)] // Each test binary uses its own part of this module.

use std::path::{Path, PathBuf};
use std::process::Child;
use std::time::Duration;

use rcgen::{CertificateParams, CertifiedIssuer, IsCa, KeyPair, SanType};

/// How long the test waits for what should happen promptly.
pub(crate) const PROMPT: Duration = Duration::from_secs(60);

/// The per-architecture manifest whose digest names the image the node's store holds.
pub(crate) const MANIFEST: &[u8] =
    br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json"}"#;

/// Where tests that run a container daemon put their directories: Cargo's per-target
/// temporary directory, unless an ancestor of it is closed to other users (a home
/// directory often is), which the container driver refuses (its containers' ids are not
/// the daemon's, and must be able to search down to the data directory); then the
/// system's temporary directory.
pub(crate) fn scratch_root() -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt as _;
    let target = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    let searchable = std::fs::canonicalize(&target).is_ok_and(|real| {
        real.ancestors()
            .all(|a| std::fs::metadata(a).is_ok_and(|m| m.permissions().mode() & 0o001 != 0))
    });
    if searchable {
        target
    } else {
        std::env::temp_dir()
    }
}

/// A CA, the node's certificate (`node-1`, its one DNS name, and the daemon role its
/// worker session needs on the worker listener), the server's (`localhost`), and a
/// storage client's (`client.pem`, naming the role `frontend` as a storage listener
/// under mutual TLS requires), written into `dir`.
pub(crate) fn pki(dir: &Path) {
    let mut ca = CertificateParams::new(Vec::<String>::new()).expect("CA params");
    ca.is_ca = IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let ca = CertifiedIssuer::self_signed(ca, KeyPair::generate().expect("key")).expect("CA");
    std::fs::write(dir.join("ca.pem"), ca.pem()).expect("write");
    for (name, dns) in [("node", "node-1"), ("server", "localhost")] {
        let key = KeyPair::generate().expect("key");
        let mut params = CertificateParams::new(vec![dns.to_owned()]).expect("params");
        if name == "node" {
            let role = format!("{}daemon", kbf_storage::service::ROLE_URI_PREFIX);
            params
                .subject_alt_names
                .push(SanType::URI(role.try_into().expect("IA5")));
        }
        let cert = params.signed_by(&key, &ca).expect("sign");
        std::fs::write(dir.join(format!("{name}.pem")), cert.pem()).expect("write");
        std::fs::write(dir.join(format!("{name}.key")), key.serialize_pem()).expect("write");
    }
    let mut client = CertificateParams::new(Vec::<String>::new()).expect("params");
    let role = format!("{}frontend", kbf_storage::service::ROLE_URI_PREFIX);
    client.subject_alt_names = vec![SanType::URI(role.try_into().expect("IA5"))];
    let key = KeyPair::generate().expect("key");
    let cert = client.signed_by(&key, &ca).expect("sign");
    std::fs::write(dir.join("client.pem"), cert.pem()).expect("write");
    std::fs::write(dir.join("client.key"), key.serialize_pem()).expect("write");
}

/// `--id-files`: a passwd naming this test's user `kbf`, with 65,536 subordinate ids.
pub(crate) fn id_files(dir: &Path) -> PathBuf {
    use std::os::unix::fs::MetadataExt as _;

    let ids = dir.join("ids");
    std::fs::create_dir_all(&ids).expect("mkdir");
    let passwd = ids.join("passwd");
    std::fs::write(&passwd, "").expect("write");
    let uid = std::fs::metadata(&passwd).expect("stat").uid();
    std::fs::write(&passwd, format!("kbf:x:{uid}:{uid}::/:/usr/bin/false\n")).expect("write");
    for file in ["subuid", "subgid"] {
        std::fs::write(ids.join(file), "kbf:100000:65536\n").expect("write");
    }
    ids
}

/// The stand-in `crun` in `bin` (a link to `fake`, the package's build of
/// crates/kbf-driver-container/tests/bin/fake_crun.rs), `kbf-act` beside it, the id tools
/// the OCI backend's start-up looks for on `PATH` (setuid files, as `uidmap` installs
/// them) and a `podman` that fails and records that it ran (the same program as the
/// stand-in: [`podman_ran`]), with the image [`MANIFEST`] names already unpacked in the
/// node's image store under `data` (the node's `--data-dir`); the stand-in runs `steps`
/// as each action. Returns the image as an action names it.
pub(crate) fn crun(bin: &Path, data: &Path, fake: &str, steps: &[&str]) -> String {
    use std::os::unix::fs::PermissionsExt as _;
    let state = bin.join("crun-state");
    std::fs::create_dir_all(&state).expect("mkdir");
    std::os::unix::fs::symlink(fake, bin.join("crun")).expect("link crun");
    std::os::unix::fs::symlink(fake, bin.join("podman")).expect("link podman");
    for tool in ["newuidmap", "newgidmap"] {
        let path = bin.join(tool);
        std::fs::write(&path, b"").expect("write the tool");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o4755)).expect("chmod");
    }
    kbf_testkit::act::link_self(&bin.join(kbf_testkit::act::NAME)).expect("link kbf-act");
    std::fs::write(state.join("nonce"), format!("{}:", std::process::id())).expect("nonce");
    let knob: String = steps.iter().map(|s| format!("{s}\n")).collect();
    std::fs::write(state.join("action"), knob).expect("action");
    let hex = kbf_transfer::digest_of(MANIFEST).hash;
    let rootfs = data.join("leases/oci-images").join(&hex).join("rootfs");
    std::fs::create_dir_all(rootfs.join("kbf/root")).expect("an unpacked image");
    format!("docker://registry.test/tools/busybox@sha256:{hex}")
}

/// The flag that points the daemon at the stand-in `crun` [`crun`] put in `bin`.
pub(crate) fn oci_runtime(bin: &Path) -> String {
    format!("--oci-runtime={}", bin.join("crun").display())
}

/// What the `podman` of `bin` recorded: nothing, on a daemon that never runs it.
pub(crate) fn podman_ran(bin: &Path) -> String {
    std::fs::read_to_string(bin.join("podman-ran")).unwrap_or_default()
}

/// A cgroup v2 mount of plain directories at `bin/cgroup`, where the stand-in `crun`
/// looks, holding the daemon's unit; the flags that point the daemon at it.
pub(crate) fn cgroup_tree(bin: &Path) -> Vec<String> {
    let root = bin.join("cgroup");
    let unit = root.join("kbf.slice/kbf-all-in-one.service");
    std::fs::create_dir_all(&unit).expect("mkdir");
    let plant = |path: PathBuf, text: &str| std::fs::write(path, text).expect("plant");
    plant(
        root.join("cgroup.controllers"),
        "cpuset cpu io memory pids\n",
    );
    plant(unit.join("cgroup.controllers"), "cpu memory pids\n");
    plant(unit.join("cgroup.procs"), "1\n");
    plant(unit.join("cpuset.cpus.effective"), "0\n");
    let own = bin.join("self-cgroup");
    plant(own.clone(), "0::/kbf.slice/kbf-all-in-one.service\n");
    vec![
        format!("--cgroup-root={}", root.display()),
        format!("--self-cgroup={}", own.display()),
    ]
}

/// A port nothing listens on now.
pub(crate) fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("addr")
        .port()
}

/// `PATH` with `bin`, where [`crun`] put the id tools and the failing `podman`, first.
pub(crate) fn image_path(bin: &Path) -> String {
    format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    )
}

/// Kills the process when dropped, so a failed test leaves nothing running.
pub(crate) struct Running(pub(crate) Child);

impl Running {
    /// Waits for the process to end on its own, for at most [`PROMPT`].
    pub(crate) fn wait(mut self) -> std::process::ExitStatus {
        let deadline = std::time::Instant::now() + PROMPT;
        loop {
            if let Some(status) = self.0.try_wait().expect("wait") {
                return status;
            }
            assert!(std::time::Instant::now() < deadline, "still running");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// An input of more bytes than any chunk or buffer of the read path (a ByteStream chunk
/// and the input cache's copy buffer are 1 MiB), none of whose 64-byte runs repeats.
pub(crate) fn input_bytes() -> Vec<u8> {
    let mut state = 0x9e37_79b9_7f4a_7c15_u64;
    (0..(3 << 20) + 7)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state.to_le_bytes()[3]
        })
        .collect()
}

/// Every file under `dirs` (recursively, not following symlinks) that holds `bytes`
/// whole, each inode once: its path and size.
pub(crate) fn files_holding(dirs: &[PathBuf], bytes: &[u8]) -> Vec<(PathBuf, u64)> {
    use std::os::unix::fs::MetadataExt as _;
    let mut found = std::collections::BTreeMap::new();
    let mut todo: Vec<PathBuf> = dirs.to_vec();
    while let Some(at) = todo.pop() {
        let Ok(meta) = std::fs::symlink_metadata(&at) else {
            continue;
        };
        if meta.is_dir() {
            let listed = std::fs::read_dir(&at).into_iter().flatten().flatten();
            todo.extend(listed.map(|entry| entry.path()));
            continue;
        }
        if !meta.is_file() || meta.len() < bytes.len() as u64 {
            continue;
        }
        let Ok(held) = std::fs::read(&at) else {
            continue;
        };
        let anchor = &bytes[..64];
        let holds = held
            .windows(64)
            .enumerate()
            .filter(|(_, run)| *run == anchor)
            .any(|(at, _)| held.get(at..at + bytes.len()) == Some(bytes));
        if holds {
            found
                .entry((meta.dev(), meta.ino()))
                .or_insert((at, meta.len()));
        }
    }
    found.into_values().collect()
}
