//! `linux check-units`: rendered units against the host's priorities, MinIO >
//! kbf-server > kbf-daemon > build containers (docs/deploy/storage-node.md,
//! "Priorities"). Each broken rule is one [`Failure`]; the rules, and the defect each
//! catches:
//!
//! - `render`: no `@KEY@` placeholder is left; with `installable`, no `<...>` one
//!   either (`render-units` without `--kbf-uid` leaves `<kbf uid>`).
//! - `swap`: MinIO and kbf-server have `MemorySwapMax=0` (their pages paged out would
//!   slow every build); the daemon and the slice have none (it would cap the leases'
//!   swap: they live in the daemon's cgroup and the swap is for them).
//! - `oom`: `OOMScoreAdjust` MinIO < kbf-server < kbf-daemon < 0, the containers'
//!   score (a host-wide OOM kills the highest score first), MinIO at least -1000.
//! - `cpu`: `CPUWeight` MinIO > kbf-server > kbf-daemon.
//! - `min`: each `MemoryMin=` is the configured reservation; the slice's is their sum
//!   (the kernel caps a unit's protection at its slice's).
//! - `slice`: all three in `kbf.slice`; the daemon has `Delegate=yes`.
//! - `caps`: no `MemoryMax=`, `MemoryHigh=`, `CPUQuota=` or `AllowedCPUs=` anywhere:
//!   the cap on builds is the daemon's `actions/memory.max`, and CPU is not capped.
//! - `actions`: `--actions-memory-max-gib` is RAM - (MinIO + kbf-server + OS) and
//!   `--supervisor-memory-min-mib` is the daemon's `MemoryMin=`.
//! - `server`: `--cold-store=s3://...` with an `endpoint=` and the key in
//!   `EnvironmentFile=` (none on the command line or in `Environment=`), both
//!   `--reapi-tls-*` flags, and a `--meta-dir` outside the daemon's data directory.
//! - `image`: the daemon's `--image=` values are `KBF_IMAGES`, each as `docker://<entry>`,
//!   and each passes kbf-daemon's own parser (any other value makes it exit 2 at start).
//! - `runtime`: the daemon's unit passes `--container-runtime=oci` and `--oci-runtime=`,
//!   and no file of it names Podman ([`super::runtime`]).
//!
//! Each rule reads a key's last assignment in its section, the one systemd uses.

use std::fmt;
use std::path::Path;

use kbf_types::ImageRef;

use super::config::LinuxConfig;
use super::render::{DAEMON, DROP_IN, SERVER, SLICE};
use super::runtime;
use crate::template::leftovers;
use crate::unit::{UnitError, UnitFile};

/// One broken rule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure {
    pub rule: &'static str,
    pub message: String,
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "FAIL {}: {}", self.rule, self.message)
    }
}

/// Why the units could not be checked at all.
#[derive(Debug, thiserror::Error)]
pub enum CheckError {
    #[error("cannot read {path}: {source}")]
    Read {
        path: String,
        source: std::io::Error,
    },
    #[error("{name}: {source}")]
    Unit { name: String, source: UnitError },
}

/// What the check found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
    pub failures: Vec<Failure>,
    /// Lines about a rule not checked, such as `SKIP parse: ...`; not failures.
    pub notes: Vec<String>,
    /// The summary line: `UNITS OK: ...` or `UNITS BROKEN: ...`.
    pub summary: String,
}

/// One rendered file: its name under the units directory, its text and its directives.
struct File {
    name: String,
    text: String,
    unit: UnitFile,
}

impl File {
    fn new(name: String, text: String) -> Result<File, CheckError> {
        let unit = UnitFile::parse(&text).map_err(|source| CheckError::Unit {
            name: name.clone(),
            source,
        })?;
        Ok(File { name, text, unit })
    }

    fn service(&self, key: &str) -> Option<&str> {
        self.unit.get_in("Service", key)
    }
}

/// Read the four files `render-units` wrote into `dir` and check them.
pub fn check_dir(
    config: &LinuxConfig,
    dir: &Path,
    installable: bool,
) -> Result<Report, CheckError> {
    let read = |name: String| {
        let path = dir.join(&name);
        match std::fs::read_to_string(&path) {
            Ok(text) => File::new(name, text),
            Err(source) => Err(CheckError::Read {
                path: path.display().to_string(),
                source,
            }),
        }
    };
    let slice = read(SLICE.to_owned())?;
    let minio = read(format!("{}.d/{DROP_IN}", config.minio_unit))?;
    let server = read(SERVER.to_owned())?;
    let daemon = read(DAEMON.to_owned())?;
    Ok(check(
        config,
        [&slice, &minio, &server, &daemon],
        installable,
    ))
}

