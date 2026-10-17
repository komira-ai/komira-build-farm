//! The settings that reach the mode units' flags: the full flag set each mode renders
//! when every setting is given, that each key lands in exactly the units that read it,
//! and the refusals of the keys' values. The unit text itself is the goldens' (and
//! `modes_tests`'s), the flags' meaning is kbf's own parser's
//! (`crates/kbf-cli/tests/deploy_units.rs`).

use super::*;
use crate::unit::UnitFile;

const EXAMPLE: &str = include_str!("../../templates/linux/config.example");

/// The example settings with each `KEY=` line given the value, which must be there once.
fn text_with(edits: &[(&str, &str)]) -> String {
    let mut text = EXAMPLE.to_owned();
    for (key, value) in edits {
        let line = text
            .lines()
            .find(|l| l.starts_with(&format!("{key}=")))
            .unwrap_or_else(|| panic!("config.example has no {key}= line"))
            .to_owned();
        text = text.replacen(&format!("{line}\n"), &format!("{key}={value}\n"), 1);
    }
    text
}

fn config_with(edits: &[(&str, &str)]) -> LinuxConfig {
    LinuxConfig::parse(&text_with(edits)).expect("valid settings")
}

/// The words of the unit's `ExecStart=` after `kbf <mode>`.
fn flags(c: &LinuxConfig, mode: Mode) -> Vec<String> {
    let text = render(c, Some(1500), mode).expect("renders");
    let unit = UnitFile::parse(&text).expect("a unit");
    unit.exec_start()[2..]
        .iter()
        .map(|w| (*w).to_owned())
        .collect()
}

/// Every key a deployment sets, given a value that is not the default.
const ALL_SET: &[(&str, &str)] = &[
    ("KBF_FRONTEND_NAME", "front.example.internal"),
    ("KBF_S3_PREFIX", "farm2"),
    ("KBF_STORAGE_URL", "https://192.0.2.30:9443"),
    ("KBF_REAPI_CERT", "/etc/kbf/front.example.ts.net.crt"),
    ("KBF_REAPI_KEY", "/etc/kbf/front.example.ts.net.key"),
    ("KBF_API_TOKEN_FILE", "/run/secrets/kbf-api-token"),
    ("KBF_STATE_DIR", "/srv/kbf/state"),
    ("KBF_RESERVE_CPUS", "4"),
    ("KBF_LEASE_PLACEMENT", "numa-node"),
];

fn words(list: &[&str]) -> Vec<String> {
    list.iter().map(|w| (*w).to_owned()).collect()
}

