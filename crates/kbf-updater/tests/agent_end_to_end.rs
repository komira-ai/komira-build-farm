//! The agent as a process would run it, with everything it talks to played by threads of the
//! test: the coordinator over mutual TLS, the frontend's operator API on loopback, and the
//! helper's socket speaking the real protocol. One update, from the coordinator's order to the
//! heartbeat that says it succeeded.

mod support;

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use kbf_testkit::dir::TestDir;
use kbf_types::HostRole;
use support::{Events, OPERATOR_TOKEN, Pki, VB};

/// How the agent reaches the frontend in a run.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Way {
    /// The opt-in fallback: the operator token over the frontend's loopback API.
    Token,
    /// The shipped way: the host's certificate on the frontend's worker listener, no token.
    Certificate,
}

/// A daemon host is told to update to B and does: the helper installs it, the node is drained
/// at the frontend before the helper switches, the node's health is read, B is confirmed, and
/// the node is given back; and the heartbeats say all along who is not named (no node id, no
/// role), that an update is running, and at the end that it succeeded.
#[test]
fn a_daemon_host_updates_from_the_order_to_the_heartbeat_that_says_it_worked() {
    an_update_by(Way::Token);
}

/// The same update with no token anywhere: the agent is given `--frontend` and its certificate
/// and nothing else, the frontend sees the node's own certificate on every call and no
/// `Authorization` header, and the drain, the health reads and the undrain happen in the same
/// order around the helper's switch.
#[test]
fn a_daemon_host_updates_with_its_certificate_alone_and_no_token() {
    an_update_by(Way::Certificate);
}

fn an_update_by(way: Way) {
    let dir = TestDir::under(std::env::temp_dir(), "agent-e2e");
    let pki = Pki::new();
    let leaf = pki.updater("n1", HostRole::Daemon);
    pki.write_updater_files(&dir, &leaf);
    let token = dir.join("operator-token");
    if way == Way::Token {
        std::fs::write(&token, OPERATOR_TOKEN).unwrap();
        std::fs::set_permissions(&token, std::os::unix::fs::PermissionsExt::from_mode(0o600))
            .unwrap();
    } else {
        assert!(!token.exists(), "no token file is made for this run");
    }

    let events = Events::default();
    let beats = Arc::new(Mutex::new(Vec::new()));
    let stop = Arc::new(AtomicBool::new(false));
    // The coordinator orders B until a heartbeat says B succeeded.
    let (coordinator_addr, coordinator) = support::coordinator(
        &pki,
        &events,
        &beats,
        |beat| {
            if beat["last_update"]["outcome"] == "succeeded" {
                r#"{"reply":"nothing"}"#.to_owned()
            } else {
                format!(r#"{{"reply":"update","version":"{VB}"}}"#)
            }
        },
        &stop,
    );
    let (api_addr, api) = match way {
        Way::Token => support::operator_api(&events, &stop),
        Way::Certificate => support::frontend_worker(&pki, &events, &stop),
    };
    let socket = dir.join("local.sock");
    let helper = support::helper(&socket, &events, &stop);

    let args: Vec<String> = [
        "kbf-updater",
        "agent",
        "--coordinator",
        &format!("localhost:{}", coordinator_addr.port()),
        "--client-cert",
        &dir.join("cert.pem").display().to_string(),
        "--client-key",
        &dir.join("key.pem").display().to_string(),
        "--ca",
        &dir.join("ca.pem").display().to_string(),
        "--channel",
        "stable",
        "--helper-socket",
        &socket.display().to_string(),
        "--heartbeat-secs",
        "1",
        "--health-poll-secs",
        "1",
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .chain(match way {
        Way::Token => vec![
            "--operator-api".to_owned(),
            format!("http://{api_addr}"),
            "--token-file".to_owned(),
            token.display().to_string(),
        ],
        Way::Certificate => vec![
            "--frontend".to_owned(),
            format!("localhost:{}", api_addr.port()),
        ],
    })
    .collect();

    let watched = Arc::clone(&beats);
    let (code, text) = kbf_updater::run_agent_with(&args, move |agent| {
        std::thread::spawn(move || {
            let start = Instant::now();
            while !watched
                .lock()
                .unwrap()
                .iter()
                .any(|b| b["last_update"]["outcome"] == "succeeded")
                && start.elapsed() < Duration::from_secs(60)
            {
                std::thread::sleep(Duration::from_millis(20));
            }
            agent.stop();
        });
    });
    stop.store(true, Ordering::SeqCst);
    coordinator.join().unwrap();
    api.join().unwrap();
    helper.join().unwrap();
    assert_eq!((code, text.as_str()), (0, ""));
    if way == Way::Certificate {
        let all = events.all();
        let calls = all.iter().filter(|e| e.starts_with("api:client")).count();
        assert!(calls >= 3, "{all:?}");
        for event in &all {
            assert_ne!(
                event, "api:credential",
                "a call carried a credential: {all:?}"
            );
            assert_ne!(event, "api:client none", "{all:?}");
        }
        assert!(
            all.iter()
                .filter(|e| e.starts_with("api:client"))
                .all(|e| e == "api:client n1"),
            "{all:?}"
        );
    }

    // The A/B, in order, across the three.
    let all = events.all();
    let steps: Vec<&str> = all
        .iter()
        .map(String::as_str)
        .filter(|e| {
            e.starts_with("helper:")
                || e == &"api:drain"
                || e == &"api:uncordon"
                || e.starts_with("api:node")
        })
        .collect();
    let position = |needle: &str| {
        steps
            .iter()
            .position(|e| e.starts_with(needle))
            .unwrap_or_else(|| panic!("{needle} in {steps:?}"))
    };
    assert_eq!(
        steps
            .iter()
            .filter(|e| e.starts_with("helper:install"))
            .count(),
        1,
        "{steps:?}"
    );
    assert!(
        steps
            .iter()
            .any(|e| *e == format!("helper:install install kbf={VB} from stable"))
    );
    assert!(
        position("helper:install") < position("api:drain"),
        "{steps:?}"
    );
    assert!(
        position("api:drain") < position("helper:switch"),
        "{steps:?}"
    );
    assert!(
        position("helper:switch") < position("helper:confirm"),
        "{steps:?}"
    );
    assert!(
        position("helper:confirm") < position("api:uncordon"),
        "{steps:?}"
    );
    assert!(
        steps[position("helper:switch")..position("helper:confirm")].contains(&"api:node drained"),
        "the node is judged while drained: {steps:?}"
    );

    // What the heartbeats said.
    let beats = beats.lock().unwrap();
    let first = &beats[0];
    assert_eq!(
        first["version"], "0.1.0+gaaaaaaaaaaaa",
        "the version of a release placed by hand"
    );
    assert_eq!(first["platform"], "linux-x86_64");
    for beat in beats.iter() {
        let mut keys: Vec<_> = beat.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        assert_eq!(
            keys,
            ["health", "last_update", "platform", "updating", "version"]
        );
        for forbidden in ["n1", "daemon", "node", "role"] {
            assert!(!beat.to_string().contains(forbidden), "{forbidden}: {beat}");
        }
    }
    let during = beats.iter().filter(|b| b["updating"] == VB).count();
    assert!(
        during >= 2,
        "the heartbeat went on through the install: {during}"
    );
    let done = beats
        .iter()
        .rev()
        .find(|b| b["last_update"]["outcome"] == "succeeded")
        .unwrap();
    assert_eq!(done["last_update"]["target"], VB);
    assert_eq!(done["version"], VB);
}
