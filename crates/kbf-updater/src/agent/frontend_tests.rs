//! The agent's calls to the frontend with the host's own certificate and no token, against a
//! frontend played by a TLS server in the test: what it presents, what it sends (the three
//! calls about its node and nothing else), whom it will talk to, and what the agent builds from
//! the flags it is given.

use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::thread::JoinHandle;
use std::time::Duration;

use kbf_testkit::dir::TestDir;
use kbf_types::HostRole;

use super::build::{NO_CREDENTIAL, node_ports};
use super::coordinator::{Endpoint, TlsFiles};
use super::coordinator_tests::{Plan, Seen, http, ok, serve, server_config};
use super::flags::{Config, NodeApi, Timing};
use super::local::{Limits, SocketLocal};
use super::pki::{Leaf, Pki, write_files};
use super::testing::host;
use crate::identity::{Identity, identity_of};
use crate::inventory::Role;
use crate::operator::OperatorApi;
use crate::ports::PortErrorKind;
use crate::version::Version;

/// A storage host's own view: its member active on the commit `aaaaaaaaaaaa`, nothing owed.
const OWN: &str = r#"{"node":"n1","half":"member","member":{"id":1,"state":"active","build_commit":"aaaaaaaaaaaa"},"storage":{"members_active":3,"under_replicated_blobs":0}}"#;

const NODE: &str = r#"{"placement":{"state":"draining"},"leases":["a","b"],"connected":true}"#;

struct Rig {
    pki: Pki,
    dir: TestDir,
    files: TlsFiles,
    updater: Leaf,
}

impl Rig {
    fn new(name: &str) -> Self {
        let pki = Pki::new();
        let dir = TestDir::under(std::env::temp_dir(), name);
        let updater = pki.updater("n1", HostRole::Daemon);
        let files = write_files(&dir, &pki, &updater);
        Self {
            pki,
            dir,
            files,
            updater,
        }
    }

    fn identity(&self) -> Identity {
        identity_of(&self.updater.der).unwrap()
    }

    /// A frontend that answers `answers` in turn, with the proxy's certificate from this CA.
    fn frontend(&self, answers: Vec<Plan>) -> (SocketAddr, JoinHandle<Vec<Seen>>) {
        serve(server_config(&self.pki, &self.pki.frontend()), answers)
    }

    fn api(&self, addr: SocketAddr) -> OperatorApi {
        OperatorApi::as_updater(
            Endpoint::parse(&format!("localhost:{}", addr.port())).unwrap(),
            self.files.clone(),
            self.identity(),
        )
        .with_timeout(Duration::from_secs(5))
    }

    fn config(&self, addr: Option<SocketAddr>) -> Config {
        Config {
            coordinator: Endpoint::parse("localhost:1").unwrap(),
            files: self.files.clone(),
            channel: "stable".to_owned(),
            helper_socket: self.dir.join("no-helper.sock"),
            frontend: addr.map(|a| Endpoint::parse(&format!("localhost:{}", a.port())).unwrap()),
            enroll_frontend: None,
            state_dir: None,
            node_api: None,
            timing: Timing::DEFAULT,
        }
    }
}

fn head_of(seen: &Seen) -> (Vec<String>, String) {
    let text = String::from_utf8(seen.request.clone()).unwrap();
    let (head, body) = text.split_once("\r\n\r\n").unwrap();
    (head.lines().map(str::to_owned).collect(), body.to_owned())
}

/// A daemon host's agent drains, reads and undrains its own node through the frontend with its
/// updater certificate: the three calls, the paths of the operator API, the node id of the
/// certificate in each, mutual TLS with that certificate, and no `Authorization` header, no
/// proxy secret and no token anywhere in a request.
#[test]
fn the_agent_drains_reads_and_undrains_its_node_with_its_certificate_and_no_token() {
    let rig = Rig::new("fe-three");
    let (addr, server) = rig.frontend(vec![ok("{}"), ok(NODE), ok("{}")]);
    let api = rig.api(addr);
    let drain = super::build::drain_for(Role::Daemon, &api);
    let h = host(Role::Daemon);
    assert_eq!(drain.drain(&h, Duration::from_secs(600)), Ok(()));
    assert_eq!(drain.leases(&h), Ok(2));
    assert_eq!(drain.undrain(&h), Ok(()));

    let seen = server.join().unwrap();
    let calls: Vec<(String, String)> = seen
        .iter()
        .map(|s| {
            let (lines, body) = head_of(s);
            (lines[0].clone(), body)
        })
        .collect();
    assert_eq!(
        calls,
        [
            (
                "POST /v1/nodes/n1:drain HTTP/1.1".to_owned(),
                r#"{"deadline_secs":600}"#.to_owned()
            ),
            ("GET /v1/nodes/n1 HTTP/1.1".to_owned(), String::new()),
            (
                "POST /v1/nodes/n1:uncordon HTTP/1.1".to_owned(),
                String::new()
            ),
        ]
    );
    for one in &seen {
        let (lines, _) = head_of(one);
        assert!(lines.contains(&format!("Host: localhost:{}", addr.port())));
        for line in &lines {
            let lower = line.to_ascii_lowercase();
            assert!(
                !lower.starts_with("authorization") && !lower.starts_with("x-kbf-"),
                "{line}"
            );
        }
        let who = identity_of(one.client.as_deref().expect("a client certificate")).unwrap();
        assert_eq!(
            (who.node_id.to_string(), who.role),
            ("n1".to_owned(), HostRole::Daemon)
        );
    }
}

