//! The ssh installer against a fake process runner that records what it was asked to run and
//! answers from a script.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;
use std::sync::Mutex;

use kbf_testkit::dir::TestDir;

use super::*;
use crate::inventory::Role;
use crate::ports::{Apply, Installed};

const KEY: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIExample";

/// What the fake saw of one call, including the `known_hosts` file as it was while "ssh" ran.
#[derive(Clone, Debug)]
struct Seen {
    spawn: Spawn,
    known_hosts_text: Option<String>,
    known_hosts_mode: Option<u32>,
    known_hosts_dir_mode: Option<u32>,
    known_hosts_path: Option<PathBuf>,
}

#[derive(Default)]
struct Fake {
    seen: Mutex<Vec<Seen>>,
    script: Mutex<VecDeque<Result<Exited, ExecError>>>,
}

impl Fake {
    fn says(&self, reply: Result<Exited, ExecError>) {
        self.script.lock().unwrap().push_back(reply);
    }

    fn calls(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
}

impl Exec for Fake {
    fn run(&self, spawn: &Spawn) -> Result<Exited, ExecError> {
        let path = spawn
            .args
            .iter()
            .filter_map(|a| a.to_str()?.strip_prefix("UserKnownHostsFile="))
            .map(PathBuf::from)
            .next();
        let mode = |p: &Path| {
            fs::metadata(p)
                .ok()
                .map(|m| m.permissions().mode() & 0o7777)
        };
        self.seen.lock().unwrap().push(Seen {
            spawn: spawn.clone(),
            known_hosts_text: path.as_ref().and_then(|p| fs::read_to_string(p).ok()),
            known_hosts_mode: path.as_deref().and_then(mode),
            known_hosts_dir_mode: path.as_deref().and_then(Path::parent).and_then(mode),
            known_hosts_path: path,
        });
        self.script
            .lock()
            .unwrap()
            .pop_front()
            .expect("an unscripted process was run")
    }
}

fn exited(code: i32, stdout: &str) -> Result<Exited, ExecError> {
    Ok(Exited {
        code: Some(code),
        stdout: stdout.as_bytes().to_vec(),
        stdout_overflow: false,
        stderr: Vec::new(),
    })
}

fn ssh_failed(stderr: &str) -> Result<Exited, ExecError> {
    Ok(Exited {
        code: Some(255),
        stdout: Vec::new(),
        stdout_overflow: false,
        stderr: stderr.as_bytes().to_vec(),
    })
}

const INSTALLED: &str =
    "{\"result\":\"installed\",\"installed_version\":\"1.2.3-1\",\"unit_state\":\"finished\"}\n";

fn host() -> Host {
    Host {
        alias: "st1".into(),
        address: "203.0.113.7".into(),
        role: Role::Storage,
        host_key: KEY.into(),
        node_id: None,
    }
}

fn version(text: &str) -> Version {
    Version::parse(text).expect("version")
}

struct Rig {
    // Declared first, dropped last.
    dir: TestDir,
    fake: Arc<Fake>,
    ssh: SshInstaller,
}

fn rig() -> Rig {
    let dir = TestDir::under(std::env::temp_dir(), "kbf-updater-ssh");
    let fake = Arc::new(Fake::default());
    let ssh = SshInstaller::new(
        PathBuf::from("/keys/updater"),
        "kbf-update",
        "stable",
        Duration::from_secs(7),
        Duration::from_secs(600),
    )
    .expect("installer")
    .with_work_dir(dir.path().to_path_buf())
    .with_exec(fake.clone());
    Rig { dir, fake, ssh }
}

fn strs(args: &[OsString]) -> Vec<&str> {
    args.iter().map(|a| a.to_str().expect("utf-8")).collect()
}

fn leftovers(dir: &Path) -> usize {
    fs::read_dir(dir).expect("dir").count()
}

/// Catches any drift in the one argument vector: an option dropped or added, the order of
/// `-T`, `--` and the destination, the key, the timeout, the command as ONE argument.
#[test]
fn the_argument_vector_is_exactly_this() {
    let r = rig();
    r.fake.says(exited(0, INSTALLED));
    r.ssh.install(&host(), &version("1.2.3-1")).expect("ok");
    let seen = &r.fake.calls()[0];
    let known = seen.known_hosts_path.clone().expect("known hosts option");
    assert_eq!(seen.spawn.program, PathBuf::from("/usr/bin/ssh"));
    let want = [
        "-F",
        "/dev/null",
        "-i",
        "/keys/updater",
        "-o",
        "BatchMode=yes",
        "-o",
        "ControlMaster=no",
        "-o",
        "ControlPath=none",
        "-o",
        "IdentitiesOnly=yes",
        "-o",
        "StrictHostKeyChecking=yes",
        "-o",
        &format!("UserKnownHostsFile={}", known.display()),
        "-o",
        "GlobalKnownHostsFile=/dev/null",
        "-o",
        "ConnectTimeout=7",
        "-o",
        "ServerAliveInterval=15",
        "-o",
        "ServerAliveCountMax=3",
        "-T",
        "--",
        "kbf-update@203.0.113.7",
        "install kbf=1.2.3-1 from stable",
    ];
    assert_eq!(strs(&seen.spawn.args), want);
    // No shell: no `-c`, and the program is the configured ssh.
    assert!(!strs(&seen.spawn.args).contains(&"-c"));
    assert_eq!(seen.spawn.timeout, Duration::from_secs(600));
    assert_eq!(seen.spawn.stdout_cap, answer::MAX_STDOUT);
}

/// Catches an option dropped on its own: the config file (`-F /dev/null`) and the
/// multiplexing off-switch (`ControlMaster=no`, `ControlPath=none`) each keep a user's or
/// a system's ssh config from changing who is trusted or whose session is reused.
#[test]
fn the_config_and_the_multiplexing_are_switched_off() {
    let r = rig();
    r.fake.says(exited(
        0,
        "{\"result\":\"query\",\"installed_version\":\"1\",\"running_commit\":null,\"in_progress\":false}\n",
    ));
    r.ssh.query(&host()).expect("ok");
    let args = strs(&r.fake.calls()[0].spawn.args).join(" ");
    assert!(args.starts_with("-F /dev/null "), "{args}");
    for needle in [
        "-o ControlMaster=no",
        "-o ControlPath=none",
        "-o BatchMode=yes",
        "-o IdentitiesOnly=yes",
        "-o StrictHostKeyChecking=yes",
        "-o GlobalKnownHostsFile=/dev/null",
        " -- ",
    ] {
        assert!(args.contains(needle), "missing {needle}: {args}");
    }
}

/// Catches the remote command drifting: a query is exactly `query`.
#[test]
fn a_query_is_the_bare_word_and_the_last_argument() {
    let r = rig();
    r.fake.says(exited(
        0,
        "{\"result\":\"query\",\"installed_version\":\"1\",\"running_commit\":null,\"in_progress\":false}\n",
    ));
    r.ssh.query(&host()).expect("ok");
    let args = strs(&r.fake.calls()[0].spawn.args)
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    assert_eq!(
        &args[args.len() - 3..],
        ["--", "kbf-update@203.0.113.7", "query"]
    );
}

/// Catches a pinned key that is not what ssh sees: the file holds the inventory's address
/// and key and nothing else, is private, lives in a private directory, and is gone after
/// the call with its directory.
#[test]
fn the_pinned_key_is_written_private_and_removed() {
    let r = rig();
    r.fake.says(exited(0, INSTALLED));
    r.ssh.install(&host(), &version("1.2.3-1")).expect("ok");
    let seen = &r.fake.calls()[0];
    assert_eq!(
        seen.known_hosts_text.as_deref(),
        Some(&*format!("203.0.113.7 {KEY}\n"))
    );
    assert_eq!(seen.known_hosts_mode, Some(0o600));
    assert_eq!(seen.known_hosts_dir_mode, Some(0o700));
    assert_eq!(
        leftovers(r.dir.path()),
        0,
        "the file and its directory are removed"
    );
}

/// Catches the file left behind on the failure paths: a timeout, an ssh failure and a bad
/// answer each remove it.
#[test]
fn the_pinned_key_is_removed_on_every_way_out() {
    let r = rig();
    r.fake.says(Err(ExecError::Timeout));
    r.fake.says(ssh_failed("Permission denied (publickey)."));
    r.fake.says(exited(0, "garbage\n"));
    r.fake.says(Err(ExecError::Spawn));
    for _ in 0..4 {
        assert!(r.ssh.install(&host(), &version("1")).is_err());
        assert_eq!(leftovers(r.dir.path()), 0);
    }
}

/// Catches a port lost: `host:port` is a `-p` and a bracketed known_hosts name, and the
/// destination carries the host only (ssh would read `user@host:22` as a host name).
#[test]
fn a_port_becomes_dash_p_and_a_bracketed_known_hosts_name() {
    let r = rig();
    r.fake.says(exited(0, INSTALLED));
    let mut h = host();
    h.address = "st1.farm.example:2222".into();
    r.ssh.install(&h, &version("1.2.3-1")).expect("ok");
    let seen = &r.fake.calls()[0];
    let args = strs(&seen.spawn.args);
    let at = args.iter().position(|a| *a == "-p").expect("-p");
    assert_eq!(args[at + 1], "2222");
    assert_eq!(args[at + 2], "-T");
    assert!(args.contains(&"kbf-update@st1.farm.example"));
    assert_eq!(
        seen.known_hosts_text.as_deref(),
        Some(&*format!("[st1.farm.example]:2222 {KEY}\n"))
    );
}

/// Catches port 22 written as `[host]:22`: OpenSSH looks a port-22 host up under the bare
/// name, so the pinned key would never match. The entry is the bare form and `-p` is omitted.
#[test]
fn port_22_is_the_bare_known_hosts_name_and_no_dash_p() {
    let r = rig();
    r.fake.says(exited(0, INSTALLED));
    let mut h = host();
    h.address = "st1.farm.example:22".into();
    r.ssh.install(&h, &version("1.2.3-1")).expect("ok");
    let seen = &r.fake.calls()[0];
    let args = strs(&seen.spawn.args);
    assert!(!args.contains(&"-p"));
    assert!(args.contains(&"kbf-update@st1.farm.example"));
    assert_eq!(
        seen.known_hosts_text.as_deref(),
        Some(&*format!("st1.farm.example {KEY}\n"))
    );
}

/// Catches a `%` in the work directory: ssh expands `%` tokens in `UserKnownHostsFile`, so
/// the pinned file would not be found. The path is refused like one with a blank.
#[test]
fn a_percent_in_the_work_directory_is_refused() {
    let r = rig();
    let odd = r.dir.path().join("a%d");
    fs::create_dir(&odd).expect("dir");
    let ssh = SshInstaller::new(
        PathBuf::from("/k"),
        "u",
        "stable",
        Duration::from_secs(1),
        Duration::from_secs(1),
    )
    .expect("installer")
    .with_work_dir(odd.clone())
    .with_exec(r.fake.clone());
    assert_eq!(
        ssh.query(&host()),
        Err(PortError::other("cannot write known hosts"))
    );
    assert!(r.fake.calls().is_empty(), "no process");
    assert_eq!(leftovers(&odd), 0);
}

/// Catches an environment that carries more than the three variables, or a missing `LANG=C`.
#[test]
fn the_environment_is_path_home_and_lang() {
    let r = rig();
    r.fake.says(exited(0, INSTALLED));
    r.ssh.install(&host(), &version("1")).expect("ok");
    let env = &r.fake.calls()[0].spawn.env;
    let mut names: Vec<&str> = env.iter().map(|(k, _)| k.to_str().unwrap()).collect();
    names.sort_unstable();
    assert!(
        names.iter().all(|n| ["HOME", "LANG", "PATH"].contains(n)),
        "{names:?}"
    );
    assert!(names.contains(&"LANG"));
    let lang = env
        .iter()
        .find(|(k, _)| k == "LANG")
        .map(|(_, v)| v.clone());
    assert_eq!(lang, Some(OsString::from("C")));
}

/// Catches a request the host would refuse reaching a process: every version outside the
/// host's grammar, and every channel, is refused before the runner is called.
#[test]
fn what_the_host_would_refuse_never_starts_a_process() {
    // A version made by hand (the type allows `1.0-`; the host does not).
    for bad in ["1.0-", "1-", &format!("1{}", "+".repeat(120))] {
        let r = rig();
        let v = Version::parse(bad);
        if let Ok(v) = v {
            assert!(r.ssh.install(&host(), &v).is_err(), "{bad}");
            assert!(r.fake.calls().is_empty(), "{bad} reached a process");
        } else {
            assert!(!host_accepts(bad));
        }
    }
    for bad in [
        "",
        "Stable",
        "-x",
        "a b",
        "a/b",
        "a_b",
        "a\nb",
        &"a".repeat(33),
    ] {
        assert_eq!(Channel::parse(bad), Err(ConfigError::Channel), "{bad:?}");
    }
    for ok in ["stable", "0", "nightly-2", &"a".repeat(32)] {
        assert!(Channel::parse(ok).is_ok(), "{ok}");
    }
    for ok in ["kbf-update", "_svc", "u_1", "u", &"u".repeat(32)] {
        assert!(user_ok(ok), "{ok}");
    }
    for bad in [
        "",
        "-root",
        "1user",
        "a b",
        "a@b",
        "A",
        "a.b",
        &"u".repeat(33),
    ] {
        let made = SshInstaller::new(
            PathBuf::from("/k"),
            bad,
            "stable",
            Duration::from_secs(1),
            Duration::from_secs(1),
        );
        assert!(matches!(made, Err(ConfigError::User)), "{bad:?}");
    }
    let zero = SshInstaller::new(
        PathBuf::from("/k"),
        "u",
        "stable",
        Duration::ZERO,
        Duration::from_secs(1),
    );
    assert!(matches!(zero, Err(ConfigError::Timeout)));
    let zero = SshInstaller::new(
        PathBuf::from("/k"),
        "u",
        "stable",
        Duration::from_secs(1),
        Duration::ZERO,
    );
    assert!(matches!(zero, Err(ConfigError::Timeout)));
}

/// Catches the version rule restated here loosening against its own corpus: every
/// hostile string is out, every real version in, and the unit-name bound counts `+` and `~`
/// twice.
#[test]
fn the_version_rule_restated_here_matches_the_corpus() {
    for s in crate::version::REJECTED {
        assert!(!host_accepts(s), "accepted {s:?}");
    }
    for s in crate::version::ACCEPTED {
        assert!(host_accepts(s), "refused {s:?}");
    }
    assert!(host_accepts(&format!("1{}", "+".repeat(114))));
    assert!(!host_accepts(&format!("1{}", "+".repeat(115))));
}

/// Catches a hand-made host that smuggles text into the pinned-key file or the destination:
/// a line break in the key, an option as the address, a bad key type. Nothing runs.
#[test]
fn a_hostile_host_record_never_starts_a_process() {
    for (address, key) in [
        (
            "203.0.113.7",
            "ssh-ed25519 AAAA\nother.example ssh-ed25519 BBBB",
        ),
        ("203.0.113.7", "ssh-ed25519  AAAA"),
        ("203.0.113.7", "ssh-dss AAAA"),
        ("203.0.113.7", "ssh-ed25519"),
        ("203.0.113.7", "ssh-ed25519 "),
        ("203.0.113.7", ""),
        ("-oProxyCommand=x", KEY),
        ("host name", KEY),
        ("host\n", KEY),
        ("", KEY),
        ("host:0", KEY),
    ] {
        let r = rig();
        let mut h = host();
        h.address = address.into();
        h.host_key = key.into();
        assert_eq!(
            r.ssh.install(&h, &version("1")),
            Err(PortError::other("bad host"))
        );
        assert_eq!(r.ssh.query(&h), Err(PortError::other("bad host")));
        assert!(r.fake.calls().is_empty());
    }
}

/// Catches each mapping of an install: installed and already installed are Ok; a refusal and
/// a failure carry their reason code and nothing else.
#[test]
fn every_install_answer_maps() {
    let ok = [
        INSTALLED,
        "{\"result\":\"already_installed\",\"installed_version\":\"1.2.3-1\",\"unit_state\":\"not_run\"}\n",
        // The line end is optional.
        "{\"result\":\"installed\",\"installed_version\":\"1.2.3-1\",\"unit_state\":\"finished\"}",
    ];
    for out in ok {
        let r = rig();
        r.fake.says(exited(0, out));
        assert!(r.ssh.install(&host(), &version("1.2.3-1")).is_ok(), "{out}");
    }
    let r = rig();
    r.fake.says(exited(
        2,
        "{\"result\":\"refused\",\"reason\":\"channel_not_allowed\"}\n",
    ));
    r.fake.says(exited(
        3,
        "{\"result\":\"failed\",\"reason\":\"install_failed\"}\n",
    ));
    let refused = r.ssh.install(&host(), &version("1")).expect_err("refused");
    assert_eq!(refused, PortError::other("refused: channel_not_allowed"));
    let failed = r.ssh.install(&host(), &version("1")).expect_err("failed");
    assert_eq!(failed, PortError::other("failed: install_failed"));
    assert!(!refused.is_unreachable() && !failed.is_unreachable());
}

/// Catches an answer taken for one when it is not: malformed, unknown fields, the wrong
/// result for the exit status, free text in a reason, several lines, an empty or non-UTF-8
/// body, a signal, an exit status the wrapper never uses.
#[test]
fn anything_else_is_a_bad_answer() {
    let done = INSTALLED.trim_end();
    let cases: Vec<(i32, Vec<u8>)> = vec![
        (0, b"".to_vec()),
        (0, b"\n".to_vec()),
        (0, b"not json\n".to_vec()),
        (0, b"{}\n".to_vec()),
        (0, b"[]\n".to_vec()),
        (0, format!("{done}\n{done}\n").into_bytes()),
        (0, format!("{done}\n\n").into_bytes()),
        (0, format!("motd line\n{done}\n").into_bytes()),
        (0, format!("{done} trailing\n").into_bytes()),
        (0, format!("{done}\r\n").into_bytes()),
        (0, done.replace("}", ",\"extra\":1}").into_bytes()),
        (
            0,
            done.replace("\"installed\"", "\"rebooted\"").into_bytes(),
        ),
        (0, done.replace("finished", "not_run").into_bytes()),
        (0, done.replace("1.2.3-1", "1.2 3").into_bytes()),
        (0, done.replace("1.2.3-1", "").into_bytes()),
        (0, done.replace("1.2.3-1", &"1".repeat(257)).into_bytes()),
        (0, vec![0xff, 0xfe, b'\n']),
        (
            0,
            b"{\"result\":\"refused\",\"reason\":\"bad_command\"}\n".to_vec(),
        ),
        (2, INSTALLED.as_bytes().to_vec()),
        (2, b"{\"result\":\"failed\",\"reason\":\"busy\"}\n".to_vec()),
        (
            3,
            b"{\"result\":\"refused\",\"reason\":\"busy\"}\n".to_vec(),
        ),
        (
            2,
            b"{\"result\":\"refused\",\"reason\":\"Permission denied (publickey)\"}\n".to_vec(),
        ),
        (2, b"{\"result\":\"refused\",\"reason\":\"\"}\n".to_vec()),
        (
            3,
            b"{\"result\":\"failed\",\"reason\":\"a\\nb\"}\n".to_vec(),
        ),
        (
            3,
            format!(
                "{{\"result\":\"failed\",\"reason\":\"{}\"}}\n",
                "x".repeat(49)
            )
            .into_bytes(),
        ),
        (1, INSTALLED.as_bytes().to_vec()),
        (4, INSTALLED.as_bytes().to_vec()),
        (-1, INSTALLED.as_bytes().to_vec()),
    ];
    for (code, out) in cases {
        let r = rig();
        r.fake.says(Ok(Exited {
            code: Some(code),
            stdout: out.clone(),
            stdout_overflow: false,
            stderr: vec![],
        }));
        assert_eq!(
            r.ssh.install(&host(), &version("1.2.3-1")),
            Err(PortError::other("bad answer")),
            "{code} {:?}",
            String::from_utf8_lossy(&out)
        );
    }
    // Killed by a signal: no exit status at all.
    let r = rig();
    r.fake.says(Ok(Exited {
        code: None,
        stdout: INSTALLED.into(),
        stdout_overflow: false,
        stderr: vec![],
    }));
    assert_eq!(
        r.ssh.install(&host(), &version("1")),
        Err(PortError::other("bad answer"))
    );
}

/// Catches the size cap: output past it is refused even when it starts with a good answer,
/// whether the runner flagged the overflow or only returned a long body; a body of exactly
/// the cap is still read.
#[test]
fn oversize_output_is_refused() {
    let padded = |n: usize| format!("{}{}\n", INSTALLED.trim_end(), " ".repeat(n));
    let at_cap = padded(answer::MAX_STDOUT - INSTALLED.len());
    assert_eq!(at_cap.len(), answer::MAX_STDOUT);
    // Trailing blanks are not JSON whitespace the line reader would take for a shape.
    // Exactly at the cap it is still parsed, and JSON allows the blanks.
    let r = rig();
    r.fake.says(exited(0, &at_cap));
    assert!(r.ssh.install(&host(), &version("1.2.3-1")).is_ok());
    let over = padded(answer::MAX_STDOUT - INSTALLED.len() + 1);
    let r = rig();
    r.fake.says(exited(0, &over));
    assert_eq!(
        r.ssh.install(&host(), &version("1")),
        Err(PortError::other("bad answer"))
    );
    let r = rig();
    r.fake.says(Ok(Exited {
        code: Some(0),
        stdout: INSTALLED.into(),
        stdout_overflow: true,
        stderr: vec![],
    }));
    assert_eq!(
        r.ssh.install(&host(), &version("1")),
        Err(PortError::other("bad answer"))
    );
}

/// Catches a timeout that is reported as a failure of the call: the install keeps running in
/// the host's unit, so the roll must re-query (the unreachable kind), never fail the roll. Also
/// catches a spawn failure that leaks its reason: that one is a fixed line and not unreachable.
#[test]
fn a_timeout_is_unreachable_and_a_missing_ssh_is_not() {
    let r = rig();
    r.fake.says(Err(ExecError::Timeout));
    r.fake.says(Err(ExecError::Timeout));
    r.fake.says(Err(ExecError::Spawn));
    let install = r.ssh.install(&host(), &version("1")).expect_err("timeout");
    assert_eq!(install, PortError::unreachable("timeout"));
    assert!(install.is_unreachable());
    let query = r.ssh.query(&host()).expect_err("timeout");
    assert_eq!(query, PortError::unreachable("timeout"));
    assert!(query.is_unreachable());
    let spawn = r.ssh.query(&host()).expect_err("spawn");
    assert_eq!(spawn, PortError::other("cannot run ssh"));
    assert!(!spawn.is_unreachable());
}

/// Catches the inventory alias reaching ssh: it is a label from a file an operator edits, so a
/// hostile one (spaces, an option, a newline, `;`) must appear nowhere in the argument vector
/// or the known-hosts file. The address is the only name used.
#[test]
fn a_hostile_alias_never_reaches_ssh() {
    let aliases = [
        "a b",
        "-oProxyCommand=touch /x",
        "st1\nHost *",
        "st1;reboot",
        "$(id)",
    ];
    for alias in aliases {
        let r = rig();
        r.fake.says(exited(0, INSTALLED));
        r.fake.says(exited(0, "{\"result\":\"query\",\"installed_version\":\"1\",\"running_commit\":null,\"in_progress\":false}\n"));
        let mut h = host();
        h.alias = alias.into();
        let _ = r.ssh.install(&h, &version("1.2.3-1"));
        let _ = r.ssh.query(&h);
        let calls = r.fake.calls();
        assert_eq!(calls.len(), 2, "{alias:?}");
        for seen in calls {
            for arg in &seen.spawn.args {
                let arg = arg.to_str().expect("utf-8");
                assert!(!arg.contains(alias), "{alias:?} in argv: {arg:?}");
            }
            assert!(strs(&seen.spawn.args).contains(&"kbf-update@203.0.113.7"));
            let known = seen.known_hosts_text.expect("known hosts");
            assert!(!known.contains(alias), "{alias:?} in known hosts");
        }
    }
}

/// Catches an ssh failure (exit 255) mapped as a failed install: it is the unreachable kind,
/// with a closed text for what stderr said, for both verbs, whatever stdout holds.
#[test]
fn exit_255_is_unreachable_and_never_failed() {
    let cases = [
        ("Host key verification failed.", "unreachable: host key"),
        (
            "@@@ WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED! @@@",
            "unreachable: host key",
        ),
        (
            "kbf-update@203.0.113.7: Permission denied (publickey).",
            "unreachable: auth",
        ),
        (
            "ssh: connect to host 203.0.113.7 port 22: Connection refused",
            "unreachable: connect",
        ),
        (
            "ssh: connect to host 203.0.113.7 port 22: Connection timed out",
            "unreachable: connect",
        ),
        (
            "ssh: Could not resolve hostname x: Name or service not known",
            "unreachable: connect",
        ),
        ("something new", "unreachable: ssh"),
        ("", "unreachable: ssh"),
    ];
    for (stderr, text) in cases {
        let r = rig();
        r.fake.says(ssh_failed(stderr));
        r.fake.says(Ok(Exited {
            code: Some(255),
            stdout: INSTALLED.into(),
            stdout_overflow: false,
            stderr: stderr.into(),
        }));
        let install = r
            .ssh
            .install(&host(), &version("1"))
            .expect_err("unreachable");
        assert!(install.is_unreachable(), "{stderr}");
        assert_eq!(install.to_string(), text);
        let query = r.ssh.query(&host()).expect_err("unreachable");
        assert!(query.is_unreachable());
        assert_eq!(query.to_string(), text);
    }
}

/// Catches raw ssh or apt text in an error: a stderr full of distinctive words, and a reason
/// that carries them, reach no error text.
#[test]
fn raw_stderr_never_reaches_an_error() {
    let words = ["TOPSECRET-banner", "E: Unable to locate package"];
    let noisy = format!("{} {}", words[0], words[1]);
    let r = rig();
    r.fake.says(ssh_failed(&noisy));
    r.fake.says(Ok(Exited {
        code: Some(3),
        stdout: b"{\"result\":\"failed\",\"reason\":\"install_failed\"}\n".to_vec(),
        stdout_overflow: false,
        stderr: noisy.clone().into_bytes(),
    }));
    r.fake.says(Ok(Exited {
        code: Some(0),
        stdout: b"garbage\n".to_vec(),
        stdout_overflow: false,
        stderr: noisy.clone().into_bytes(),
    }));
    for _ in 0..3 {
        let e = r.ssh.install(&host(), &version("1")).expect_err("error");
        let shown = format!("{e} {e:?}");
        for w in words {
            assert!(!shown.contains(w), "{shown}");
        }
    }
}

/// Catches a Mac's install answer being misread: `apply_started` for the version asked is the
/// answer `ApplyStarted`; another version, a commit that is not hex, an apply that is not
/// `running`, an extra field, a missing one, and the same line with a refusal's exit status
/// are each a bad answer.
#[test]
fn a_mac_install_answers_apply_started() {
    let line = |version: &str, commit: &str, apply: &str| {
        format!(
            "{{\"result\":\"apply_started\",\"version\":\"{version}\",\"commit\":\"{commit}\",\"apply\":\"{apply}\"}}\n"
        )
    };
    let r = rig();
    r.fake
        .says(exited(0, &line("1.2.3-1", "0123456789ab", "running")));
    assert_eq!(
        r.ssh.install(&host(), &version("1.2.3-1")),
        Ok(InstallAnswer::ApplyStarted)
    );
    let bad = [
        (0, line("9.9", "0123456789ab", "running")),
        (0, line("1.2.3-1", "not-hex", "running")),
        (0, line("1.2.3-1", "", "running")),
        (0, line("1.2.3-1", &"a".repeat(65), "running")),
        (0, line("1.2.3-1", "0123456789ab", "done")),
        (
            0,
            "{\"result\":\"apply_started\",\"version\":\"1.2.3-1\",\"commit\":\"ab\"}\n".to_owned(),
        ),
        (
            0,
            "{\"result\":\"apply_started\",\"version\":\"1.2.3-1\",\"commit\":\"ab\",\"apply\":\"running\",\"x\":1}\n"
                .to_owned(),
        ),
        (3, line("1.2.3-1", "0123456789ab", "running")),
    ];
    for (code, out) in bad {
        let r = rig();
        r.fake.says(exited(code, &out));
        assert_eq!(
            r.ssh.install(&host(), &version("1.2.3-1")),
            Err(PortError::other("bad answer")),
            "{out}"
        );
    }
}

/// Catches a Mac's apply state being misread from its query: `running`, `done` and
/// `failed:<reason>` (lower case, `_` and `:`, up to 64 bytes) are read; a line with no `apply`
/// (a Linux host) and `null` read as none; any other text, an empty or over-long or upper-case
/// reason, is a bad answer. The rest of the report is unchanged.
#[test]
fn a_mac_query_carries_the_apply_state() {
    let q = |apply: &str| {
        format!(
            "{{\"result\":\"query\",\"installed_version\":\"1.2.3-1\",\"running_commit\":\"abc123\",\"node_id\":\"mac-1\",\"in_progress\":false,\"apply\":{apply}}}\n"
        )
    };
    let good = [
        ("\"running\"", Some(Apply::Running)),
        ("\"done\"", Some(Apply::Done)),
        (
            "\"failed:health_timeout:rolled_back\"",
            Some(Apply::Failed("health_timeout:rolled_back".to_owned())),
        ),
        ("null", None),
    ];
    for (text, want) in good {
        let r = rig();
        r.fake.says(exited(0, &q(text)));
        let report = r.ssh.report(&host()).expect(text);
        assert_eq!(report.apply, want, "{text}");
        assert_eq!(report.installed, Installed::Version("1.2.3-1".into()));
        assert_eq!(report.running_commit.as_deref(), Some("abc123"));
    }
    let r = rig();
    r.fake.says(exited(
        0,
        "{\"result\":\"query\",\"installed_version\":\"1\",\"running_commit\":null,\"node_id\":null,\"in_progress\":false}\n",
    ));
    assert_eq!(r.ssh.report(&host()).expect("linux").apply, None);
    let long = format!("\"failed:{}\"", "a".repeat(65));
    let edge = format!("\"failed:{}\"", "a".repeat(64));
    let bad = [
        "\"paused\"",
        "\"failed:\"",
        "\"failed\"",
        "\"failed:Upper\"",
        "\"failed:a b\"",
        "\"Running\"",
        "7",
        long.as_str(),
    ];
    for text in bad {
        let r = rig();
        r.fake.says(exited(0, &q(text)));
        assert_eq!(
            r.ssh.report(&host()),
            Err(PortError::other("bad answer")),
            "{text}"
        );
    }
    let r = rig();
    r.fake.says(exited(0, &q(&edge)));
    assert!(r.ssh.report(&host()).is_ok());
}

/// Catches the query mapping: in progress wins, a version is the version, a null is "not
/// installed", a refusal or failure carries its code, and nonsense is a bad answer.
#[test]
fn every_query_answer_maps() {
    let q = |v: &str, p: bool| {
        format!(
            "{{\"result\":\"query\",\"installed_version\":{v},\"running_commit\":\"abc123\",\"in_progress\":{p}}}\n"
        )
    };
    let r = rig();
    r.fake.says(exited(0, &q("\"1.2.3-1\"", false)));
    r.fake.says(exited(0, &q("\"1.2.3-1\"", true)));
    r.fake.says(exited(0, &q("null", true)));
    r.fake.says(exited(0, &q("null", false)));
    r.fake.says(exited(0, &q("\"1:2.0-1\"", false)));
    r.fake.says(exited(0, &q("\"two words\"", false)));
    r.fake.says(exited(0, &q("7", false)));
    r.fake.says(exited(
        2,
        "{\"result\":\"refused\",\"reason\":\"bad_command\"}\n",
    ));
    r.fake.says(exited(
        3,
        "{\"result\":\"failed\",\"reason\":\"query_failed\"}\n",
    ));
    r.fake.says(exited(0, INSTALLED));
    r.fake.says(exited(1, &q("\"1\"", false)));
    r.fake.says(exited(4, &q("\"1\"", false)));
    r.fake
        .says(exited(0, &q(&format!("\"{}\"", "1".repeat(256)), false)));
    r.fake
        .says(exited(0, &q(&format!("\"{}\"", "1".repeat(257)), false)));
    // The host's query line carries `result: "query"`; a line without it is not its answer.
    r.fake.says(exited(
        0,
        "{\"installed_version\":\"1\",\"running_commit\":null,\"in_progress\":false}\n",
    ));
    let got: Vec<_> = (0..15).map(|_| r.ssh.query(&host())).collect();
    assert_eq!(got[0], Ok(Installed::Version("1.2.3-1".into())));
    assert_eq!(got[1], Ok(Installed::InProgress));
    assert_eq!(got[2], Ok(Installed::InProgress));
    assert_eq!(got[3], Ok(Installed::NotInstalled));
    assert_eq!(got[4], Ok(Installed::Version("1:2.0-1".into())));
    assert_eq!(got[5], Err(PortError::other("bad answer")));
    assert_eq!(got[6], Err(PortError::other("bad answer")));
    assert_eq!(got[7], Err(PortError::other("refused: bad_command")));
    assert_eq!(got[8], Err(PortError::other("failed: query_failed")));
    assert_eq!(got[9], Err(PortError::other("bad answer")));
    assert_eq!(got[10], Err(PortError::other("bad answer")));
    assert_eq!(got[11], Err(PortError::other("bad answer")));
    assert_eq!(got[12], Ok(Installed::Version("1".repeat(256))));
    assert_eq!(got[13], Err(PortError::other("bad answer")));
    assert_eq!(got[14], Err(PortError::other("bad answer")));
}

/// Catches a report that drops the node id or the running commit, and one that lets the
/// host put free text there: a node id must be a DNS label and a commit hex digits, else the
/// whole answer is bad.
#[test]
fn the_report_carries_the_node_id_and_the_running_commit() {
    let line = |commit: &str, id: &str| {
        format!(
            "{{\"result\":\"query\",\"installed_version\":\"1\",\"running_commit\":{commit},\"node_id\":{id},\"in_progress\":false}}\n"
        )
    };
    let r = rig();
    r.fake.says(exited(0, &line("\"0123abcd\"", "\"dm-1\"")));
    r.fake.says(exited(0, &line("null", "null")));
    r.fake.says(exited(0, &line("\"xyz\"", "\"dm-1\"")));
    r.fake
        .says(exited(0, &line(&format!("\"{}\"", "a".repeat(65)), "null")));
    r.fake.says(exited(0, &line("null", "\"Bad Id\"")));
    r.fake.says(exited(0, &line("\"\"", "null")));
    let got: Vec<_> = (0..6).map(|_| r.ssh.report(&host())).collect();
    let first = got[0].clone().expect("read");
    assert_eq!(first.running_commit.as_deref(), Some("0123abcd"));
    assert_eq!(first.node_id.as_deref(), Some("dm-1"));
    assert_eq!(first.installed, Installed::Version("1".into()));
    let none = got[1].clone().expect("read");
    assert_eq!((none.running_commit, none.node_id), (None, None));
    for bad in &got[2..] {
        assert_eq!(bad, &Err(PortError::other("bad answer")));
    }
}

/// Catches a non-whole-second connect timeout rounded down, and a work directory whose
/// path ssh would split.
#[test]
fn the_connect_timeout_rounds_up_and_a_blank_in_the_path_is_refused() {
    let r = rig();
    r.fake.says(exited(0, INSTALLED));
    let ssh = SshInstaller::new(
        PathBuf::from("/k"),
        "u",
        "stable",
        Duration::from_millis(1200),
        Duration::from_secs(5),
    )
    .expect("installer")
    .with_work_dir(r.dir.path().to_path_buf())
    .with_exec(r.fake.clone());
    ssh.install(&host(), &version("1")).expect("ok");
    assert!(strs(&r.fake.calls()[0].spawn.args).contains(&"ConnectTimeout=2"));

    let spaced = r.dir.path().join("a b");
    fs::create_dir(&spaced).expect("dir");
    let ssh = SshInstaller::new(
        PathBuf::from("/k"),
        "u",
        "stable",
        Duration::from_secs(1),
        Duration::from_secs(1),
    )
    .expect("installer")
    .with_work_dir(spaced.clone())
    .with_exec(r.fake.clone());
    assert_eq!(
        ssh.query(&host()),
        Err(PortError::other("cannot write known hosts"))
    );
    assert_eq!(r.fake.calls().len(), 1, "no second process");
    assert_eq!(leftovers(&spaced), 0);
}

/// Catches ssh and work directory overrides ignored.
#[test]
fn the_configured_ssh_is_the_program() {
    let r = rig();
    r.fake.says(exited(0, INSTALLED));
    let ssh = SshInstaller::new(
        PathBuf::from("/k"),
        "u",
        "stable",
        Duration::from_secs(1),
        Duration::from_secs(1),
    )
    .expect("installer")
    .with_ssh(PathBuf::from("/opt/ssh"))
    .with_work_dir(r.dir.path().to_path_buf())
    .with_exec(r.fake.clone());
    ssh.install(&host(), &version("1")).expect("ok");
    assert_eq!(r.fake.calls()[0].spawn.program, PathBuf::from("/opt/ssh"));
}

/// Catches a work directory that cannot be written reported as anything but the fixed line,
/// and a process started anyway.
#[test]
fn an_unwritable_work_directory_stops_the_call() {
    let r = rig();
    let ssh = SshInstaller::new(
        PathBuf::from("/k"),
        "u",
        "stable",
        Duration::from_secs(1),
        Duration::from_secs(1),
    )
    .expect("installer")
    .with_work_dir(r.dir.path().join("missing"))
    .with_exec(r.fake.clone());
    assert_eq!(
        ssh.query(&host()),
        Err(PortError::other("cannot write known hosts"))
    );
    assert!(r.fake.calls().is_empty());
}
