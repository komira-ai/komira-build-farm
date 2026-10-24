//! The units and jobs of the host agent (`packaging/host-agent`) against the programs they start:
//! every command line parses as the program parses it, the two ends agree on the socket and on the
//! account that may connect to it, and the agent is not root.

use std::path::{Path, PathBuf};

use kbf_host_install::contract::local::ServeConfig;
use kbf_updater::agent::enroll::parse_flags as parse_enroll;
use kbf_updater::agent::flags::{DEFAULT_HELPER_SOCKET, parse_flags};

fn dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packaging/host-agent")
}

fn read(path: &str) -> String {
    std::fs::read_to_string(dir().join(path)).unwrap_or_else(|e| panic!("{path}: {e}"))
}

/// The value of `key=` in a unit file's (single) line for it.
fn unit_value(text: &str, key: &str) -> String {
    let found: Vec<&str> = text
        .lines()
        .filter_map(|l| l.strip_prefix(&format!("{key}=")))
        .collect();
    assert_eq!(found.len(), 1, "{key} appears once: {found:?}");
    found[0].to_owned()
}

/// The strings of the `ProgramArguments` array of a property list.
fn program_arguments(plist: &str) -> Vec<String> {
    let start = plist
        .find("<key>ProgramArguments</key>")
        .expect("ProgramArguments");
    let array = &plist[start..];
    let begin = array.find("<array>").expect("an array") + "<array>".len();
    let end = array.find("</array>").expect("the end of the array");
    array[begin..end]
        .split("<string>")
        .skip(1)
        .map(|s| s.split("</string>").next().expect("closed").to_owned())
        .collect()
}

/// The string after `<key>name</key>`.
fn plist_value(plist: &str, key: &str) -> String {
    let at = plist
        .find(&format!("<key>{key}</key>"))
        .unwrap_or_else(|| panic!("{key}"));
    let rest = &plist[at..];
    let open = rest.find("<string>").expect("a string") + "<string>".len();
    rest[open..]
        .split("</string>")
        .next()
        .expect("closed")
        .to_owned()
}

fn words(line: &str) -> Vec<String> {
    line.split_whitespace().map(str::to_owned).collect()
}

/// The agent's unit is the agent's command line: it parses with the flags the program has, runs
/// as an account that is not root, and reaches the helper at the socket the helper's unit binds;
/// the helper's unit is root's, lets exactly that account connect, and restarts a unit.
#[test]
fn the_linux_units_agree_with_the_programs_and_with_each_other() {
    let agent = read("linux/kbf-updater-agent.service");
    let line = words(&unit_value(&agent, "ExecStart"));
    assert_eq!(line[1], "agent");
    let config = parse_flags(&line[2..]).expect("the agent's flags");
    assert_eq!(
        config.node_api, None,
        "the shipped unit carries no operator token"
    );
    let exec = unit_value(&agent, "ExecStart");
    for word in ["token", "operator", "proxy"] {
        assert!(!exec.contains(word), "{word}: {exec}");
    }
    // The certificate is the enrollment's, in the unit's own state directory; the frontend is
    // where the node's drain is made (its worker listener) and the enroll listener is where the
    // certificate is renewed.
    let state = config
        .state_dir
        .expect("the unit names the enrollment's directory");
    assert!(config.frontend.is_some());
    assert_ne!(config.frontend, config.enroll_frontend);
    assert_eq!(
        state,
        Path::new("/var/lib").join(unit_value(&agent, "StateDirectory"))
    );
    assert_eq!(unit_value(&agent, "StateDirectoryMode"), "0700");
    assert_eq!(config.files.cert, state.join("pki/updater.pem"));
    assert_eq!(config.files.key, state.join("pki/updater.key"));

    // The enrollment command of the README is the program's, for this unit's host: the same
    // enroll listener and the same directory, with the token in a file inside it.
    let example = read("enroll-example.txt");
    let enroll = words(example.trim());
    assert_eq!(enroll[1], "enroll");
    let flags = parse_enroll(&enroll[2..]).expect("the enrollment's flags");
    assert_eq!(Some(flags.frontend), config.enroll_frontend);
    assert_eq!(flags.state_dir, state);
    assert!(flags.token_file.starts_with(&state));
    assert_eq!(config.helper_socket, Path::new(DEFAULT_HELPER_SOCKET));
    let user = unit_value(&agent, "User");
    assert_ne!(user, "root");
    assert!(unit_value(&agent, "After").contains("kbf-host-local.service"));

    let helper = read("linux/kbf-host-local.service");
    let line = words(&unit_value(&helper, "ExecStart"));
    assert_eq!(line[1], "--serve");
    let serve = ServeConfig::parse(&line[2..]).expect("the helper's flags");
    assert_eq!(serve.socket, Path::new(DEFAULT_HELPER_SOCKET));
    assert_eq!(serve.user, user, "only the agent's account may connect");
    assert_eq!(unit_value(&helper, "User"), "root");
    assert_eq!(unit_value(&helper, "Restart"), "always");
    assert!(!serve.units.is_empty());
    for unit in &serve.units {
        assert!(unit.as_str().starts_with("kbf-"));
    }
    assert!(DEFAULT_HELPER_SOCKET.starts_with(&format!(
        "/run/{}/",
        unit_value(&helper, "RuntimeDirectory")
    )));
    assert_eq!(unit_value(&helper, "StateDirectoryMode"), "0700");

    let sysusers = read("linux/kbf-updater.conf");
    let defined: Vec<&str> = sysusers
        .lines()
        .filter(|l| l.starts_with("u "))
        .filter_map(|l| l.split_whitespace().nth(1))
        .collect();
    assert_eq!(defined, [user.as_str()]);
}

/// The same on a Mac: the agent's job parses, runs as an account that is not root, and reaches the
/// helper at the socket the helper's job binds, for the account the helper lets in.
#[test]
fn the_macos_jobs_agree_with_the_programs_and_with_each_other() {
    let agent = read("macos/com.komira.kbf-updater-agent.plist");
    let args = program_arguments(&agent);
    assert_eq!(args[1], "agent");
    let config = parse_flags(&args[2..]).expect("the agent's flags");
    assert_eq!(
        config.node_api, None,
        "the shipped job carries no operator token"
    );
    for word in ["--token-file", "--operator-api", "proxy"] {
        assert!(!agent.contains(&format!("{word}</string>")), "{word}");
    }
    assert!(config.frontend.is_some());
    assert!(config.enroll_frontend.is_some());
    assert_eq!(config.state_dir, Some(PathBuf::from("/var/db/kbf-updater")));
    let user = plist_value(&agent, "UserName");
    assert_ne!(user, "root");

    let helper = read("macos/com.komira.kbf-host-install-local.plist");
    let args = program_arguments(&helper);
    assert_eq!(args[1], "--serve");
    let serve = ServeConfig::parse(&args[2..]).expect("the helper's flags");
    assert_eq!(config.helper_socket, serve.socket);
    assert_eq!(serve.user, user);
    assert!(
        serve.units.is_empty(),
        "a Mac restarts the daemon's job, not a unit"
    );
    assert_eq!(plist_value(&helper, "UserName"), "root");
    for plist in [&agent, &helper] {
        assert!(plist.contains("<key>KeepAlive</key>\n\t<true/>"));
        assert!(plist.contains("<key>RunAtLoad</key>\n\t<true/>"));
    }
}
