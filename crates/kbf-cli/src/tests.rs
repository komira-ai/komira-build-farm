//! The modes, the flags each takes and the refusals.

use clap::error::ErrorKind;

use super::*;

/// What `kbf daemon` needs to parse: its required flags.
const DAEMON: &[&str] = &[
    "kbf",
    names::DAEMON,
    "--driver=container",
    "--server=https://127.0.0.1:8981",
    "--ca-cert=ca.pem",
    "--cert=node.pem",
    "--key=node.key",
    "--node-id=node-1",
];

/// What `kbf all-in-one` needs to parse: the same required flags.
const BOTH: &[&str] = &[
    "kbf",
    names::ALL_IN_ONE,
    "--driver=container",
    "--server=https://127.0.0.1:8981",
    "--ca-cert=ca.pem",
    "--cert=node.pem",
    "--key=node.key",
    "--node-id=node-1",
];

/// What `kbf storage` needs to parse: nothing (its stores are checked at start).
const STORAGE: &[&str] = &["kbf", names::STORAGE];

/// What `kbf storage-and-daemon` needs to parse: the daemon's required flags.
const CONVERGED: &[&str] = &[
    "kbf",
    names::STORAGE_AND_DAEMON,
    "--driver=container",
    "--server=https://127.0.0.1:8981",
    "--ca-cert=ca.pem",
    "--cert=node.pem",
    "--key=node.key",
    "--node-id=node-1",
];

/// What `kbf frontend` needs to parse: the storage node it reaches.
const FRONTEND: &[&str] = &["kbf", names::FRONTEND, "--storage=http://127.0.0.1:8982"];

fn with(base: &[&str], extra: &[&str]) -> Vec<String> {
    base.iter().chain(extra).map(|s| (*s).to_owned()).collect()
}

fn refusal(argv: Vec<String>) -> clap::Error {
    parse_from(argv).expect_err("refused")
}

fn error(argv: Vec<String>) -> String {
    refusal(argv).to_string()
}

/// Catches a mode wired to the wrong flags or entry, or a clap command that does not build (a name held twice panics in
/// `debug_assert`).
#[test]
fn each_mode_parses_its_flags() {
    command().debug_assert();
    let daemon = parse_from(with(DAEMON, &["--cpu-threads=3"])).expect("daemon");
    assert!(matches!(&daemon, Invocation::Daemon(cli)
        if format!("{cli:?}").contains("cpu_threads: Some(3)")));
    let both = parse_from(with(BOTH, &["--listen=127.0.0.1:9980"])).expect("all-in-one");
    assert!(matches!(&both, Invocation::AllInOne(flags)
        if flags.server.listen.to_string() == "127.0.0.1:9980"));
    let storage = parse_from(with(STORAGE, &["--storage-listen=127.0.0.1:9982"])).expect("storage");
    assert!(matches!(&storage, Invocation::Storage(flags)
        if flags.listen.to_string() == "127.0.0.1:9982" && flags.tls.is_none()));
    let frontend = parse_from(with(FRONTEND, &["--listen=127.0.0.1:9980"])).expect("frontend");
    assert!(matches!(&frontend, Invocation::Frontend(flags)
        if flags.server.listen.to_string() == "127.0.0.1:9980" && flags.tls.is_none()));
    let converged = with(
        CONVERGED,
        &["--storage-listen=127.0.0.1:9982", "--cpu-threads=3"],
    );
    let converged = parse_from(converged).expect("storage-and-daemon");
    assert!(matches!(&converged, Invocation::StorageAndDaemon(flags)
        if flags.storage.listen.to_string() == "127.0.0.1:9982"
            && format!("{:?}", flags.daemon).contains("cpu_threads: Some(3)")));
}