/// Check rendered unit texts: the slice, MinIO's drop-in, kbf-server's and kbf-daemon's
/// unit, by name.
pub fn check_texts(
    config: &LinuxConfig,
    texts: [(String, &str); 4],
    installable: bool,
) -> Result<Report, CheckError> {
    let [s, m, v, d] = texts.map(|(name, text)| File::new(name, text.to_owned()));
    Ok(check(config, [&s?, &m?, &v?, &d?], installable))
}

fn check(c: &LinuxConfig, files: [&File; 4], installable: bool) -> Report {
    let [slice, minio, server, daemon] = files;
    let mut out = Vec::new();
    let mut fail = |rule: &'static str, message: String| out.push(Failure { rule, message });

    // render
    for f in files {
        if let Some(p) = leftovers(&f.text).first() {
            fail("render", format!("{} keeps the placeholder {p}", f.name));
        }
        if let Some(p) = angle_placeholder(&f.text).filter(|_| installable) {
            fail(
                "render",
                format!("{} keeps a <...> placeholder: {p}", f.name),
            );
        }
    }

    // swap
    let swap = |f: &File| f.service("MemorySwapMax").unwrap_or_default().to_owned();
    if swap(minio) != "0" {
        let has = swap(minio);
        fail(
            "swap",
            format!("MinIO's drop-in must set MemorySwapMax=0, has '{has}'"),
        );
    }
    if swap(server) != "0" {
        let has = swap(server);
        fail(
            "swap",
            format!("kbf-server must set MemorySwapMax=0, has '{has}'"),
        );
    }
    if daemon.unit.get("MemorySwapMax").is_some() {
        fail(
            "swap",
            "kbf-daemon sets MemorySwapMax=: its leases may swap (no memory.swap.max per lease)"
                .to_owned(),
        );
    }
    if slice.unit.get("MemorySwapMax").is_some() {
        fail(
            "swap",
            "kbf.slice sets MemorySwapMax=: the daemon's leases are under it and may swap"
                .to_owned(),
        );
    }

    // oom
    let int = |f: &File, key| f.service(key).and_then(|v| v.parse::<i64>().ok());
    let shown = |v: Option<i64>| v.map_or("bad".to_owned(), |n| n.to_string());
    let oom = [minio, server, daemon].map(|f| int(f, "OOMScoreAdjust"));
    let mut oom_line = String::new();
    match oom {
        [Some(m), Some(s), Some(d)] => {
            if m < -1000 {
                fail("oom", format!("MinIO's OOMScoreAdjust={m} is below -1000"));
            }
            if m >= s {
                fail(
                    "oom",
                    format!("MinIO's OOMScoreAdjust={m} must be below kbf-server's {s}"),
                );
            }
            if s >= d {
                fail(
                    "oom",
                    format!("kbf-server's OOMScoreAdjust={s} must be below kbf-daemon's {d}"),
                );
            }
            if d >= 0 {
                fail(
                    "oom",
                    format!(
                        "kbf-daemon's OOMScoreAdjust={d} must be below the build containers' 0"
                    ),
                );
            }
            oom_line = format!("MinIO {m} < kbf-server {s} < kbf-daemon {d} < containers 0");
        }
        [m, s, d] => fail(
            "oom",
            format!(
                "every unit needs an integer OOMScoreAdjust= (MinIO '{}', kbf-server '{}', kbf-daemon '{}')",
                shown(m),
                shown(s),
                shown(d)
            ),
        ),
    }

    // cpu
    match [minio, server, daemon].map(|f| int(f, "CPUWeight")) {
        [Some(m), Some(s), Some(d)] => {
            if m <= s {
                fail(
                    "cpu",
                    format!("MinIO's CPUWeight={m} must be above kbf-server's {s}"),
                );
            }
            if s <= d {
                fail(
                    "cpu",
                    format!("kbf-server's CPUWeight={s} must be above kbf-daemon's {d}"),
                );
            }
        }
        [m, s, d] => fail(
            "cpu",
            format!(
                "every unit needs an integer CPUWeight= (MinIO '{}', kbf-server '{}', kbf-daemon '{}')",
                shown(m),
                shown(s),
                shown(d)
            ),
        ),
    }

    // min
    let min = |f: &File, section| f.unit.get_in(section, "MemoryMin").and_then(mib);
    let shown_mib = |v: Option<u64>| v.map_or("bad".to_owned(), |n| n.to_string());
    let (m_minio, m_server, m_daemon) = (
        min(minio, "Service"),
        min(server, "Service"),
        min(daemon, "Service"),
    );
    let m_slice = min(slice, "Slice");
    if m_minio != Some(c.minio_reservation_gib * 1024) {
        fail(
            "min",
            format!(
                "MinIO's MemoryMin= is {} MiB, not its reservation of {} GiB",
                shown_mib(m_minio),
                c.minio_reservation_gib
            ),
        );
    }
    if m_server != Some(c.server_reservation_gib * 1024) {
        fail(
            "min",
            format!(
                "kbf-server's MemoryMin= is {} MiB, not its reservation of {} GiB",
                shown_mib(m_server),
                c.server_reservation_gib
            ),
        );
    }
    if m_daemon != Some(c.daemon_memory_min_mib) {
        fail(
            "min",
            format!(
                "kbf-daemon's MemoryMin= is {} MiB, not {}",
                shown_mib(m_daemon),
                c.daemon_memory_min_mib
            ),
        );
    }
    if let (Some(a), Some(b), Some(d)) = (m_minio, m_server, m_daemon)
        && m_slice != Some(a + b + d)
    {
        fail(
            "min",
            format!(
                "kbf.slice's MemoryMin= is {} MiB, not the sum of its units' {} MiB",
                shown_mib(m_slice),
                a + b + d
            ),
        );
    }

    // slice
    for f in [minio, server, daemon] {
        if f.service("Slice") != Some("kbf.slice") {
            fail("slice", format!("{} is not in kbf.slice", f.name));
        }
    }
    if daemon.service("Delegate") != Some("yes") {
        fail(
            "slice",
            "kbf-daemon needs Delegate=yes (it makes supervisor/ and actions/ itself)".to_owned(),
        );
    }

    // caps
    for f in files {
        for key in ["MemoryMax", "MemoryHigh", "CPUQuota", "AllowedCPUs"] {
            if f.unit.get(key).is_some() {
                fail("caps", format!("{} sets {key}=", f.name));
            }
        }
    }

    // actions
    let want = c.actions_gib.to_string();
    let got = daemon
        .unit
        .flag("actions-memory-max-gib")
        .unwrap_or_default();
    if got != want {
        fail(
            "actions",
            format!(
                "--actions-memory-max-gib={got}, want {want} (RAM {} - MinIO {} - kbf-server {} - OS {})",
                c.ram_gib, c.minio_reservation_gib, c.server_reservation_gib, c.os_headroom_gib
            ),
        );
    }
    let got = daemon
        .unit
        .flag("supervisor-memory-min-mib")
        .unwrap_or_default();
    if got != shown_mib(m_daemon) {
        fail(
            "actions",
            format!(
                "--supervisor-memory-min-mib={got}, but the daemon's MemoryMin= is {} MiB",
                shown_mib(m_daemon)
            ),
        );
    }

    // server
    let flag = |name| server.unit.flag(name).unwrap_or_default();
    let cold = flag("cold-store");
    if !cold.starts_with("s3://") {
        fail(
            "server",
            "kbf-server needs --cold-store=s3://... (MinIO)".to_owned(),
        );
    } else if !cold.contains("endpoint=") {
        fail("server", "--cold-store has no endpoint= query".to_owned());
    }
    if server.service("EnvironmentFile").is_none_or(str::is_empty) {
        fail(
            "server",
            "kbf-server has no EnvironmentFile= for the S3 key pair".to_owned(),
        );
    }
    if server.unit.exec_start().iter().any(|w| w.contains("AWS_")) {
        fail(
            "server",
            "kbf-server's command line names an AWS_ variable: the key belongs in EnvironmentFile="
                .to_owned(),
        );
    }
    let mut env = server
        .unit
        .directives()
        .iter()
        .filter(|d| d.key == "Environment");
    if env.any(|d| d.value.contains("AWS_")) {
        fail(
            "server",
            "kbf-server sets an AWS_ value in Environment=: the key belongs in EnvironmentFile="
                .to_owned(),
        );
    }
    if flag("reapi-tls-cert").is_empty() || flag("reapi-tls-key").is_empty() {
        fail(
            "server",
            "kbf-server needs --reapi-tls-cert and --reapi-tls-key (the proxy-to-server hop is TLS)"
                .to_owned(),
        );
    }
    let meta = flag("meta-dir");
    if meta.is_empty() {
        fail("server", "kbf-server has no --meta-dir".to_owned());
    } else if Path::new(meta).starts_with(c.data_dir.as_str()) {
        let data = &c.data_dir;
        fail(
            "server",
            format!("--meta-dir={meta} is under the daemon's data directory {data}"),
        );
    }

    // image
    let got = daemon.unit.flag_values("image");
    let want: Vec<String> = c.images.iter().map(|i| format!("docker://{i}")).collect();
    if got != want {
        fail(
            "image",
            format!(
                "kbf-daemon's --image values are '{}', want '{}' (KBF_IMAGES, each as docker://<entry>)",
                got.join(" "),
                want.join(" ")
            ),
        );
    }
    for v in got {
        if let Err(e) = ImageRef::parse(v) {
            fail("image", format!("kbf-daemon would exit 2 at start: {e}"));
        }
    }

    // runtime
    for how in runtime::check(&daemon.name, &daemon.unit, &[(&daemon.name, &daemon.text)]) {
        fail("runtime", how);
    }

    let summary = if out.is_empty() {
        format!(
            "UNITS OK: {oom_line}; swap off for MinIO and kbf-server; builds get {} GiB",
            c.actions_gib
        )
    } else {
        format!("UNITS BROKEN: {} failed check(s)", out.len())
    };
    Report {
        failures: out,
        notes: Vec::new(),
        summary,
    }
}

