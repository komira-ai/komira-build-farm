//! Rules `storage`, `state-store` and `ports` read on their own: how a flag and a
//! variable are read, which `--state-store` values pass (a SQLite file by an absolute
//! path, nothing else), and which listeners share a port.

use std::path::Path;

use super::*;

fn unit(text: &str) -> UnitFile {
    UnitFile::parse(text).expect("a unit")
}

fn words(line: &str) -> Vec<String> {
    credential::words(line)
}

const KEY: &str = "/etc/kbf/s3.env";

/// The storage node's flags, as the template renders them.
const FULL: &str = "--cold-store=s3://kbf-cas?endpoint=http://127.0.0.1:9000 \
    --meta-dir=/var/lib/kbf-server/raft --storage-listen=192.0.2.10:8983 \
    --storage-tls-cert=/etc/kbf/storage.pem --storage-tls-key=/etc/kbf/storage.key \
    --storage-client-ca=/etc/kbf/ca.pem";

fn storage_unit(flags: &str, env: &str) -> UnitFile {
    unit(&format!(
        "[Service]\n{env}ExecStart=/usr/local/bin/kbf storage {flags}\n"
    ))
}

/// Catches a flag read only in its `=` form (clap takes `--name value` too), the first
/// of two values read where kbf reads the last, and a longer flag (`--state-store-x`)
/// taken for the one asked about.
#[test]
fn a_flag_is_read_in_both_forms_and_the_last_wins() {
    let w = words("kbf storage --meta-dir /a --meta-dir=/b --meta-dir-x=/c");
    assert_eq!(flag(&w, "meta-dir"), Some("/b"));
    let w = words("kbf storage --meta-dir=/a \"--meta-dir\" '/b'");
    assert_eq!(flag(&w, "meta-dir"), Some("/b"));
    assert_eq!(flag(&words("kbf --state-store-x=/c"), "state-store"), None);
    assert_eq!(flag(&words("kbf --state-store"), "state-store"), None);
}

/// Catches each of the six flags dropped from a storage node's unit (one red per flag
/// planted), a `--cold-store` read only in its `=` form, an `s3://` cold store without
/// the key file passed, and a `file://` cold store asked for one.
#[test]
fn a_storage_unit_names_its_stores_listener_and_tls_and_its_key() {
    let env = format!("EnvironmentFile={KEY}\n");
    let key = Path::new(KEY);
    assert_eq!(
        storage(&storage_unit(FULL, &env), key, false),
        Vec::<String>::new()
    );
    for f in STORAGE_FLAGS {
        let dropped: Vec<&str> = FULL
            .split_whitespace()
            .filter(|w| !w.starts_with(&format!("--{f}=")))
            .collect();
        let got = storage(&storage_unit(&dropped.join(" "), &env), key, false);
        assert_eq!(got.len(), 1, "--{f}: {got:?}");
        assert!(got[0].starts_with(&format!("gives no --{f}:")), "{got:?}");
    }
    let empty = FULL.replace("--meta-dir=/var/lib/kbf-server/raft", "--meta-dir=");
    assert_eq!(storage(&storage_unit(&empty, &env), key, false).len(), 1);
    let spaced = FULL.replace("--cold-store=", "--cold-store ");
    assert_eq!(
        storage(&storage_unit(&spaced, &env), key, false),
        Vec::<String>::new()
    );
    assert_eq!(
        storage(&storage_unit(FULL, ""), key, false),
        [
            "has an s3:// cold store and no EnvironmentFile=/etc/kbf/s3.env: kbf storage starts without the bucket's key pair and refuses"
        ]
    );
    // The key given through an optional, quoted file still counts.
    let quoted = format!("EnvironmentFile=-\"{KEY}\"\n");
    assert_eq!(
        storage(&storage_unit(FULL, &quoted), key, false),
        Vec::<String>::new()
    );
    let file = FULL.replace(
        "s3://kbf-cas?endpoint=http://127.0.0.1:9000",
        "file:///var/lib/kbf/cold",
    );
    assert_eq!(
        storage(&storage_unit(&file, ""), key, false),
        Vec::<String>::new()
    );
}

/// Catches a variable not found in either of systemd's spellings, an unclosed brace
/// or a bare `$` read as a name, and a name read past its last letter.
#[test]
fn a_state_store_value_names_its_variable() {
    let rows = [
        ("${KBF_STATE_STORE}", Some("KBF_STATE_STORE")),
        ("$KBF_STATE_STORE", Some("KBF_STATE_STORE")),
        ("x${A}y", Some("A")),
        ("$A/b", Some("A")),
        ("${A", None),
        ("$", None),
        ("${}", None),
        ("sqlite:///var/lib/kbf/state.db", None),
    ];
    for (value, want) in rows {
        assert_eq!(variable(value), want, "{value}");
    }
}

fn frontend(service: &str, flags: &str) -> UnitFile {
    unit(&format!(
        "[Service]\n{service}ExecStart=/usr/local/bin/kbf frontend {flags}\n"
    ))
}

const POSTGRES: &str = "--state-store is a postgres URI: postgres is not wired yet; the state store is sqlite:///<absolute path>";
const NOT_SQLITE: &str =
    "--state-store is not sqlite:///<absolute path>: SQLite is the only state store kbf opens";