/// Catches a table that is not exactly the five modes with the mapping the design
/// gives (every mode runs its own entry), a duplicate mode, or a clap command that lists other modes than the
/// table.
#[test]
fn the_table_has_exactly_five_modes_and_their_entries() {
    let table: Vec<(&str, Entry)> = MODES.iter().map(|m| (m.name, m.entry)).collect();
    assert_eq!(
        table,
        [
            ("frontend", Entry::Frontend),
            ("storage", Entry::Storage),
            ("daemon", Entry::Daemon),
            ("storage-and-daemon", Entry::StorageAndDaemon),
            ("all-in-one", Entry::AllInOne),
        ]
    );
    let cmd = command();
    let listed: Vec<&str> = cmd.get_subcommands().map(Command::get_name).collect();
    let named: Vec<&str> = MODES.iter().map(|m| m.name).collect();
    assert_eq!(listed, named);
}

/// Catches a bare `kbf`, an unknown mode, a mode named `state`, the old binary names or
/// the old subcommand names taken as modes, or a `help` subcommand that would make a
/// sixth word.
#[test]
fn a_mode_is_required_and_an_unknown_one_is_rejected() {
    for argv in [
        vec!["kbf"],
        vec!["kbf", "serve"],
        vec!["kbf", "state"],
        vec!["kbf", "server"],
        vec!["kbf", "kbf-daemon"],
        vec!["kbf", "help"],
    ] {
        let err = parse_from(argv.clone()).expect_err("rejected");
        assert!(
            matches!(
                err.kind(),
                ErrorKind::InvalidSubcommand
                    | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
                    | ErrorKind::UnknownArgument
            ),
            "{argv:?}: {err}"
        );
        assert_eq!(err.exit_code(), 2, "{argv:?}");
    }
}

/// Catches a `--help` that lists a sixth word (`help`, an old name) or misses a mode:
/// the words under `Commands:` are exactly the table's.
#[test]
fn help_lists_exactly_the_five_modes() {
    let help = command().render_long_help().to_string();
    let listed: Vec<&str> = help
        .lines()
        .skip_while(|l| !l.starts_with("Commands:"))
        .skip(1)
        .take_while(|l| l.starts_with("  "))
        .filter_map(|l| l.split_whitespace().next())
        .collect();
    let named: Vec<&str> = MODES.iter().map(|m| m.name).collect();
    assert_eq!(listed, named, "{help}");
    assert_eq!(listed.len(), 5);
}

/// Catches a mode that accepts a flag of the other (`--driver` is the daemon's, so the
/// union takes it; a flag only `kbf-all-in-one` has is refused by the daemon), a refusal
/// that is not clap's conflict kind and exit 2, a refused set that is not derived (it
/// must equal the all-in-one flags the daemon lacks, and a flag added to
/// `kbf-all-in-one` is covered), and a mode that takes the union refusing something.
#[test]
fn each_mode_refuses_the_flags_of_another() {
    let daemon = longs(&kbf_node::Cli::command());
    let both = longs(&kbf_all_in_one::cli::command());
    let storage = longs(&kbf_server::storage_node::command());
    let frontend = longs(&kbf_server::frontend::command());
    let others: BTreeSet<String> = [&both, &storage, &frontend]
        .into_iter()
        .flatten()
        .cloned()
        .collect();
    let derived: Vec<&str> = others.difference(&daemon).map(String::as_str).collect();
    assert_eq!(refused_by(names::DAEMON), derived.as_slice());
    assert!(derived.contains(&"cold-store") && derived.contains(&"listen"));
    assert!(
        derived.contains(&"storage-listen"),
        "the storage node's own flags are refused"
    );
    assert!(derived.contains(&"storage-ca"), "the frontend's own too");
    assert!(daemon.is_subset(&both), "the union takes every daemon flag");
    // The storage node's own flags are not all-in-one's, so it refuses them; it takes
    // none of the daemon's flags that it lacks.
    let refused = |mode: &str| refused_by(mode).iter().copied().collect::<BTreeSet<_>>();
    let minus = |sets: &[&BTreeSet<String>], mine: &BTreeSet<String>| -> BTreeSet<&'static str> {
        sets.iter()
            .flat_map(|set| set.difference(mine))
            .map(|flag| &*Box::leak(flag.clone().into_boxed_str()))
            .collect()
    };
    assert_eq!(
        refused(names::ALL_IN_ONE),
        minus(&[&storage, &frontend], &both)
    );
    assert_eq!(
        refused(names::STORAGE),
        minus(&[&daemon, &frontend], &storage)
    );
    assert_eq!(
        refused(names::FRONTEND),
        minus(&[&daemon, &storage], &frontend)
    );
    for flag in ["storage-listen", "storage-tls-cert", "driver", "cas"] {
        assert!(refused(names::FRONTEND).contains(flag), "{flag}");
    }
    for flag in ["storage-client-cert", "storage-client-key", "storage-ca"] {
        assert!(derived.contains(&flag), "{flag}");
        assert!(refused(names::STORAGE).contains(flag), "{flag}");
        assert!(refused(names::ALL_IN_ONE).contains(flag), "{flag}");
        assert!(!refused(names::FRONTEND).contains(flag), "{flag}");
    }
    assert!(refused_by("no-such-mode").is_empty());
    for flag in derived {
        for given in [format!("--{flag}"), format!("--{flag}=x")] {
            let err = refusal(with(DAEMON, &[given.as_str()]));
            assert_eq!(err.kind(), ErrorKind::ArgumentConflict, "{given}");
            assert_eq!(err.exit_code(), 2, "{given}");
            assert!(
                err.to_string()
                    .contains(&format!("kbf {} refuses --{flag}: ", names::DAEMON)),
                "{given}: {err}"
            );
        }
    }
}