/// Each mode's whole command line with every setting given: the frontend's three
/// listeners, certificates, storage URL and SQLite state store; the storage-and-daemon
/// unit's cold store (with the prefix), meta dir, hot store, listener, registration and
/// advertised name; the daemons' frontend name and tuning flags. Catches a flag dropped,
/// renamed or given another key's value (the node's own name where the frontend's
/// belongs), and the frontend unit's ports crossing.
#[test]
fn each_mode_renders_its_full_flag_set() {
    let c = config_with(ALL_SET);
    assert_eq!(
        flags(&c, Mode::Frontend),
        words(&[
            "--listen=192.0.2.10:8980",
            "--reapi-tls-cert=/etc/kbf/front.example.ts.net.crt",
            "--reapi-tls-key=/etc/kbf/front.example.ts.net.key",
            "--worker-listen=192.0.2.10:8981",
            "--worker-tls-cert=/etc/kbf/worker.pem",
            "--worker-tls-key=/etc/kbf/worker.key",
            "--worker-client-ca=/etc/kbf/ca.pem",
            "--api-listen=127.0.0.1:8982",
            "--api-token-file=/run/secrets/kbf-api-token",
            "--storage=https://192.0.2.30:9443",
            "--storage-client-cert=/etc/kbf/worker.pem",
            "--storage-client-key=/etc/kbf/worker.key",
            "--storage-ca=/etc/kbf/ca.pem",
            "--state-store=sqlite:///srv/kbf/state/state.db",
            "--handover-socket=/run/kbf-frontend/control.sock",
        ])
    );
    assert_eq!(
        flags(&c, Mode::StorageAndDaemon),
        words(&[
            "--cold-store=s3://kbf-cas/farm2/?endpoint=http://127.0.0.1:9000&conditional-put=true",
            "--meta-dir=/var/lib/kbf-server/raft",
            "--hot-store=file:///var/lib/kbf-server/hot",
            "--storage-listen=192.0.2.10:8983",
            "--storage-tls-cert=/etc/kbf/storage.pem",
            "--storage-tls-key=/etc/kbf/storage.key",
            "--storage-client-ca=/etc/kbf/ca.pem",
            "--frontend=https://front.example.internal:8981",
            "--advertise-addr=storage-node.example.internal:8983",
            "--driver=container",
            "--server=https://front.example.internal:8981",
            "--ca-cert=/etc/kbf/ca.pem",
            "--cert=/etc/kbf/node.pem",
            "--key=/etc/kbf/node.key",
            "--node-id=storage-node-1",
            "--data-dir=/var/lib/kbf",
            "--reserve-cpus=4",
            "--lease-placement=numa-node",
            "--actions-memory-max-gib=200",
            "--supervisor-memory-min-mib=256",
        ])
    );
    assert_eq!(
        flags(&c, Mode::Daemon),
        words(&[
            "--driver=container",
            "--server=https://front.example.internal:8981",
            "--cas=https://front.example.internal:8981",
            "--ca-cert=/etc/kbf/ca.pem",
            "--cert=/etc/kbf/node.pem",
            "--key=/etc/kbf/node.key",
            "--node-id=storage-node-1",
            "--data-dir=/var/lib/kbf",
            "--reserve-cpus=4",
            "--lease-placement=numa-node",
            "--actions-memory-max-gib=200",
            "--supervisor-memory-min-mib=256",
        ])
    );
    assert_eq!(
        flags(&c, Mode::Storage),
        words(&[
            "--cold-store=s3://kbf-cas/farm2/?endpoint=http://127.0.0.1:9000&conditional-put=true",
            "--hot-store=file:///var/lib/kbf-server/hot",
            "--meta-dir=/var/lib/kbf-server/raft",
            "--storage-listen=192.0.2.10:8983",
            "--storage-tls-cert=/etc/kbf/storage.pem",
            "--storage-tls-key=/etc/kbf/storage.key",
            "--storage-client-ca=/etc/kbf/ca.pem",
        ])
    );
    let all_in_one = flags(&c, Mode::AllInOne);
    for want in [
        "--reapi-tls-cert=/etc/kbf/front.example.ts.net.crt",
        "--reapi-tls-key=/etc/kbf/front.example.ts.net.key",
        "--api-token-file=/run/secrets/kbf-api-token",
        "--reserve-cpus=4",
        "--lease-placement=numa-node",
        // The all-in-one is the frontend: its daemon half dials itself by its own name.
        "--server=https://storage-node.example.internal:8981",
    ] {
        assert!(
            all_in_one.iter().any(|w| w == want),
            "{want}: {all_in_one:?}"
        );
    }
}