/// The node's status for the health check is the same call, and a daemon that is connected and
/// held (drained) is judged by it alone, not by anything of the operator API.
#[test]
fn a_daemon_hosts_health_is_read_from_its_own_node_through_the_frontend() {
    let rig = Rig::new("fe-health");
    let (addr, server) = rig.frontend(vec![ok(NODE)]);
    let config = rig.config(Some(addr));
    let local = std::sync::Arc::new(SocketLocal::new(
        &rig.dir.join("none.sock"),
        Limits::default(),
    ));
    let (_, health) = node_ports(&config, &rig.identity(), Role::Daemon, &local).unwrap();
    let target = Version::parse("0.1.5+gaaaaaaaaaaaa-2").unwrap();
    // The helper is not there, so the commit cannot be read: the check fails there, after it
    // asked the frontend for the node, and not before.
    let result = health.check_held(&host(Role::Daemon), &target);
    assert!(result.is_err(), "{result:?}");
    let seen = server.join().unwrap();
    assert_eq!(head_of(&seen[0]).0[0], "GET /v1/nodes/n1 HTTP/1.1");
}

/// What the agent builds from its flags: the token, if it was given, is the way to the frontend
/// (the opt-in fallback); without it the certificate and `--frontend` are; with neither the
/// agent refuses.
#[test]
fn the_ports_follow_the_flags_the_agent_was_given() {
    let rig = Rig::new("fe-ports");
    let local = std::sync::Arc::new(SocketLocal::new(
        &rig.dir.join("none.sock"),
        Limits::default(),
    ));
    let h = host(Role::Daemon);

    let none = rig.config(None);
    let (drain, health) = node_ports(&none, &rig.identity(), Role::Daemon, &local).unwrap();
    for text in [
        drain
            .drain(&h, Duration::from_secs(1))
            .unwrap_err()
            .to_string(),
        health.refusal(&h).unwrap(),
    ] {
        assert_eq!(text, NO_CREDENTIAL);
    }

    let (addr, server) = rig.frontend(vec![ok("{}")]);
    let with_frontend = rig.config(Some(addr));
    let (drain, _) = node_ports(&with_frontend, &rig.identity(), Role::Daemon, &local).unwrap();
    assert_eq!(drain.drain(&h, Duration::from_secs(1)), Ok(()));
    assert_eq!(server.join().unwrap().len(), 1);

    // A storage host is built the same way: its drain and its health are calls to the frontend
    // too, and it refuses nothing for want of the token.
    let (addr, server) = rig.frontend(vec![http("404 Not Found", "", "{}"), ok(OWN)]);
    let with_frontend = rig.config(Some(addr));
    let (storage_drain, storage_health) =
        node_ports(&with_frontend, &rig.identity(), Role::Storage, &local).unwrap();
    let st = host(Role::Storage);
    assert_eq!(storage_health.refusal(&st), None);
    assert_eq!(storage_drain.drain(&st, Duration::from_secs(1)), Ok(()));
    let target = Version::parse("0.1.5+gaaaaaaaaaaaa-2").unwrap();
    assert_eq!(
        storage_health.check_held(&st, &target),
        Ok(crate::ports::Health::Healthy)
    );
    let seen = server.join().unwrap();
    assert_eq!(head_of(&seen[0]).0[0], "POST /v1/nodes/n1:drain HTTP/1.1");
    assert_eq!(
        head_of(&seen[1]).0[0],
        "GET /v1/nodes/n1/storage-health HTTP/1.1"
    );

    // The token, given, wins: the drain goes to the loopback address and the frontend is never
    // dialled (a connection of the test's own is the only one it sees).
    let (alive, watcher) = rig.frontend(vec![ok("{}")]);
    let token = rig.dir.join("token");
    std::fs::write(&token, "t".repeat(40)).unwrap();
    std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o600)).unwrap();
    let mut with_token = rig.config(Some(alive));
    with_token.node_api = Some(NodeApi {
        addr: "127.0.0.1:1".parse().unwrap(),
        token_file: token,
    });
    let (drain, _) = node_ports(&with_token, &rig.identity(), Role::Daemon, &local).unwrap();
    let err = drain.drain(&h, Duration::from_secs(1)).unwrap_err();
    assert_eq!(err.kind(), PortErrorKind::Unreachable);
    assert!(err.to_string().contains("the operator API"), "{err}");
    drop(std::net::TcpStream::connect(alive));
    let seen = watcher.join().unwrap();
    assert!(
        seen[0].request.is_empty() && seen[0].client.is_none(),
        "{seen:?}"
    );
}

