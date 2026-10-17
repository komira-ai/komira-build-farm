//! The units `kbf-deploy linux render-units --mode` writes, against this binary's own
//! parser, in-process: each mode's rendered `ExecStart=` is a command line `kbf` takes,
//! for the mode the unit is named after, and kbf-deploy renders exactly kbf's modes;
//! and `check-units --mode` runs this build's `kbf check-args` on a mode's flags.

use kbf_cli::{Invocation, parse_from};
use kbf_deploy::linux::modes::{TRUSTED_NOTE, render};
use kbf_deploy::linux::{LinuxConfig, Mode, check_mode_unit};
use kbf_deploy::unit::UnitFile;
use kbf_host::exec::{Entry as HostEntry, Exec, HostExec, Meta};
use kbf_host::path::{HostPath, Root};
use kbf_host::step::{Cmd, HttpUrl, Step};
use kbf_host::trust::Untrusted;

const EXAMPLE: &str = include_str!("../../kbf-deploy/templates/linux/config.example");

/// The example settings with one pinned image, so the image flags render too.
fn config() -> LinuxConfig {
    let image = format!("docker.io/library/busybox@sha256:{}", "a".repeat(64));
    let text = EXAMPLE.replace("KBF_IMAGES=\n", &format!("KBF_IMAGES={image}\n"));
    assert_ne!(
        text, EXAMPLE,
        "config.example has no empty KBF_IMAGES= line"
    );
    LinuxConfig::parse(&text).expect("valid settings")
}

/// Catches a mode unit naming a flag `kbf <mode>` does not take or refuses as another
/// mode's (a store flag in the daemon's unit, `--cas` in the all-in-one's), a value
/// its parser refuses, and a unit that starts another mode than its name says.
#[test]
fn each_rendered_mode_unit_parses_with_kbfs_own_parser() {
    let c = config();
    for mode in Mode::ALL {
        let text = render(&c, Some(1500), mode).expect("renders");
        let unit = UnitFile::parse(&text).expect("a unit");
        let argv = unit.exec_start();
        assert_eq!(argv[..2], ["/usr/local/bin/kbf", mode.name()], "{mode}");
        let parsed = match parse_from(argv.iter().copied()) {
            Ok(parsed) => parsed,
            Err(e) => panic!("kbf {mode} refuses its rendered ExecStart= {argv:?}:\n{e}"),
        };
        match parsed {
            Invocation::Daemon(_) => assert_eq!(mode, Mode::Daemon),
            Invocation::AllInOne(flags) => {
                assert_eq!(mode, Mode::AllInOne);
                assert_eq!(
                    flags.server.api_token_file.as_deref(),
                    Some(std::path::Path::new("/etc/kbf/api-token"))
                );
            }
            Invocation::Frontend(flags) => {
                assert_eq!(mode, Mode::Frontend);
                // The unit's storage URL and the three flags toward it are read as the
                // settings spell them: mutual TLS with the frontend's worker certificate.
                assert_eq!(flags.storage.uri().to_string(), "https://192.0.2.20:8983/");
                let paths = flags
                    .tls
                    .expect("the frontend unit turns on mutual TLS to storage");
                assert_eq!(
                    paths.map(|p| p.display().to_string()),
                    [
                        "/etc/kbf/worker.pem",
                        "/etc/kbf/worker.key",
                        "/etc/kbf/ca.pem"
                    ]
                );
                assert_eq!(flags.server.listen.to_string(), "192.0.2.10:8980");
                assert_eq!(flags.server.worker_listen.to_string(), "192.0.2.10:8981");
                assert_eq!(
                    flags.server.api_listen.map(|a| a.to_string()).as_deref(),
                    Some("127.0.0.1:8982")
                );
                // The operator API's token is the file the unit names, so a write is not 401.
                assert_eq!(
                    flags.server.api_token_file.as_deref(),
                    Some(std::path::Path::new("/etc/kbf/api-token"))
                );
                assert_eq!(
                    flags.server.state_store,
                    Some(kbf_server::StateStoreUri::Sqlite(
                        "/var/lib/kbf-server/state/state.db".into()
                    ))
                );
            }
            Invocation::Storage(flags) => {
                assert_eq!(mode, Mode::Storage);
                // The unit's certificate flags are read as the paths they spell (not
                // dropped, and not another flag's), so the node starts with mutual TLS.
                let paths = flags.tls.expect("the storage unit turns on mutual TLS");
                assert_eq!(
                    paths.map(|p| p.display().to_string()),
                    [
                        "/etc/kbf/storage.pem",
                        "/etc/kbf/storage.key",
                        "/etc/kbf/ca.pem"
                    ]
                );
                // Its stores and listener are the settings', read as kbf reads them:
                // the bucket, the hot store and the metadata log, on the pinned address.
                assert_eq!(flags.listen.to_string(), "192.0.2.10:8983");
                assert_eq!(
                    flags.server.meta_dir.as_deref(),
                    Some(std::path::Path::new("/var/lib/kbf-server/raft"))
                );
                assert!(flags.server.cold_store.is_some(), "no cold store");
                assert!(flags.server.hot_store.is_some(), "no hot store");
            }
            Invocation::StorageAndDaemon(flags) => {
                assert_eq!(mode, Mode::StorageAndDaemon);
                // The storage half's certificate flags are read as the storage unit's
                // are, and the daemon half's are its own.
                let paths = flags
                    .storage
                    .tls
                    .expect("the storage half turns on mutual TLS");
                assert_eq!(
                    paths.map(|p| p.display().to_string()),
                    [
                        "/etc/kbf/storage.pem",
                        "/etc/kbf/storage.key",
                        "/etc/kbf/ca.pem"
                    ]
                );
                assert!(format!("{:?}", flags.daemon).contains("/etc/kbf/node.pem"));
            }
        }
    }
}