/// Catches: `--state-store` not reaching the all-in-one's server flags, a scheme the
/// design does not admit accepted at parse (a start would find out much later), and a
/// build node that takes it (it holds no scheduler state, so the flag would be silently
/// ignored there).
#[test]
fn only_the_scheduling_modes_take_a_state_store() {
    let sqlite = with(BOTH, &["--state-store=sqlite:///var/lib/kbf/state.db"]);
    let both = parse_from(sqlite).expect("all-in-one");
    let path = std::path::PathBuf::from("/var/lib/kbf/state.db");
    assert!(matches!(&both, Invocation::AllInOne(flags)
        if flags.server.state_store == Some(kbf_server::StateStoreUri::Sqlite(path.clone()))));
    // Only SQLite is wired; the refusal is fixed text, not the value.
    let postgres = error(with(BOTH, &["--state-store=postgres://db.example/kbf"]));
    assert!(postgres.contains("only sqlite:///"), "{postgres}");
    let default = parse_from(with(BOTH, &[])).expect("all-in-one");
    assert!(matches!(&default, Invocation::AllInOne(flags)
        if matches!(flags.server.state_store, Some(kbf_server::StateStoreUri::Sqlite(_)))));
    let front = parse_from(with(
        FRONTEND,
        &["--state-store=sqlite:///var/lib/kbf/state.db"],
    ))
    .expect("frontend");
    assert!(matches!(&front, Invocation::Frontend(flags)
        if flags.server.state_store == Some(kbf_server::StateStoreUri::Sqlite(path.clone()))));
    let file = error(with(BOTH, &["--state-store=file:///var/lib/kbf"]));
    assert!(file.contains("only sqlite:///"), "{file}");
    let daemon = refusal(with(DAEMON, &["--state-store=sqlite:///x.db"]));
    assert_eq!(daemon.kind(), ErrorKind::ArgumentConflict);
    assert!(
        daemon.to_string().contains("refuses --state-store"),
        "{daemon}"
    );
}

/// Catches an all-in-one that takes less or more than `kbf-all-in-one` takes: its
/// flags are exactly those of that command.
#[test]
fn all_in_one_takes_exactly_what_kbf_all_in_one_takes() {
    let mode = MODES
        .iter()
        .find(|m| m.name == names::ALL_IN_ONE)
        .expect("mode");
    let mut new = longs(&mode_command(mode.name, mode.entry));
    // Besides its own, the command holds the flags of other modes it refuses.
    for flag in refused_by(mode.name) {
        assert!(new.remove(*flag), "{flag}");
    }
    assert_eq!(longs(&kbf_all_in_one::cli::command()), new);
}

