//! The binary's `run`, in this process: serving, and failing to start. (A child killed to
//! stop it writes no coverage data, so the start path is also walked here.)

mod support;

use std::process::ExitCode;
use std::time::{Duration, Instant};

use kbf_ca::{Algorithm, Authority, Policy, Signer};
use kbf_signer::{Flags, Route, SocketSigner, run};
use support::{euid, real_now, scratch};
use x509_parser::prelude::{FromDer, X509Certificate};

fn flags(scratch: &std::path::Path, socket: &std::path::Path) -> Flags {
    Flags {
        ca_dir: scratch.join("ca"),
        socket: socket.to_path_buf(),
        frontend_uid: euid(),
        client_group: None,
        lifetime_secs: Policy::DEFAULT_LIFETIME_S,
    }
}

// Catches: a start path that only works under a subprocess: `run` on a fresh directory
// makes the CA, binds, and serves a client that gets the CA's key and a signature valid
// under it; a second `run` on the same socket (the first still serving, holding the lock)
// fails with a message and a failure exit, touching nothing.
#[test]
fn run_serves_a_fresh_ca_and_a_second_run_on_its_socket_fails() {
    let scratch = scratch("run_in_process");
    let socket = scratch.join("run").join("sock");
    let served = flags(&scratch, &socket);
    std::thread::spawn({
        let served = served.clone();
        move || run(&served)
    });
    let deadline = Instant::now() + Duration::from_secs(60);
    let client = loop {
        match SocketSigner::connect(&socket, Route::Node) {
            Ok(client) => break client,
            Err(e) => assert!(Instant::now() < deadline, "never came up: {e}"),
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let ca = Authority::load_dir(&served.ca_dir, Policy::default()).expect("the CA it made");
    let root = ca.root_der().to_vec();
    let (_, cert) = X509Certificate::from_der(&root).expect("root");
    assert_eq!(client.public_key(), cert.public_key().raw);
    let made = support::Ca {
        dir: served.ca_dir.clone(),
        root: root.clone(),
        spki: client.public_key().to_vec(),
        root_subject: cert.subject().as_raw().to_vec(),
        scratch,
    };
    let tbs = support::node_tbs_at(&made, kbf_types::Role::Daemon, None, real_now());
    let signature = client
        .sign(&tbs, Algorithm::EcdsaP256Sha256)
        .expect("signed by the clock it reads");
    assert!(!signature.is_empty());

    // The same socket again: the lock is held by the first.
    assert_eq!(run(&served), ExitCode::FAILURE);
}

// Catches: a `--client-group` start path wired only under a subprocess: `run` with a group
// of the account serves, hands the socket's directory and the socket to the group at
// 0750 and 0660, and a second `run` on the same socket fails.
#[test]
fn run_with_a_client_group_serves_and_shares_the_socket() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let (name, gid) = support::a_named_group_of_mine();
    let scratch = scratch("run_group_in_process");
    let socket = scratch.join("run").join("sock");
    let served = Flags {
        client_group: Some(name),
        ..flags(&scratch, &socket)
    };
    std::thread::spawn({
        let served = served.clone();
        move || run(&served)
    });
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        match SocketSigner::connect(&socket, Route::Node) {
            Ok(_) => break,
            Err(e) => assert!(Instant::now() < deadline, "never came up: {e}"),
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let meta = |p: &std::path::Path| {
        let m = std::fs::metadata(p).expect("metadata");
        (m.permissions().mode() & 0o7777, m.gid())
    };
    assert_eq!(meta(socket.parent().expect("dir")), (0o750, gid));
    assert_eq!(meta(&socket), (0o660, gid));
    assert_eq!(meta(&served.ca_dir.join("ca.pem")), (0o640, gid));
    assert_eq!(run(&served), ExitCode::FAILURE);
}