/// The first `<lower case words>` placeholder in `text`, such as `<kbf uid>`.
pub(super) fn angle_placeholder(text: &str) -> Option<&str> {
    let mut rest = text;
    while let Some(open) = rest.find('<') {
        let after = &rest[open + 1..];
        let words = after.split_once('>').map(|(w, _)| w).filter(|w| {
            w.starts_with(|c: char| c.is_ascii_lowercase())
                && w.chars().all(|c| c.is_ascii_lowercase() || c == ' ')
        });
        if let Some(w) = words {
            return Some(&rest[open..open + w.len() + 2]);
        }
        rest = after;
    }
    None
}

/// A systemd size in MiB: a number with a `K`, `M`, `G` or `T` suffix, or bytes.
fn mib(value: &str) -> Option<u64> {
    let (n, mul, div) = match value.as_bytes().last()? {
        b'K' => (&value[..value.len() - 1], 1, 1024),
        b'M' => (&value[..value.len() - 1], 1, 1),
        b'G' => (&value[..value.len() - 1], 1024, 1),
        b'T' => (&value[..value.len() - 1], 1024 * 1024, 1),
        _ => (value, 1, 1024 * 1024),
    };
    if n.is_empty() || !n.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    n.parse::<u64>().ok()?.checked_mul(mul).map(|n| n / div)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches a `<...>` placeholder missed when another `<` comes first (an XML-ish
    /// comment, `<B>`, `<x1>`), and a `<` with no `>` read as one.
    #[test]
    fn angle_placeholders_are_lower_case_words_in_brackets() {
        assert_eq!(
            angle_placeholder("a <B> <x1> < y> <kbf uid> <z>"),
            Some("<kbf uid>")
        );
        assert_eq!(angle_placeholder("a < b"), None);
        assert_eq!(angle_placeholder("<>"), None);
    }

    /// Catches a size read in the wrong unit (a slice minimum given in bytes or KiB
    /// compared as MiB), and a malformed size read as some number instead of refused.
    #[test]
    fn sizes_are_read_in_mib() {
        let cases = [
            ("512M", Some(512)),
            ("2G", Some(2048)),
            ("1T", Some(1_048_576)),
            ("2048K", Some(2)),
            ("1048576", Some(1)),
            ("", None),
            ("G", None),
            ("1.5G", None),
            ("-1G", None),
            ("99999999999999999999T", None),
        ];
        for (value, want) in cases {
            assert_eq!(mib(value), want, "{value:?}");
        }
    }
}