/// Each new key against every mode: the keys render into exactly the units that read
/// them and no other unit's text moves. Catches a key parsed and then rendered nowhere
/// (a dead setting that an operator believes in), one rendered into a unit that does not
/// read it, and a unit that stops carrying it.
#[test]
fn each_new_key_reaches_exactly_the_units_that_read_it() {
    use Mode::{AllInOne, Daemon, Frontend, Storage, StorageAndDaemon};
    let rows: [(&str, &str, &[Mode]); 9] = [
        (
            "KBF_FRONTEND_NAME",
            "other.example.internal",
            &[Daemon, StorageAndDaemon],
        ),
        ("KBF_S3_PREFIX", "other", &[Storage, StorageAndDaemon]),
        ("KBF_STORAGE_URL", "https://192.0.2.99:9443", &[Frontend]),
        (
            "KBF_REAPI_CERT",
            "/etc/kbf/other.crt",
            &[Frontend, AllInOne],
        ),
        ("KBF_REAPI_KEY", "/etc/kbf/other.key", &[Frontend, AllInOne]),
        (
            "KBF_API_TOKEN_FILE",
            "/etc/kbf/other-token",
            &[Frontend, AllInOne],
        ),
        ("KBF_STATE_DIR", "/srv/other", &[Frontend]),
        (
            "KBF_RESERVE_CPUS",
            "7",
            &[Daemon, StorageAndDaemon, AllInOne],
        ),
        (
            "KBF_LEASE_PLACEMENT",
            "none",
            &[Daemon, StorageAndDaemon, AllInOne],
        ),
    ];
    let base = config_with(&[]);
    for (key, value, reads) in rows {
        let c = config_with(&[(key, value)]);
        for mode in Mode::ALL {
            let before = render(&base, Some(1500), mode).expect("renders");
            let after = render(&c, Some(1500), mode).expect("renders");
            let carries = after.contains(&value.replace("https://", "")) || after.contains(value);
            if reads.contains(&mode) {
                assert_ne!(before, after, "{key} does not move the {mode} unit");
                assert!(carries, "{key}={value} is not in the {mode} unit:\n{after}");
            } else {
                assert_eq!(before, after, "{key} moves the {mode} unit");
            }
        }
    }
}

/// The node's own name and the frontend's are two settings: `KBF_HOST_NAME` is what the
/// node advertises and `KBF_FRONTEND_NAME` what it dials; unset, the frontend is this
/// host. Catches the daemon dialing its own address (the bug this key fixes: nothing
/// listens there, so it never registers) and the advertised name following the frontend's.
#[test]
fn the_frontend_name_is_not_the_nodes_own() {
    let c = config_with(&[
        ("KBF_HOST_NAME", "192.0.2.14"),
        ("KBF_FRONTEND_NAME", "front.example.internal"),
    ]);
    for mode in [Mode::Daemon, Mode::StorageAndDaemon] {
        let f = flags(&c, mode);
        assert!(
            f.contains(&"--server=https://front.example.internal:8981".to_owned()),
            "{mode}: {f:?}"
        );
        assert!(
            !f.iter().any(|w| w.contains("192.0.2.14:8981")),
            "{mode}: {f:?}"
        );
    }
    let f = flags(&c, Mode::StorageAndDaemon);
    assert!(f.contains(&"--frontend=https://front.example.internal:8981".to_owned()));
    assert!(f.contains(&"--advertise-addr=192.0.2.14:8983".to_owned()));
    // Left out, the frontend is this host.
    let text = text_with(&[("KBF_HOST_NAME", "192.0.2.14")])
        .replace("KBF_FRONTEND_NAME=frontend.example.internal\n", "");
    let c = LinuxConfig::parse(&text).expect("valid settings");
    assert_eq!(c.frontend_name, "192.0.2.14");
    assert!(flags(&c, Mode::Daemon).contains(&"--server=https://192.0.2.14:8981".to_owned()));
}