/// The all-in-one unit gives no `--state-store`, and kbf all-in-one, parsing the rendered
/// command line, takes the SQLite file `state.db` in the unit's `--data-dir`: the state
/// the template's comment promises, and one rule `state-store` passes. Catches the unit
/// growing a `--state-store` (a Postgres URI, say) or kbf's default moving out of the
/// data directory, or off SQLite.
#[test]
fn the_all_in_one_unit_leaves_its_state_store_to_kbfs_sqlite_default() {
    let text = render(&config(), Some(1500), Mode::AllInOne).expect("renders");
    let unit = UnitFile::parse(&text).expect("a unit");
    let argv = unit.exec_start();
    assert!(
        !argv.iter().any(|a| a.starts_with("--state-store")),
        "{argv:?}"
    );
    let data_dir = argv
        .iter()
        .find_map(|a| a.strip_prefix("--data-dir="))
        .expect("the all-in-one unit gives --data-dir");
    match parse_from(argv.iter().copied()) {
        Ok(Invocation::AllInOne(flags)) => assert_eq!(
            flags.server.state_store,
            Some(kbf_server::StateStoreUri::Sqlite(
                std::path::Path::new(data_dir).join("state.db")
            ))
        ),
        Ok(_) => panic!("not the all-in-one"),
        Err(e) => panic!("kbf all-in-one refuses its rendered unit: {e}"),
    }
}

/// A certificate made for each row of the farm's certificate table, as the farm's
/// issuer makes it (an EC P-256 key; `CA:FALSE`, `digitalSignature`, the row's extended
/// key usage, the authority key id; its names and one role URI in the subjectAltName),
/// passes `kbf-deploy linux verify-cert` for its row, and names its role the way a
/// storage node reads it (kbf-storage's `role_of`, the admission of every gRPC call).
/// Catches the two spellings drifting apart (the `urn:kbf:role:` prefix changed on one
/// side only, a role spelled otherwise or missing from the table), and a row whose
/// certificate verify-cert passes but storage reads as another role or none.
#[test]
fn each_rows_certificate_names_its_role_as_storage_reads_it() {
    use kbf_deploy::linux::verify_cert::{CLIENT_AUTH, ROLE_URI_PREFIX, Row, verify};
    use kbf_storage::service::{PeerRole, role_of};
    use rcgen::{ExtendedKeyUsagePurpose as Eku, KeyUsagePurpose, SanType};

    assert_eq!(ROLE_URI_PREFIX, kbf_storage::service::ROLE_URI_PREFIX);
    let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).expect("params");
    params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    let ca = rcgen::CertifiedIssuer::self_signed(params, rcgen::KeyPair::generate().expect("key"))
        .expect("CA");
    let mut roles = Vec::new();
    for row in Row::ALL {
        let name = if row == Row::Daemon {
            "node-1"
        } else {
            "node-1.farm.example"
        };
        let mut leaf = rcgen::CertificateParams::new(vec![name.to_owned()]).expect("params");
        leaf.subject_alt_names.push(SanType::URI(
            format!("{ROLE_URI_PREFIX}{}", row.role())
                .try_into()
                .expect("IA5"),
        ));
        leaf.is_ca = rcgen::IsCa::ExplicitNoCa;
        leaf.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        leaf.extended_key_usages = row
            .ekus()
            .iter()
            .map(|e| {
                if *e == CLIENT_AUTH {
                    Eku::ClientAuth
                } else {
                    Eku::ServerAuth
                }
            })
            .collect();
        leaf.use_authority_key_identifier_extension = true;
        let cert = leaf
            .signed_by(&rcgen::KeyPair::generate().expect("key"), &ca)
            .expect("signed");
        let node_id = (row == Row::Daemon).then_some(name);
        let failures = verify(
            ca.pem().as_bytes(),
            cert.pem().as_bytes(),
            row,
            node_id,
            std::time::SystemTime::now(),
        )
        .expect("checked");
        assert!(failures.is_empty(), "{row:?}: {failures:?}");
        let role = role_of(cert.der()).expect("a role storage admits");
        assert_eq!(role.name(), row.role(), "{row:?}");
        roles.push(role);
    }
    // Every role storage knows is some row's.
    for role in PeerRole::ALL {
        assert!(roles.contains(&role), "{role:?} is no row's");
    }
}