/// What answers must be the frontend: a coordinator's certificate for the right name, one for
/// another name and one from another CA are refused, and nothing is sent to any of them (no
/// request line, so the token-free credential of the host is never shown to a stranger).
#[test]
fn only_the_frontend_is_talked_to() {
    let rig = Rig::new("fe-who");
    let h = host(Role::Daemon);
    let strangers = Pki::new();
    let cases = [
        serve(
            server_config(&rig.pki, &rig.pki.coordinator()),
            vec![ok("{}")],
        ),
        serve(
            server_config(&rig.pki, &rig.pki.storage_node("localhost")),
            vec![ok("{}")],
        ),
        serve(
            server_config(&strangers, &strangers.frontend()),
            vec![ok("{}")],
        ),
    ];
    for (addr, server) in cases {
        let drain = super::build::drain_for(Role::Daemon, &rig.api(addr));
        let err = drain.drain(&h, Duration::from_secs(1)).unwrap_err();
        assert_eq!(err.kind(), PortErrorKind::Other, "{err}");
        assert!(err.to_string().contains("is not the frontend"), "{err}");
        assert!(server.join().unwrap()[0].request.is_empty());
    }
}

/// An agent whose files were swapped for another node's certificate does not present it (the
/// node it drains is the one it started as), whose files cannot be used say so without a path,
/// and a frontend that cannot be reached is unreachable.
#[test]
fn a_swapped_pair_a_bad_file_and_a_dead_frontend_are_refused() {
    let rig = Rig::new("fe-files");
    let h = host(Role::Daemon);
    let (addr, server) = rig.frontend(vec![ok("{}")]);
    let api = rig.api(addr);
    let other = rig.pki.updater("n2", HostRole::Daemon);
    write_files(&rig.dir, &rig.pki, &other);
    let err = super::build::drain_for(Role::Daemon, &api)
        .drain(&h, Duration::from_secs(1))
        .unwrap_err();
    assert_eq!(err.kind(), PortErrorKind::Other);
    assert!(
        err.to_string().contains("TLS files cannot be used"),
        "{err}"
    );
    assert!(
        !err.to_string().contains(&rig.dir.display().to_string()),
        "{err}"
    );
    drop(api);
    // Nobody connected: the server is still waiting; give it a connection to end it.
    drop(std::net::TcpStream::connect(addr));
    let _ = server.join();

    // A pair that matches but is another role's is no updater's either.
    let storage = rig.pki.storage_node("n1");
    std::fs::write(&rig.files.cert, storage.cert_pem()).unwrap();
    std::fs::write(&rig.files.key, storage.key_pem()).unwrap();
    let err = super::build::drain_for(Role::Daemon, &api_for_dead(&rig))
        .drain(&h, Duration::from_secs(1))
        .unwrap_err();
    assert!(
        err.to_string().contains("TLS files cannot be used"),
        "{err}"
    );
    write_files(&rig.dir, &rig.pki, &rig.updater);
    let dead = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap()
    };
    let err = super::build::drain_for(Role::Daemon, &rig.api(dead))
        .drain(&h, Duration::from_secs(1))
        .unwrap_err();
    assert_eq!(err.kind(), PortErrorKind::Unreachable);
}

/// A client for a frontend that is never dialled: the files are judged first.
fn api_for_dead(rig: &Rig) -> OperatorApi {
    rig.api("127.0.0.1:1".parse().unwrap())
}

/// A reply the proxy sends in chunks is read whole, and a refusal of the frontend (a 403 for a
/// call that is not the node's own) is that refusal and not a success.
#[test]
fn a_chunked_reply_is_read_and_a_refusal_is_a_refusal() {
    let rig = Rig::new("fe-reply");
    let chunked = Plan::Say(
        format!(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n\
             5\r\n{}\r\n{:x}\r\n{}\r\n0\r\n\r\n",
            &NODE[..5],
            NODE.len() - 5,
            &NODE[5..]
        )
        .into_bytes(),
    );
    let (addr, server) = rig.frontend(vec![chunked, http("403 Forbidden", "", "{}")]);
    let drain = super::build::drain_for(Role::Daemon, &rig.api(addr));
    let h = host(Role::Daemon);
    assert_eq!(drain.leases(&h), Ok(2));
    let err = drain.drain(&h, Duration::from_secs(1)).unwrap_err();
    assert!(err.to_string().contains("refused the call (403)"), "{err}");
    server.join().unwrap();
}