/// Catches a vote flag creeping back in, or a reserved flag listed by hand: no mode
/// takes `--electable` (clap's unknown argument), and none of the cluster flags of the
/// earlier design is in any refused set.
#[test]
fn there_is_no_vote_flag() {
    for argv in [DAEMON, BOTH] {
        let err = refusal(with(argv, &["--electable"]));
        assert_eq!(err.kind(), ErrorKind::UnknownArgument);
    }
    for gone in [
        "electable",
        "advertise",
        "raft-dir",
        "bootstrap",
        "voter-count",
    ] {
        assert!(!refused_by(names::DAEMON).contains(&gone), "{gone}");
    }
}

/// Catches a flag the refused set cannot see: a short flag or an alias is not a
/// `get_long()`, so a flag carried only as an alias would be accepted by the wrong
/// mode. Neither command line has one today; adding one fails here until the
/// derivation reads it.
#[test]
fn no_mode_has_a_short_flag_or_an_alias() {
    for cmd in [kbf_node::Cli::command(), kbf_all_in_one::cli::command()] {
        for arg in cmd.get_arguments() {
            assert_eq!(arg.get_short(), None, "{}", arg.get_id());
            assert!(arg.get_all_aliases().is_none(), "{}", arg.get_id());
            assert!(arg.get_all_short_aliases().is_none(), "{}", arg.get_id());
        }
    }
}

/// Catches a build node that takes a store or a credential (docs/design/cluster.md,
/// sections 3.3, S2 to S4) through a flag only the daemon gained, which the refusal
/// cannot see, or through the environment: clap's `env` feature, which a flag needs to
/// read a variable, is off in the workspace. The daemon's only credentials are its node
/// identity (`--ca-cert`, `--cert`, `--key`). The words are the wire test's
/// (kbf-proto's tests/daemon_wire.rs): a part (`store`) is matched anywhere in a flag, a
/// short word (`sts`, `url`, `key`) only as a whole word between dashes, except in the
/// reviewed flags of `REVIEWED_DAEMON_FLAGS`, each of which must still be a flag.
#[test]
fn the_daemon_takes_no_store_credential_or_environment_variable() {
    /// Flags that hold a short word and were reviewed.
    const REVIEWED_DAEMON_FLAGS: &[&str] = &[
        // The node identity's private key.
        "key",
        // The socket `kbf-mac-session` serves: a macOS login session, not a cloud one.
        "mac-session-socket",
    ];
    let flags = longs(&kbf_node::Cli::command());
    for flag in &flags {
        for part in [
            "store",
            "cold",
            "bucket",
            "endpoint",
            "credential",
            "secret",
            "access-key",
            "password",
            "region",
            "presign",
            "auth",
            "tls-cert",
            "tls-key",
        ] {
            assert!(!flag.contains(part), "--{flag} names {part}");
        }
        if REVIEWED_DAEMON_FLAGS.contains(&flag.as_str()) {
            continue;
        }
        for word in [
            "s3", "aws", "sts", "session", "token", "url", "uri", "key", "object",
        ] {
            let plural = format!("{word}s");
            assert!(
                !flag.split('-').any(|w| w == word || w == plural),
                "--{flag} names {word}"
            );
        }
    }
    for identity in ["ca-cert", "cert", "key"] {
        assert!(flags.contains(identity), "{identity}");
    }
    for reviewed in REVIEWED_DAEMON_FLAGS {
        assert!(
            flags.contains(*reviewed),
            "--{reviewed} is no longer a flag"
        );
    }
    let workspace = include_str!("../../../Cargo.toml");
    let clap = workspace
        .lines()
        .find(|l| l.starts_with("clap = "))
        .expect("the workspace names clap");
    assert!(!clap.contains("\"env\""), "{clap}");
}