const BAD_PATH: &str = "--state-store is sqlite:// with an empty, relative or not normal path: give sqlite:///<absolute path>";

/// Catches a SQLite file by an absolute path refused (in either flag form, quoted), a
/// unit with no `--state-store` failed, and a Postgres URI passed in either scheme, with
/// or without a password, in either flag form or as the last of two values (the planted
/// mutant: the postgres branch returning `None` turns this red). No message holds the
/// value.
#[test]
fn the_state_store_is_a_sqlite_file_and_postgres_is_not_wired_yet() {
    for flags in [
        "--state-store=sqlite:///var/lib/kbf/state.db",
        "--state-store sqlite:///var/lib/kbf/state.db",
        "\"--state-store=sqlite:///var/lib/kbf/state.db\"",
        "--state-store=postgres://kbf@db/kbf --state-store=sqlite:///s.db",
        "",
    ] {
        assert_eq!(
            state_store(&frontend("", flags)),
            Vec::<String>::new(),
            "{flags}"
        );
    }
    for flags in [
        "--state-store=postgres://kbf@db/kbf",
        "--state-store=postgresql://kbf@db.example:5432/kbf",
        "--state-store=postgres://kbf:secret@db/kbf",
        "--state-store postgres://db/kbf?password=secret",
        "--state-store=sqlite:///s.db --state-store=postgres://kbf@db/kbf",
    ] {
        let got = state_store(&frontend("", flags));
        assert_eq!(got, [POSTGRES], "{flags}");
        assert!(!got[0].contains("secret") && !got[0].contains("db/kbf"));
    }
}

/// Catches a value of another scheme, or none, passed (or taken for postgres, or for a
/// bad SQLite path), and a SQLite path that is empty, relative or not normal passed.
#[test]
fn any_other_scheme_and_a_relative_or_empty_sqlite_path_fail() {
    for value in [
        "",
        "state.db",
        "/var/lib/kbf/state.db",
        "mysql://kbf@db/kbf",
        "file:///var/lib/kbf/state.db",
        "sqlite:/var/lib/kbf/state.db",
        "postgres:/db/kbf",
        "SQLITE:///var/lib/kbf/state.db",
    ] {
        assert_eq!(
            state_store(&frontend("", &format!("--state-store={value}"))),
            [NOT_SQLITE],
            "{value}"
        );
    }
    for value in [
        "sqlite://",
        "sqlite:///",
        "sqlite://state.db",
        "sqlite://var/lib/kbf/state.db",
        "sqlite:////var/lib/kbf/state.db",
        "sqlite:///var/lib/../kbf/state.db",
    ] {
        assert_eq!(
            state_store(&frontend("", &format!("--state-store={value}"))),
            [BAD_PATH],
            "{value}"
        );
    }
}

/// Catches `--state-store=${VAR}` passed (with or without a file to set it: the
/// expanded value is in kbf's argv either way, and the rule cannot read it), in either
/// spelling, inside an otherwise good SQLite value, and a variable taken for a scheme.
#[test]
fn a_state_store_that_names_a_variable_fails() {
    let filed = "EnvironmentFile=/etc/kbf/state-store.env\n";
    let refused = |var: &str| {
        format!(
            "--state-store names ${{{var}}}: systemd expands it into kbf's command line, which every user on the node can read in /proc, and the rule cannot read the store it names; write sqlite:///<absolute path>"
        )
    };
    for (service, value, var) in [
        (filed, "${KBF_STATE_STORE}", "KBF_STATE_STORE"),
        ("", "$KBF_STATE_STORE", "KBF_STATE_STORE"),
        ("", "sqlite://${KBF_DATA_DIR}/state.db", "KBF_DATA_DIR"),
        ("", "postgres://$PGUSER@db/kbf", "PGUSER"),
    ] {
        assert_eq!(
            state_store(&frontend(service, &format!("--state-store={value}"))),
            [refused(var)],
            "{value}"
        );
    }
}

/// Catches two listeners on one port passed (the first and the second, the first and
/// the last, the second form `--flag value`), and listeners on their own ports, or a
/// unit with none, failed.
#[test]
fn no_two_listeners_share_a_port() {
    let ports_of = |flags: &str| ports(&frontend("", flags));
    assert_eq!(
        ports_of("--listen=a:1 --worker-listen=a:2 --api-listen=127.0.0.1:3 --storage-listen=a:4"),
        Vec::<String>::new()
    );
    assert_eq!(ports_of(""), Vec::<String>::new());
    assert_eq!(
        ports_of("--listen=noport --api-listen=a:1"),
        Vec::<String>::new()
    );
    let tail = "give each listener its own (KBF_REAPI_PORT, KBF_WORKER_PORT, KBF_API_PORT, KBF_STORAGE_PORT)";
    assert_eq!(
        ports_of("--listen=a:1 --worker-listen [::1]:1"),
        [format!(
            "--listen and --worker-listen are both on port 1: {tail}"
        )]
    );
    assert_eq!(
        ports_of("--listen=a:1 --worker-listen=a:2 --api-listen=127.0.0.1:3 --storage-listen=b:1"),
        [format!(
            "--listen and --storage-listen are both on port 1: {tail}"
        )]
    );
}
