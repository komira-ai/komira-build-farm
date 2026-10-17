//! The object-store keys of a storage node: fetched over its mutual-TLS session and held
//! in memory only.

mod support;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use kbf_enroll::client::{CallError, Enrollment, connect, enroll};
use kbf_enroll::keys::{KeysError, fetch};
use kbf_enroll::pin::Anchor;
use kbf_enroll::store::Store;
use kbf_types::{Role, RoleSet};
use support::fake::{self, Running};
use support::{config_for, facts, scratch};

async fn enrolled(name: &str, roles: &[Role]) -> (Running, Store, PathBuf) {
    let running = fake::start();
    let root = scratch(name);
    let config = config_for(&root, running.enroll_addr.port(), "");
    let token = running.fake.mint(0x3333, 1);
    std::fs::write(&config.token_file, token.to_line()).expect("token file");
    std::fs::set_permissions(&config.token_file, std::fs::Permissions::from_mode(0o600))
        .expect("mode");
    let store = Store::open(&config.state_dir).expect("store");
    let frontend = connect(&config.frontend, Anchor::Pin(*token.pin()))
        .await
        .expect("connect");
    enroll(
        &frontend,
        &store,
        &Enrollment {
            config: &config,
            token: &token,
            roles: RoleSet::new(roles.iter().copied()).expect("roles"),
            facts: facts(),
            now: running.fake.now(),
        },
    )
    .await
    .expect("enrolled");
    (running, store, root)
}

fn worker(running: &Running) -> String {
    format!("https://localhost:{}", running.worker_addr.port())
}

fn files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("dir") {
        let path = entry.expect("entry").path();
        if path.is_dir() {
            files(&path, out);
        } else {
            out.push(path);
        }
    }
}

#[tokio::test]
async fn a_storage_node_gets_the_keys_over_its_certificate_and_they_reach_no_file() {
    let (running, store, root) = enrolled("keys", &[Role::Storage, Role::Daemon]).await;
    let keys = fetch(&store, &worker(&running)).await.expect("keys");
    assert_eq!(keys.access_key_id(), "AKIATESTKEY0000");
    assert_eq!(keys.secret_access_key(), "secret-that-must-stay-in-memory");
    assert_eq!(running.fake.keys_calls(), 1);

    // Catches: a Debug or log line that prints the secret.
    assert_eq!(format!("{keys:?}"), "Keys(..)");

    // Catches: the keys landing in a file (a cache, a state file, a temp file).
    let mut all = Vec::new();
    files(&root, &mut all);
    assert!(all.len() > 4, "the node's files are there to be searched");
    for path in all {
        let text = String::from_utf8_lossy(&std::fs::read(&path).expect("read")).into_owned();
        assert!(
            !text.contains("secret-that-must-stay-in-memory"),
            "{}",
            path.display()
        );
        assert!(!text.contains("AKIATESTKEY0000"), "{}", path.display());
    }

    // They are fetched again on demand (every start, and when the store refuses them).
    fetch(&store, &worker(&running)).await.expect("again");
    assert_eq!(running.fake.keys_calls(), 2);
}

#[tokio::test]
async fn a_revoked_node_is_refused_the_keys() {
    let (running, store, _) = enrolled("keys-revoked", &[Role::Storage]).await;
    running.fake.revoke_keys();
    let err = fetch(&store, &worker(&running)).await.unwrap_err();
    assert!(
        matches!(&err, KeysError::Call(CallError::Status(s)) if s.code() == tonic::Code::PermissionDenied),
        "{err:?}"
    );
}

#[tokio::test]
async fn a_node_without_a_storage_identity_has_nothing_to_ask_with() {
    let (running, store, _) = enrolled("keys-daemon-only", &[Role::Daemon]).await;
    let err = fetch(&store, &worker(&running)).await.unwrap_err();
    assert!(
        matches!(&err, KeysError::Identity(m) if m.contains("storage.pem")),
        "{err}"
    );
    assert_eq!(running.fake.keys_calls(), 0);
}

#[tokio::test]
async fn an_endpoint_that_is_not_a_url_or_not_listening_is_a_transport_error() {
    let (running, store, _) = enrolled("keys-endpoint", &[Role::Storage]).await;
    let err = fetch(&store, "not a url").await.unwrap_err();
    assert!(matches!(err, KeysError::Transport(_)), "{err}");
    let closed = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        l.local_addr().expect("addr").port()
    };
    let err = fetch(&store, &format!("https://localhost:{closed}"))
        .await
        .unwrap_err();
    assert!(matches!(err, KeysError::Transport(_)), "{err}");
    drop(running);
}