/// Catches a build node that can be pointed at the cold store (docs/design/architecture-v2.md,
/// section 9: a daemon-only node holds no cold-store credential and reaches bytes only
/// through storage): every cold-store flag of the modes that have one, the hidden sizing
/// flags included, is refused by `kbf daemon` with the store reason, by `kbf check-args
/// daemon` (what `kbf-deploy linux check-units --mode` runs on a unit), and as an unknown
/// argument by the `kbf-daemon` binary's own parser. Also catches a set of cold-store
/// flags that is empty or misses `--cold-store`, which would pass vacuously.
#[test]
fn a_build_node_refuses_every_cold_store_flag() {
    let cold: BTreeSet<String> = [
        kbf_all_in_one::cli::command(),
        kbf_server::storage_node::command(),
        <kbf_server::Args as clap::CommandFactory>::command(),
    ]
    .iter()
    .flat_map(longs)
    .filter(|flag| flag.starts_with("cold"))
    .collect();
    assert!(cold.contains("cold-store"), "{cold:?}");
    assert!(cold.len() > 1, "the sizing flags too: {cold:?}");
    for flag in &cold {
        let given = format!("--{flag}=s3://kbf-cas?endpoint=https://objects.example");
        let err = refusal(with(DAEMON, &[given.as_str()]));
        assert_eq!(err.kind(), ErrorKind::ArgumentConflict, "{flag}");
        assert_eq!(err.exit_code(), 2, "{flag}");
        assert!(
            err.to_string().contains(&format!(
                "kbf daemon refuses --{flag}: a build node holds no store and no credential"
            )),
            "{flag}: {err}"
        );
        let mut checked = vec!["kbf", CHECK_ARGS];
        checked.extend(&DAEMON[1..]);
        checked.push(given.as_str());
        let e = check(&checked).expect_err("refused");
        assert!(e.contains(&format!("refuses --{flag}")), "{flag}: {e}");
        let mut alone: Vec<&str> = vec!["kbf-daemon"];
        alone.extend(&DAEMON[2..]);
        alone.push(given.as_str());
        let err = <kbf_node::Cli as clap::Parser>::try_parse_from(alone).expect_err("refused");
        assert_eq!(err.kind(), ErrorKind::UnknownArgument, "{flag}: {err}");
    }
}

/// Catches a refused set that takes in a flag the daemon needs (the thread-pool flags
/// both take), which would break `kbf daemon` for its own flags.
#[test]
fn the_refused_set_leaves_the_daemons_own_flags() {
    let daemon = longs(&kbf_node::Cli::command());
    let both = longs(&kbf_all_in_one::cli::command());
    assert!(daemon.intersection(&both).any(|f| f == "cpu-threads"));
    for flag in refused_by(names::DAEMON) {
        assert!(!daemon.contains(*flag), "{flag}");
    }
    for flag in &daemon {
        assert!(
            !refused_by(names::DAEMON).contains(&flag.as_str()),
            "{flag}"
        );
    }
}

/// Catches a credential-or-store flag refused with the generic reason (the operator is
/// told why, not only that), a generic reason that names no mode, and the credential
/// reason given to a mode other than the daemon.
#[test]
fn a_refusal_says_why() {
    let store = "no store and no credential";
    for flag in [
        "cold-store",
        "worker-tls-key",
        "api-token-file",
        "reapi-auth-policy",
    ] {
        assert!(reason(names::DAEMON, flag).contains(store), "{flag}");
    }
    assert!(reason(names::DAEMON, "listen").contains("`kbf daemon` does not take"));
    assert!(reason(names::ALL_IN_ONE, "cold-store").contains("`kbf all-in-one` does not take"));
}

/// Catches a mode that parses its flags differently from the binary it replaces: the
/// same words give the same result through `kbf <mode>` as through the old binary's own
/// parser.
#[test]
fn the_modes_parse_as_the_old_binaries_do() {
    let alias =
        <kbf_node::Cli as clap::Parser>::try_parse_from(with(&["kbf-daemon"], &DAEMON[2..]))
            .expect("alias");
    let inv = parse_from(with(DAEMON, &[])).expect("daemon");
    assert!(matches!(&inv, Invocation::Daemon(cli) if format!("{alias:?}") == format!("{cli:?}")));

    let matches = kbf_all_in_one::cli::command()
        .try_get_matches_from(with(&["kbf-all-in-one"], &BOTH[2..]))
        .expect("alias");
    let alias = kbf_all_in_one::cli::parse(&matches).expect("alias flags");
    let inv = parse_from(with(BOTH, &[])).expect("both");
    assert!(
        matches!(&inv, Invocation::AllInOne(flags) if format!("{alias:?}") == format!("{flags:?}"))
    );
}