/// The values the new keys refuse, and the ones they take. Catches a placement outside
/// `none` and `numa-node` reaching a unit kbf refuses at start, a CPU count that is not
/// a whole number, a prefix that would escape or break the cold store's URI, a storage
/// URL that is not https (the frontend's mutual TLS), the frontend unit rendered with no
/// storage to call, and an unknown key accepted.
#[test]
fn the_new_keys_refuse_values_a_unit_cannot_carry() {
    let refuses = |key: &str, value: &str| {
        LinuxConfig::parse(&text_with(&[(key, value)])).expect_err(&format!("{key}={value}"))
    };
    assert_eq!(
        refuses("KBF_LEASE_PLACEMENT", "numa").to_string(),
        "KBF_LEASE_PLACEMENT=numa: must be none or numa-node"
    );
    refuses("KBF_LEASE_PLACEMENT", "NUMA-NODE");
    for bad in ["four", "-1", "1.5", "4 5", "0x4"] {
        assert!(
            matches!(
                refuses("KBF_RESERVE_CPUS", bad),
                LinuxError::NotNumber { .. }
            ),
            "{bad}"
        );
    }
    for bad in [
        "/farm", "a//b", "a/../b", "./a", "a b", "a?b", "a%2fb", "a@b", "..",
    ] {
        assert!(
            matches!(
                refuses("KBF_S3_PREFIX", bad),
                LinuxError::S3Prefix { .. } | LinuxError::Chars { .. }
            ),
            "{bad}"
        );
    }
    for bad in ["http://192.0.2.30:8983", "192.0.2.30:8983", "https://"] {
        assert!(
            matches!(refuses("KBF_STORAGE_URL", bad), LinuxError::StorageUrl(_)),
            "{bad}"
        );
    }
    let state = refuses("KBF_STATE_DIR", "relative/dir");
    assert!(
        matches!(
            state,
            LinuxError::Path {
                key: "KBF_STATE_DIR",
                ..
            }
        ),
        "{state}"
    );
    // Taken: each placement, a zero, a nested prefix with or without its slash.
    for (key, value) in [
        ("KBF_LEASE_PLACEMENT", "none"),
        ("KBF_LEASE_PLACEMENT", "numa-node"),
        ("KBF_RESERVE_CPUS", "0"),
        ("KBF_S3_PREFIX", "farm2/cas-1.x_y"),
    ] {
        config_with(&[(key, value)]);
    }
    assert_eq!(
        config_with(&[("KBF_S3_PREFIX", "farm2")]).s3_prefix,
        "farm2/"
    );
    assert_eq!(
        config_with(&[("KBF_S3_PREFIX", "farm2/")]).s3_prefix,
        "farm2/"
    );
    assert_eq!(config_with(&[("KBF_S3_PREFIX", "a/b")]).s3_prefix, "a/b/");
    // Empty is the default everywhere.
    let c = config_with(&[]);
    assert_eq!(
        (c.s3_prefix.as_str(), c.reserve_cpus, c.lease_placement),
        ("", None, None)
    );
    assert_eq!(c.reapi_cert.as_str(), "/etc/kbf/reapi.pem");
    assert_eq!(c.reapi_key.as_str(), "/etc/kbf/reapi.key");
    // The REAPI pair follows KBF_ETC when it is not named.
    let moved = config_with(&[("KBF_ETC", "/srv/etc")]);
    assert_eq!(moved.reapi_cert.as_str(), "/srv/etc/reapi.pem");
    // So does the operator token: the path, not the token, is what a unit carries.
    assert_eq!(c.api_token_file.as_str(), "/etc/kbf/api-token");
    assert_eq!(moved.api_token_file.as_str(), "/srv/etc/api-token");
    for mode in [Mode::Frontend, Mode::AllInOne] {
        assert!(
            flags(&moved, mode).contains(&"--api-token-file=/srv/etc/api-token".to_owned()),
            "{mode}"
        );
    }
    let relative = refuses("KBF_API_TOKEN_FILE", "api-token");
    assert!(
        relative.to_string().contains("KBF_API_TOKEN_FILE"),
        "{relative}"
    );

    // The frontend needs its storage; the others do not.
    let none = LinuxConfig::parse(&text_with(&[("KBF_STORAGE_URL", "")])).expect("valid");
    let e = render(&none, Some(1500), Mode::Frontend).expect_err("no storage URL");
    assert_eq!(
        e.to_string(),
        "KBF_STORAGE_URL must be set for --mode frontend: `kbf frontend` requires --storage, a storage node's https:// URL"
    );
    for mode in [
        Mode::Storage,
        Mode::Daemon,
        Mode::StorageAndDaemon,
        Mode::AllInOne,
    ] {
        render(&none, Some(1500), mode).expect("renders without a storage URL");
    }
    // The key set stays closed.
    let e = LinuxConfig::parse(&format!("{EXAMPLE}KBF_PLANTED=1\n")).expect_err("unknown key");
    assert!(e.to_string().contains("unknown key KBF_PLANTED"), "{e}");
    let e = LinuxConfig::parse(&format!("{EXAMPLE}KBF_S3_PREFIX=a\n")).expect_err("repeated key");
    assert!(e.to_string().contains("KBF_S3_PREFIX"), "{e}");
}