/// Catches kbf-deploy rendering a mode kbf does not have (a sixth, or an old name),
/// missing one kbf has, and listing them in another order than `kbf --help`.
#[test]
fn kbf_deploy_renders_exactly_the_modes_kbf_takes() {
    let kbf = kbf_cli::command();
    let help: Vec<&str> = kbf.get_subcommands().map(clap::Command::get_name).collect();
    let deploy: Vec<&str> = Mode::ALL.iter().map(|m| m.name()).collect();
    assert_eq!(deploy, help);
}

/// The scratch directory `name` under the workspace's `target/`, empty.
fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch");
    dir
}

/// The real host, but a command it runs keeps this test's `LLVM_PROFILE_FILE`:
/// [`HostExec`] clears the environment, and an instrumented `kbf` would then write its
/// profile into the working directory (the crate's), where the repo lints refuse it.
/// With `old` set, every command answers as a `kbf` from before `check-args` would:
/// this build's parser without the word intercepted, which that `kbf` ran on every
/// command line.
struct Profiled {
    host: HostExec,
    old: bool,
}

impl Exec for Profiled {
    fn euid(&self) -> u32 {
        self.host.euid()
    }
    fn inspect(&self, path: &HostPath) -> std::io::Result<Option<HostEntry>> {
        self.host.inspect(path)
    }
    fn stat(&self, path: &HostPath) -> std::io::Result<Option<Meta>> {
        self.host.stat(path)
    }
    fn query(&self, cmd: &Cmd) -> std::io::Result<std::process::Output> {
        if self.old {
            return Ok(old_kbf(cmd));
        }
        match std::env::var("LLVM_PROFILE_FILE") {
            Ok(file) => self.host.query(&cmd.clone().env("LLVM_PROFILE_FILE", file)),
            Err(_) => self.host.query(cmd),
        }
    }
    fn http_status(&self, url: &HttpUrl) -> std::io::Result<u16> {
        self.host.http_status(url)
    }
    fn apply(&mut self, step: &Step) -> std::io::Result<()> {
        self.host.apply(step)
    }
    fn check_binary(&self) -> Result<(), Untrusted> {
        self.host.check_binary()
    }
}

/// `check-units --mode` on the unit `text` of `mode`, with this build's kbf as
/// `--kbf-bin`: the failure lines and the notes.
fn check_units(name: &str, mode: Mode, text: &str) -> (Vec<String>, Vec<String>) {
    let exec = Profiled {
        host: HostExec::new(Root::host()),
        old: false,
    };
    let bin = std::path::Path::new(env!("CARGO_BIN_EXE_kbf"));
    check_units_with(&exec, bin, name, mode, text)
}

/// `check-units --mode` on the unit `text` of `mode`, beside the rendered `kbf.slice`,
/// on the host `exec` with `bin` as `--kbf-bin`: the failure lines and the notes.
fn check_units_with<E: Exec>(
    exec: &E,
    bin: &std::path::Path,
    name: &str,
    mode: Mode,
    text: &str,
) -> (Vec<String>, Vec<String>) {
    let dir = scratch(name);
    let cfg = dir.join("config.local");
    std::fs::write(&cfg, EXAMPLE).expect("config");
    kbf_deploy::linux::render_mode_unit(&cfg, &dir, Some(1500), mode).expect("renders");
    std::fs::write(dir.join(mode.unit_name()), text).expect("unit");
    let r = check_mode_unit(exec, &cfg, &dir, mode, true, Some(bin)).expect("checks");
    (
        r.failures.iter().map(ToString::to_string).collect(),
        r.notes,
    )
}

