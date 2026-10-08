//! Keys, statements and sets the tests share.

use std::collections::BTreeMap;
use std::path::PathBuf;

use sha2::{Digest as _, Sha256};

use crate::set::{Artifact, Pin, Platform, SoftwareSet};
use crate::signed::{Envelope, KeyStatement, SET_CONTEXT, STATEMENT_CONTEXT, public_key};

pub const ROOT_SEED: [u8; 32] = [1; 32];
pub const COMPONENT_SEED: [u8; 32] = [3; 32];
pub const PLATFORM_SEED: [u8; 32] = [4; 32];

/// Lowercase hex SHA-256 of `s`.
pub fn sha(s: &str) -> String {
    hex::encode(Sha256::digest(s.as_bytes()))
}

pub fn pin() -> Pin {
    Pin {
        pool: "linux-x86".into(),
        platform: Platform {
            os: "linux".into(),
            arch: "x86_64".into(),
        },
    }
}

/// A set for [`pin`] naming the daemon (contents `d`) and the updater (contents `u`),
/// expiring at 2,000.
pub fn set(serial: u64) -> SoftwareSet {
    let p = pin();
    SoftwareSet {
        pool: p.pool,
        platform: p.platform,
        serial,
        min_serial: 0,
        expires: 2_000,
        artifacts: BTreeMap::from([
            ("kbf-daemon".to_owned(), Artifact { sha256: sha("d") }),
            ("kbf-updater".to_owned(), Artifact { sha256: sha("u") }),
        ]),
        snapshot: None,
    }
}

/// A statement naming the platform key and `component` keys, expiring at 2,000.
pub fn statement(serial: u64, component: &[[u8; 32]]) -> KeyStatement {
    KeyStatement {
        serial,
        expires: 2_000,
        component_keys: component
            .iter()
            .map(|s| hex::encode(public_key(s)))
            .collect(),
        platform_keys: vec![hex::encode(public_key(&PLATFORM_SEED))],
    }
}

pub fn seal_statement(s: &KeyStatement) -> Envelope {
    Envelope::seal(
        STATEMENT_CONTEXT,
        &serde_json::to_vec(s).unwrap(),
        &ROOT_SEED,
    )
}

pub fn seal_set(s: &SoftwareSet, seed: &[u8; 32]) -> Envelope {
    Envelope::seal(SET_CONTEXT, &serde_json::to_vec(s).unwrap(), seed)
}

/// A fresh, empty directory for one test: `<target>/tmp/ku/<name>`, the directory
/// integration tests get as `CARGO_TARGET_TMPDIR` (unit tests are not given it).
pub fn scratch(name: &str) -> PathBuf {
    let exe = std::env::current_exe().unwrap();
    let target = exe
        .ancestors()
        .nth(3)
        .expect("<target>/<profile>/deps/<test binary>");
    let dir = target.join("tmp").join("ku").join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A scratch directory and a short alias for it, `/proc/<pid>/fd/<n>`, valid while the
/// value lives. A Unix socket path is limited to 107 bytes, which a target directory can
/// exceed (coverage builds nest it deeper). Child processes can use the alias too.
pub struct ShortDir {
    _dir: std::fs::File,
    /// The alias.
    pub path: PathBuf,
}

/// [`scratch`], with a short alias.
pub fn short_dir(name: &str) -> ShortDir {
    use std::os::fd::AsRawFd as _;
    let dir = std::fs::File::open(scratch(name)).unwrap();
    let path = PathBuf::from(format!(
        "/proc/{}/fd/{}",
        std::process::id(),
        dir.as_raw_fd()
    ));
    ShortDir { _dir: dir, path }
}
