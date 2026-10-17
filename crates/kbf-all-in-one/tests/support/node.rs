//! What the tests that run a node binary share: its certificates, the stand-in
//! `podman` and cgroup tree its container driver runs over, and the checks of what is
//! on its disk. Used by this crate's tests/binary.rs and by kbf-cli's
//! tests/storage_and_daemon.rs (through `#[path]`: a test can run only its own
//! package's binaries, so each package that runs a node has a test of its own).

#![allow(dead_code)] // Each test binary uses its own part of this module.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use rcgen::{CertificateParams, CertifiedIssuer, IsCa, KeyPair, SanType};

/// How long the test waits for what should happen promptly.
pub(crate) const PROMPT: Duration = Duration::from_secs(60);

/// The per-architecture manifest the stand-in image store holds.
pub(crate) const MANIFEST: &[u8] =
    br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json"}"#;

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

/// The stand-in `podman` in `bin` (a link to `fake`, the package's build of
/// crates/kbf-driver-container/tests/bin/fake_podman.rs), and `kbf-act` beside it, with
/// an image store holding [`MANIFEST`] as image `img1`; the stand-in runs `steps` as each
/// action. Returns the image as an action names it.
pub(crate) fn podman(bin: &Path, fake: &str, steps: &[&str]) -> String {
    let state = bin.join("state");
    std::fs::create_dir_all(&state).expect("mkdir");
    std::os::unix::fs::symlink(fake, bin.join("podman")).expect("link podman");
    kbf_testkit::act::link_self(&bin.join(kbf_testkit::act::NAME)).expect("link kbf-act");
    std::fs::write(state.join("image-id"), "img1\n").expect("image id");
    std::fs::write(state.join("nonce"), format!("{}:", std::process::id())).expect("nonce");
    let knob: String = steps.iter().map(|s| format!("{s}\n")).collect();
    std::fs::write(state.join("action"), knob).expect("action");
    let digest = format!("sha256:{}", kbf_transfer::digest_of(MANIFEST).hash);
    let store = state.join("store/fake-images/img1");
    std::fs::create_dir_all(&store).expect("mkdir store");
    let file = kbf_driver_container::image::manifest_file(&digest);
    std::fs::write(store.join(file), MANIFEST).expect("manifest");
    format!("docker://registry.test/tools/busybox@{digest}")
}

/// A cgroup v2 mount of plain directories at `bin/cgroup`, where the stand-in `podman`
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

/// An address nothing listens on now, for a worker listener the binary binds itself,
/// that no other test or process is likely to take before the binary does. The port
/// cannot be left to the binary (the daemon half dials it by the `--server` flag), and a
/// port number probed on `127.0.0.1` is free only until a test binding `127.0.0.1:0`, or
/// a connection's source port, takes it. So the address is a loopback address of this
/// call's own (`127.x.y.z`; all of `127.0.0.0/8` is the host's own on Linux) that nothing
/// else binds, with a port probed on it: the one thing that could take it is a process
/// that chose the same address.
pub(crate) fn worker_addr() -> SocketAddr {
    static CALLS: AtomicU32 = AtomicU32::new(0);
    let call = CALLS.fetch_add(1, Ordering::Relaxed);
    assert!(call < 15, "more addresses than one process can tell apart");
    // 23 bits of process and call number under a leading 1 bit: never 127.0.x.y, so
    // never 127.0.0.1, and with a call number under 15, never 127.255.255.255.
    let host = ((std::process::id().wrapping_mul(16) + call) & 0x7F_FFFF) | 0x80_0000;
    let [_, a, b, c] = host.to_be_bytes();
    let port = std::net::TcpListener::bind(SocketAddr::from(([127, a, b, c], 0)))
        .expect("bind")
        .local_addr()
        .expect("addr")
        .port();
    SocketAddr::from(([127, a, b, c], port))
}

/// `PATH` with `bin`, where [`podman`] put the stand-in, first.
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