/// Catches a `--help` or `--version` that is reported as a failure.
#[test]
fn help_and_version_are_not_errors() {
    for argv in [
        vec!["kbf", "--version"],
        vec!["kbf", "all-in-one", "--help"],
    ] {
        let e = parse_from(argv).expect_err("an exit");
        assert!(matches!(
            e.kind(),
            ErrorKind::DisplayVersion | ErrorKind::DisplayHelp
        ));
        assert_eq!(e.exit_code(), 0);
    }
}

/// Catches a `kbf --version` without the commit (the package version alone), which the
/// artifacts workflow's `cargo xtask build-commit check` refuses: it must print
/// `kbf <package version>+<commit>`, the stamp `kbf-server` carries.
#[test]
fn the_version_names_the_commit_built() {
    let e = refusal(vec!["kbf".into(), "--version".into()]);
    assert_eq!(e.kind(), ErrorKind::DisplayVersion);
    let line = e.to_string();
    assert_eq!(
        line.trim_end(),
        format!("kbf {}", kbf_server::SERVER_VERSION)
    );
    assert!(line.contains('+'), "{line}");
}

/// Catches a refusal hidden behind clap's complaint about missing required flags: a
/// build node given only a flag of another mode hears which flag is not its own, and a
/// build node missing a required flag with no refused flag still hears about that.
#[test]
fn a_refusal_comes_before_a_missing_required_flag() {
    for flag in ["--cold-store=s3://b", "--hot-store=file:///x"] {
        let msg = error(vec!["kbf".into(), names::DAEMON.into(), flag.into()]);
        let name = flag.split('=').next().unwrap_or(flag);
        assert!(
            msg.contains(&format!("kbf {} refuses {name}: ", names::DAEMON)),
            "{msg}"
        );
    }
    let msg = error(vec![
        "kbf".into(),
        names::DAEMON.into(),
        "--node-id=n".into(),
    ]);
    assert!(
        msg.contains("required arguments were not provided"),
        "{msg}"
    );
    // A mode that refuses nothing holds no refusal to find.
    let msg = error(vec![
        "kbf".into(),
        names::ALL_IN_ONE.into(),
        "--node-id=n".into(),
    ]);
    assert!(
        msg.contains("required arguments were not provided"),
        "{msg}"
    );
}

fn check(argv: &[&str]) -> Result<(), String> {
    let args: Vec<std::ffi::OsString> = argv.iter().map(Into::into).collect();
    check_args(&args).map_err(|e| e.to_string())
}

/// Catches `check_args` reading the wrong words (the program's name or `check-args`
/// itself as the mode), passing `--help`, and refusing a built
/// mode's own command line. The binary's exit codes are tests/binary.rs's.
#[test]
fn check_args_parses_the_words_after_check_args() {
    let mut daemon = vec!["kbf", CHECK_ARGS];
    daemon.extend(&DAEMON[1..]);
    assert_eq!(check(&daemon), Ok(()));
    let mut both = vec!["kbf", CHECK_ARGS];
    both.extend(&BOTH[1..]);
    assert_eq!(check(&both), Ok(()));
    assert_eq!(
        check(&[
            "kbf",
            CHECK_ARGS,
            names::FRONTEND,
            "--storage=http://127.0.0.1:1"
        ]),
        Ok(())
    );
    let e = check(&["kbf", CHECK_ARGS, names::FRONTEND]).expect_err("refused");
    assert!(e.contains("--storage"), "{e}");
    daemon.push("--bogus-flag");
    let e = check(&daemon).expect_err("refused");
    assert!(e.contains("unexpected argument '--bogus-flag'"), "{e}");
    assert_eq!(
        check(&[
            "kbf",
            CHECK_ARGS,
            names::STORAGE,
            "--storage-listen=127.0.0.1:1"
        ]),
        Ok(())
    );
    let mut converged = vec!["kbf", CHECK_ARGS];
    converged.extend(&CONVERGED[1..]);
    assert_eq!(check(&converged), Ok(()));
    let e = check(&["kbf", CHECK_ARGS, names::STORAGE_AND_DAEMON]).expect_err("refused");
    assert!(e.contains("required arguments were not provided"), "{e}");
    for word in ["--help", "--version"] {
        let e = check(&["kbf", CHECK_ARGS, word]).expect_err("refused");
        assert!(e.contains("the mode would not start"), "{word}: {e}");
    }
}