/// Catches `check-units --mode` passing a mode's unit whose flags kbf refuses (an
/// operator's edit that adds a flag `kbf daemon` does not have, or another mode's flag;
/// red before rule `parse`, which runs `kbf check-args`), failing a unit kbf takes, and
/// the two crates spelling `check-args` differently.
#[test]
fn check_units_parses_a_modes_flags_with_this_kbf() {
    assert_eq!(kbf_deploy::linux::modes::CHECK_ARGS, kbf_cli::CHECK_ARGS);
    let c = config();
    for mode in Mode::ALL {
        let text = render(&c, Some(1500), mode).expect("renders");
        // all-in-one is labelled trusted; the storage unit's key file is not on this
        // machine; nothing else is said.
        let notes: Vec<String> = match mode {
            Mode::AllInOne => vec![TRUSTED_NOTE.to_owned()],
            Mode::Storage | Mode::StorageAndDaemon => vec![
                "SKIP credential: no key file at /etc/kbf/s3.env yet, so its owner and mode were not checked"
                    .to_owned(),
            ],
            _ => Vec::new(),
        };
        assert_eq!(
            check_units(&format!("clean_{mode}"), mode, &text),
            (Vec::new(), notes),
            "{mode}"
        );
    }
    let bin = env!("CARGO_BIN_EXE_kbf");
    let text = render(&c, Some(1500), Mode::Daemon).expect("renders");
    for (flag, said) in [
        (
            "--bogus-flag",
            "error: unexpected argument '--bogus-flag' found",
        ),
        (
            "--cold-store=file:///x",
            "error: kbf daemon refuses --cold-store: a build node holds no store and no credential (docs/design/cluster.md, section 3.3)",
        ),
    ] {
        let edited = text.replace("--driver=container", &format!("--driver=container {flag}"));
        assert_ne!(edited, text);
        let (failures, notes) = check_units("edited", Mode::Daemon, &edited);
        assert_eq!(
            failures,
            [format!(
                "FAIL parse: kbf daemon refuses the flags of kbf-daemon.service ({bin} check-args: exit 2): {said}"
            )]
        );
        assert_eq!(notes, Vec::<String>::new());
    }
    // The storage node's parser, which refuses kbf-server's other flags by name.
    let text = render(&c, Some(1500), Mode::Storage).expect("renders");
    let edited = text.replace(
        "--meta-dir=",
        "--worker-listen=192.0.2.10:8981 \\\n    --meta-dir=",
    );
    assert_ne!(edited, text);
    let (failures, _) = check_units("edited_storage", Mode::Storage, &edited);
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert!(
        failures[0].starts_with(&format!(
            "FAIL parse: kbf storage refuses the flags of kbf-storage.service ({bin} check-args: exit 2): error: kbf storage refuses --worker-listen"
        )),
        "{failures:?}"
    );
}

/// What a `kbf` without `check-args` answers to `cmd`: clap's refusal, exit 2.
fn old_kbf(cmd: &Cmd) -> std::process::Output {
    use std::os::unix::process::ExitStatusExt as _;
    let argv = std::iter::once(cmd.program.as_str().to_owned()).chain(cmd.args.clone());
    let e = parse_from(argv)
        .map(drop)
        .expect_err("an old kbf refuses check-args");
    std::process::Output {
        status: std::process::ExitStatus::from_raw(e.exit_code() << 8),
        stdout: Vec::new(),
        stderr: e.render().to_string().into_bytes(),
    }
}

/// The upgrade note in docs/deploy/kbf-modes.md: an older `kbf`, without `check-args`,
/// fails rule `parse` with the line the doc quotes. Catches the doc's line going stale
/// (clap's wording, the rule's message) and an older kbf's refusal not failing the rule.
#[test]
fn an_older_kbf_fails_the_parse_rule_with_the_line_the_docs_quote() {
    let doc = include_str!("../../../docs/deploy/kbf-modes.md");
    let text = render(&config(), Some(1500), Mode::Daemon).expect("renders");
    let bin = std::path::Path::new("/usr/local/bin/kbf");
    let (failures, notes) = check_units_with(
        &Profiled {
            host: HostExec::new(Root::host()),
            old: true,
        },
        bin,
        "old_kbf",
        Mode::Daemon,
        &text,
    );
    assert_eq!(notes, Vec::<String>::new());
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert!(
        doc.contains(&format!("\n{}\n", failures[0])),
        "docs/deploy/kbf-modes.md does not quote:\n{}",
        failures[0]
    );
}

/// Catches `kbf frontend handover` starting instances the unit template does not have:
/// kbf-cli spells the template's name and instances itself (kbf-deploy is a dev-dependency
/// only), and the two must be the same words.
#[test]
fn the_handover_command_starts_the_instances_of_the_frontends_template() {
    use kbf_cli::handover::{INSTANCES, TEMPLATE};
    let instances: Vec<String> = INSTANCES
        .iter()
        .map(|i| format!("{TEMPLATE}@{i}.service"))
        .collect();
    assert_eq!(instances, Mode::Frontend.instances());
    assert_eq!(Mode::Frontend.unit_name(), format!("{TEMPLATE}@.service"));
    assert_eq!(TEMPLATE, format!("kbf-{}", Mode::Frontend.name()));
}