/// Every key `config.example` sets is in the closed set and every key of the set is in
/// the example: a key added to one and not the other is a setting no operator can find or
/// one the loader refuses.
#[test]
fn the_example_documents_exactly_the_closed_key_set() {
    let in_example: std::collections::BTreeSet<&str> = EXAMPLE
        .lines()
        .filter(|l| !l.starts_with('#'))
        .filter_map(|l| l.split_once('=').map(|(k, _)| k))
        .collect();
    let in_set: std::collections::BTreeSet<&str> =
        crate::linux::config::KEYS.iter().copied().collect();
    assert_eq!(in_example, in_set);
}

/// On the node, `storage-and-daemon` may name its key file and nothing else that sets an
/// `AWS_*` variable: a copy of the key under another name is refused as for a build node,
/// while the key file itself (root's, 0600) is not read as a copy and not reported twice.
/// Catches the unit's other environment files going unread on the node.
#[test]
fn on_the_node_a_copy_of_the_key_beside_the_allowed_file_is_refused() {
    use kbf_host::exec::Entry;
    use kbf_host::step::{Owner, Perms};
    use kbf_host::testkit::FakeExec;

    let c = config_with(&[]);
    let unit = render(&c, Some(1500), Mode::StorageAndDaemon)
        .expect("renders")
        .replace(
            "EnvironmentFile=/etc/kbf/s3.env\n",
            "EnvironmentFile=/etc/kbf/s3.env\nEnvironmentFile=/etc/kbf/copy.env\n",
        );
    let slice = UnitText::new(
        SLICE,
        crate::linux::render::render(&c, Some(1500), &crate::linux::render::Templates::BUILT_IN)
            .expect("renders")
            .slice,
    );
    let mut exec = FakeExec::new(0);
    let file = |bits: u32, uid: u32, text: &str| Entry::File {
        perms: Perms::new(bits),
        owner: Owner { uid, gid: uid },
        bytes: text.as_bytes().to_vec(),
    };
    for (path, entry) in [
        ("/usr/local/bin/kbf", file(0o755, 0, "")),
        ("/etc/kbf/s3.env", file(0o600, 0, "AWS_ACCESS_KEY_ID=a\n")),
        (
            "/etc/kbf/copy.env",
            file(0o600, 0, "AWS_SECRET_ACCESS_KEY=s\n"),
        ),
    ] {
        exec.entries
            .insert(kbf_host::HostPath::new(path).expect("absolute"), entry);
    }
    let r = check_on_host(
        &exec,
        &c,
        Mode::StorageAndDaemon,
        &UnitText::new(Mode::StorageAndDaemon.unit_name(), unit),
        &slice,
        true,
        None,
    )
    .expect("runs");
    let credential: Vec<String> = r
        .failures
        .iter()
        .filter(|f| f.rule == "credential")
        .map(ToString::to_string)
        .collect();
    assert_eq!(
        credential,
        [
            "FAIL credential: kbf-storage-and-daemon.service gives a build node the cold store's key (EnvironmentFile=/etc/kbf/copy.env, which sets AWS_SECRET_ACCESS_KEY): a build node holds no credential, and its builds run as its user"
        ]
    );
}