/// Catches a storage node that takes a state store (arch-v2 section 6: storage nodes do
/// not talk to the state store; they reach it only through a frontend), however it is
/// spelled, or any flag of `kbf-server` outside the store, write and log flags, or a
/// flag of the daemon: each is refused naming the flag, exit 2, clap's conflict kind or
/// its unknown-argument kind.
#[test]
fn a_storage_node_has_no_state_store_flag() {
    for flag in [
        "--state-store=sqlite:///x",
        "--state-store",
        "--state-store=postgres://h/db",
    ] {
        let err = refusal(with(STORAGE, &[flag]));
        assert_eq!(err.exit_code(), 2, "{flag}");
        // Whatever kind clap gives it (a flag of `kbf-server` the node does not take is a
        // conflict, a bare one has no value), the refusal names the flag.
        assert!(err.to_string().contains("--state-store"), "{flag}: {err}");
    }
    // The command lists every flag of `kbf-server` to refuse it by name; none about a
    // state store is one the node takes.
    for id in kbf_server::storage_node::ALLOWED {
        assert!(!id.contains("state"), "{id}");
    }
    // Every flag of kbf-server the node does not take is refused by name, so one added
    // later (a state store, say) is refused with no edit.
    let server = <kbf_server::Args as clap::CommandFactory>::command();
    let mut refused = 0;
    for arg in server.get_arguments() {
        let id = arg.get_id().as_str();
        let taken = kbf_server::storage_node::ALLOWED.contains(&id)
            || ["async_threads", "blocking_threads", "cpu_threads"].contains(&id);
        if taken || id == "help" || id == "version" {
            continue;
        }
        let long = arg.get_long().expect("a long flag");
        let err = refusal(with(STORAGE, &[format!("--{long}=x").as_str()]));
        assert_eq!(err.exit_code(), 2, "{long}");
        refused += 1;
    }
    assert!(refused > 20, "{refused}");
    // Where clap takes the value, the refusal names the flag and says why.
    for flag in [
        "--reapi-auth-policy=/p",
        "--invocations-open",
        "--finished-retention-secs=5",
        "--role=all",
        "--api-listen=127.0.0.1:1",
        "--listen=127.0.0.1:1",
    ] {
        let err = refusal(with(STORAGE, &[flag]));
        let name = flag.split('=').next().unwrap_or(flag);
        assert_eq!(err.kind(), ErrorKind::ArgumentConflict, "{flag}");
        assert!(
            err.to_string()
                .contains(&format!("kbf storage refuses {name}: ")),
            "{err}"
        );
    }
    for flag in ["--cas=x", "--driver=container", "--node-id=n"] {
        assert_eq!(
            refusal(with(STORAGE, &[flag])).kind(),
            ErrorKind::ArgumentConflict,
            "{flag}"
        );
    }
}

