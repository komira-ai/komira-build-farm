//! The one command line of both halves.

use clap::error::ErrorKind;

use super::*;

/// The flags every command line here gives: the daemon's required ones.
const BASE: &[&str] = &[
    "kbf-all-in-one",
    "--server=https://127.0.0.1:8981",
    "--ca-cert=ca.pem",
    "--cert=node.pem",
    "--key=node.key",
    "--node-id=node-1",
];

fn flags(extra: &[&str]) -> Result<Flags, clap::Error> {
    let argv = BASE.iter().chain(extra);
    let matches = command().try_get_matches_from(argv)?;
    parse(&matches)
}

/// Catches a server flag that never reaches the server half, a daemon flag that never
/// reaches the daemon half, and the two halves' shared thread flags held twice (clap's
/// own check of the command panics on a name held twice) or handed to the server half,
/// which sizes itself as `kbf-server` does.
#[test]
fn each_flag_reaches_its_half_once() {
    command().debug_assert();
    let flags = flags(&[
        "--driver=container",
        "--worker-listen=127.0.0.1:9981",
        "--listen=127.0.0.1:9980",
        "--cold-store=file:///var/kbf/cold",
        "--hot-store=file:///var/kbf/hot",
        "--cpu-threads=3",
        "--data-dir=/var/kbf",
    ])
    .expect("flags");
    assert_eq!(flags.server.worker_listen.to_string(), "127.0.0.1:9981");
    assert_eq!(flags.server.listen.to_string(), "127.0.0.1:9980");
    assert_eq!(flags.server.threads, kbf_threads::PoolArgs::default());
    let daemon = format!("{:?}", flags.daemon);
    assert!(daemon.contains("cpu_threads: Some(3)"), "{daemon}");
    assert!(daemon.contains("\"/var/kbf\""), "{daemon}");
    let server = format!("{:?}", flags.server);
    assert!(server.contains("\"/var/kbf/hot\""), "{server}");
}

/// Catches a driver other than the container driver taken (the native drivers run
/// actions as the farm's own user, beside the server's state), whether alone or beside
/// the container driver, and `--cas` taken (a remote data plane in the same process as
/// its server). Each refusal says what to do instead.
#[test]
fn other_drivers_and_cas_are_refused() {
    for (extra, says) in [
        (&["--driver=native"][..], "--driver native"),
        (
            &["--driver=container", "--driver=fake"][..],
            "--driver fake",
        ),
        (
            &["--driver=container", "--cas=https://127.0.0.1:8981"][..],
            "--cas",
        ),
    ] {
        let refused = flags(extra).expect_err("refused");
        assert_eq!(refused.kind(), ErrorKind::ArgumentConflict, "{extra:?}");
        assert!(refused.to_string().contains(says), "{refused}");
    }
}

/// Catches an all-in-one that keeps no state file when `--state-store` is left out (the
/// scheduler would run in memory and a restart lose its queue), one that puts it
/// anywhere but the daemon's data directory (default `/var/lib/kbf`), and one that
/// replaces a file the operator named.
#[test]
fn the_state_file_defaults_to_the_data_directory() {
    let store = |extra: &[&str]| {
        let given = [&["--driver=container"], extra].concat();
        flags(&given).expect("flags").server.state_store
    };
    let sqlite = |path: &str| Some(kbf_server::StateStoreUri::Sqlite(path.into()));
    assert_eq!(store(&[]), sqlite("/var/lib/kbf/state.db"));
    assert_eq!(store(&["--data-dir=/srv/kbf"]), sqlite("/srv/kbf/state.db"));
    assert_eq!(
        store(&[
            "--data-dir=/srv/kbf",
            "--state-store=sqlite:///elsewhere/s.db"
        ]),
        sqlite("/elsewhere/s.db")
    );
}

/// Catches an all-in-one that takes Postgres (not wired yet: any value but a SQLite path
/// is refused with a fixed message), and a relative data directory, which the daemon half
/// refuses after the server half has already made its state file in the working
/// directory.
#[test]
fn postgres_and_a_relative_data_directory_are_refused() {
    let refused = flags(&[
        "--driver=container",
        "--state-store=postgres://db.example/kbf",
    ])
    .expect_err("refused");
    let said = refused.to_string();
    assert!(said.contains("only sqlite:///<absolute path>"), "{said}");
    let relative = flags(&["--driver=container", "--data-dir=kbf-data"]).expect_err("relative");
    assert_eq!(relative.kind(), ErrorKind::ArgumentConflict);
    assert!(
        relative.to_string().contains("must be absolute"),
        "{relative}"
    );
    // A named state file needs no data directory to hold it.
    flags(&[
        "--driver=container",
        "--data-dir=kbf-data",
        "--state-store=sqlite:///s.db",
    ])
    .expect("named file");
}

/// Catches a refused `--state-store` that quotes its value back: clap echoes the value of
/// a parser's refusal (`invalid value '...'`), the command line is world-readable, and
/// the error goes to stderr and the journal. Each value holds a password in a form a
/// mistyped URL takes: user information, no `://`, a scheme position, and a parameter.
#[test]
fn a_refused_state_store_never_echoes_its_value() {
    for value in [
        "postgres://kbf:hunter2@db/x",
        "postgres:kbf:hunter2@db/x",
        "postgres:kbf:hunter2@db://x",
        "postgres://db/x?user=kbf&password=hunter2",
        "sqlite://hunter2",
    ] {
        let arg = format!("--state-store={value}");
        let refused = flags(&["--driver=container", &arg]).expect_err(value);
        let said = refused.to_string();
        for secret in ["hunter2", "kbf:", "kbf@", "password="] {
            assert!(!said.contains(secret), "{value}: {said}");
        }
        assert!(said.contains("--state-store: "), "{value}: {said}");
    }
}