/// Catches a frontend that opens a store or takes a storage node's listener: its own
/// flags and the scheduling ones parse, every store, write and segment flag is refused
/// naming `kbf frontend`, and a storage node's listener flags are refused as another
/// mode's (exit 2, clap's conflict kind).
#[test]
fn a_frontend_has_no_store_and_none_of_a_storage_nodes_listener() {
    for flag in [
        "--cold-store=file:///x",
        "--hot-store=file:///x",
        "--meta-dir=/x",
        "--write-linger=5",
        "--segment-upload-idle-secs=5",
    ] {
        let err = refusal(with(FRONTEND, &[flag]));
        let name = flag.split('=').next().unwrap_or(flag);
        assert_eq!(err.kind(), ErrorKind::ArgumentConflict, "{flag}");
        assert_eq!(err.exit_code(), 2, "{flag}");
        assert!(
            err.to_string()
                .contains(&format!("kbf frontend refuses {name}: ")),
            "{err}"
        );
    }
    for flag in [
        "--storage-listen=127.0.0.1:1",
        "--storage-tls-cert=c",
        "--driver=container",
    ] {
        let err = refusal(with(FRONTEND, &[flag]));
        let name = flag.split('=').next().unwrap_or(flag);
        assert_eq!(err.kind(), ErrorKind::ArgumentConflict, "{flag}");
        assert!(
            err.to_string()
                .contains(&format!("kbf frontend refuses {name}: ")),
            "{err}"
        );
    }
    let tls = with(
        &[
            "kbf",
            names::FRONTEND,
            "--storage=https://storage.example:8982",
        ],
        &[
            "--storage-client-cert=/c",
            "--storage-client-key=/k",
            "--storage-ca=/a",
        ],
    );
    let parsed = parse_from(tls).expect("frontend with TLS");
    assert!(matches!(&parsed, Invocation::Frontend(flags)
        if flags.tls.as_ref().is_some_and(|t| t.iter().map(|p| p.display().to_string()).eq(["/c", "/k", "/a"]))));
}

fn os(words: &[&str]) -> Vec<std::ffi::OsString> {
    words.iter().map(Into::into).collect()
}

/// Catches `--version --json` read from a longer or shorter command line, from `--json`
/// alone, or from a mode's own words: only the two words and nothing else ask for the
/// identity, so a typo is a usage error and never an answer.
#[test]
fn only_version_and_json_ask_for_the_identity() {
    for yes in [
        ["kbf", "--version", "--json"],
        ["kbf", "--json", "--version"],
    ] {
        assert!(version::asked(&os(&yes)), "{yes:?}");
    }
    for no in [
        &["kbf"][..],
        &["kbf", "--version"],
        &["kbf", "--json"],
        &["kbf", "--version", "--json", "extra"],
        &["kbf", "--version", "--version"],
        &["kbf", "--json", "--json"],
        &["kbf", "daemon", "--json"],
        &["kbf", "--version", "-json"],
    ] {
        assert!(!version::asked(&os(no)), "{no:?}");
    }
}

/// An empty protocol range is a build error.
const _: () =
    assert!(kbf_server::worker::OLDEST_PROTOCOL_VERSION <= kbf_server::worker::PROTOCOL_VERSION);

/// Catches an identity that reports a value other than the library's own (a commit, a
/// protocol or a schema written down twice and drifting), two halves of the binary that
/// speak different protocols (the one number reported would be wrong for one), a
/// protocol range that is empty (the constant above), and a line that is not one object ending in one `\n`.
#[test]
fn the_identity_is_the_libraries_own_and_both_halves_speak_one_protocol() {
    let v = version::identity();
    assert_eq!(v["commit"], kbf_server::BUILD_COMMIT);
    assert_eq!(v["protocol"], kbf_server::worker::PROTOCOL_VERSION);
    assert_eq!(
        v["protocol"],
        kbf_daemon::PROTOCOL_VERSION,
        "the daemon half of kbf speaks another protocol than the server half"
    );
    assert_eq!(
        v["min_protocol"],
        kbf_server::worker::OLDEST_PROTOCOL_VERSION
    );
    assert_eq!(v["state_schema"], kbf_state::sqlite::SCHEMA_VERSION);
    assert_eq!(v["version"], env!("CARGO_PKG_VERSION"));
    assert!(
        ["debug", "release"].contains(&version::PROFILE),
        "{}",
        version::PROFILE
    );
    let line = version::line();
    assert!(
        line.ends_with("}\n") && line.matches('\n').count() == 1,
        "{line:?}"
    );
    assert_eq!(serde_json::from_str::<serde_json::Value>(&line).unwrap(), v);
}
