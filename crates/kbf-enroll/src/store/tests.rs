//! The files: modes, one-step replacement, and what a crash at each point of an install
//! leaves behind.

use std::os::unix::fs::{PermissionsExt, symlink};

use rcgen::{CertificateParams, PublicKeyData};

use super::*;
use crate::testutil::scratch;

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

fn mkdir(path: &Path, mode: u32) {
    std::fs::create_dir_all(path).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

fn store(name: &str) -> (Store, PathBuf) {
    let root = scratch(name);
    (Store::open(&root.join("state")).unwrap(), root)
}

fn both() -> RoleSet {
    RoleSet::new([Role::Storage, Role::Daemon]).unwrap()
}

/// A self-signed stand-in for an issued certificate, for the key at `key_path`.
fn cert_for(store: &Store, role: Role, from_staging: bool, serial: &str) -> Cert {
    let name = format!("{}.key", role.name());
    let path = if from_staging {
        store.staging().join(&name)
    } else {
        store.key_path(role)
    };
    let key = KeyPair::from_pem(&std::fs::read_to_string(path).unwrap()).unwrap();
    let cert = CertificateParams::new(vec!["node-02".to_owned()])
        .unwrap()
        .self_signed(&key)
        .unwrap();
    Cert {
        role,
        pem: cert.pem(),
        serial: serial.to_owned(),
        not_before: 1,
        not_after: 100,
    }
}

fn node(certs: &[Cert], generation: u64) -> Node {
    Node::new(
        &"node-02".parse().unwrap(),
        &CaPin::from_bytes([3; 32]),
        certs,
        50,
        StoredConfig {
            generation,
            ..StoredConfig::default()
        },
    )
}

/// Whether the installed key and certificate of `role` are a pair.
fn pair_matches(store: &Store, role: Role) -> bool {
    let key = store.key_pair(role).unwrap();
    let pem = std::fs::read(store.cert_path(role)).unwrap();
    let der = rustls::pki_types::CertificateDer::from_pem_slice(&pem).unwrap();
    let (_, cert) = x509_parser::parse_x509_certificate(&der).unwrap();
    cert.public_key().raw == key.subject_public_key_info().as_slice()
}

use rustls::pki_types::pem::PemObject;

/// An enrolled store: keys and certificates for both roles installed.
fn enrolled(name: &str) -> (Store, PathBuf, Node) {
    let (store, root) = store(name);
    store.pending(both()).unwrap();
    let certs = [
        cert_for(&store, Role::Storage, true, "a1"),
        cert_for(&store, Role::Daemon, true, "a2"),
    ];
    let node = node(&certs, 1);
    store.stage(&certs, b"CA ONE", &node).unwrap();
    store.commit().unwrap();
    (store, root, node)
}

#[test]
fn open_makes_private_directories_and_refuses_ones_that_are_not() {
    let root = scratch("store-open");
    let state = root.join("a/b/state");
    let store = Store::open(&state).unwrap();
    assert_eq!(mode(&state), 0o700);
    assert_eq!(mode(&state.join("pki")), 0o700);
    assert_eq!(store.load().unwrap(), None);
    Store::open(&state).expect("opening again is fine");

    mkdir(&state, 0o755);
    assert!(matches!(Store::open(&state), Err(StoreError::Insecure(_))));
    mkdir(&state, 0o700);
    mkdir(&state.join("pki"), 0o750);
    let err = Store::open(&state).unwrap_err();
    assert!(err.to_string().contains("mode 0700"), "{err}");

    // A directory somebody else owns (the root of the file system is not the service user's).
    assert!(matches!(
        Store::open(Path::new("/")),
        Err(StoreError::Insecure(_))
    ));

    // A file where the directory should be, and a link to a good directory.
    let file = root.join("file");
    std::fs::write(&file, b"x").unwrap();
    assert!(Store::open(&file).is_err());
    mkdir(&root.join("real"), 0o700);
    symlink(root.join("real"), root.join("link")).unwrap();
    assert!(matches!(
        Store::open(&root.join("link")),
        Err(StoreError::Insecure(_))
    ));
}

#[test]
fn pending_makes_a_key_and_request_per_role_and_a_retry_gets_the_same() {
    let (store, _) = store("store-pending");
    let first = store.pending(both()).unwrap();
    assert_eq!(
        first.iter().map(|p| p.role).collect::<Vec<_>>(),
        vec![Role::Storage, Role::Daemon]
    );
    assert_ne!(first[0].spki, first[1].spki, "one key per role");
    for role in Role::ALL {
        let staged = store.staging().join(format!("{}.key", role.name()));
        assert_eq!(mode(&staged), 0o600);
        assert_eq!(
            mode(&store.staging().join(format!("{}.csr", role.name()))),
            0o600
        );
    }
    assert_eq!(mode(&store.staging()), 0o700);
    let again = store.pending(both()).unwrap();
    assert_eq!(first, again, "the same bytes");
    // The key behind each request is the one in the file.
    let key =
        KeyPair::from_pem(&std::fs::read_to_string(store.staging().join("daemon.key")).unwrap())
            .unwrap();
    assert_eq!(csr::spki_of(&key), first[1].spki);
    let (_, parsed) = x509_parser::prelude::FromDer::from_der(&first[1].csr_der)
        .map(
            |(r, c): (
                &[u8],
                x509_parser::certification_request::X509CertificationRequest,
            )| (r, c),
        )
        .unwrap();
    parsed.verify_signature().unwrap();
    // A subset asks for fewer.
    let one = store
        .pending(RoleSet::new([Role::Daemon]).unwrap())
        .unwrap();
    assert_eq!(one, vec![first[1].clone()]);
}

#[test]
fn a_key_without_its_request_is_replaced_not_reused() {
    // A crash between the two writes leaves a request with no key (the request is written
    // first). A key with no request cannot happen; if it did, both are made again.
    let (store, _) = store("store-pending-half");
    let first = store.pending(both()).unwrap();
    std::fs::remove_file(store.staging().join("storage.csr")).unwrap();
    let second = store.pending(both()).unwrap();
    assert_ne!(first[0].spki, second[0].spki);
    assert_eq!(first[1], second[1]);
    std::fs::remove_file(store.staging().join("daemon.key")).unwrap();
    assert_ne!(first[1].spki, store.pending(both()).unwrap()[1].spki);
}

#[test]
fn a_damaged_staged_key_is_reported() {
    let (store, _) = store("store-pending-damaged");
    store.pending(both()).unwrap();
    std::fs::write(store.staging().join("storage.key"), b"not a key").unwrap();
    let err = store.pending(both()).unwrap_err();
    assert!(
        matches!(&err, StoreError::Damaged(p) if p.ends_with("storage.key")),
        "{err:?}"
    );
    std::fs::write(store.staging().join("storage.key"), [0xff, 0xfe]).unwrap();
    assert!(matches!(store.pending(both()), Err(StoreError::Damaged(_))));
}

#[test]
fn a_first_install_puts_every_file_in_place_with_its_mode_and_clears_staging() {
    let (store, _, node) = enrolled("store-install");
    for role in Role::ALL {
        assert_eq!(mode(&store.key_path(role)), 0o600);
        assert_eq!(mode(&store.cert_path(role)), 0o644);
        assert!(pair_matches(&store, role));
    }
    assert_eq!(mode(&store.ca_path()), 0o644);
    assert_eq!(std::fs::read(store.ca_path()).unwrap(), b"CA ONE");
    assert_eq!(mode(&store.pki.join("node.json")), 0o600);
    assert!(!store.staging().exists());
    assert_eq!(store.load().unwrap(), Some(node.clone()));
    assert_eq!(store.recover().unwrap(), Some(node));
}

#[test]
fn a_replaced_file_never_keeps_a_wider_mode_than_asked() {
    // A temp file left by a crash, with a wide mode, must not leak into the new file.
    let root = scratch("store-temp-mode");
    let leftover = root.join(".k.tmp");
    std::fs::write(&leftover, b"old").unwrap();
    std::fs::set_permissions(&leftover, std::fs::Permissions::from_mode(0o666)).unwrap();
    write_atomic(&root, "k", b"secret", 0o600).unwrap();
    assert_eq!(mode(&root.join("k")), 0o600);
    assert_eq!(std::fs::read(root.join("k")).unwrap(), b"secret");
    assert!(!leftover.exists(), "the temp file was renamed away");
    // And a directory that is not there is an error naming the file.
    let err = write_atomic(&root.join("absent"), "k", b"x", 0o600).unwrap_err();
    assert!(err.to_string().contains(".k.tmp"), "{err}");
}

#[test]
fn a_crash_at_any_point_of_a_renewal_leaves_a_certificate_that_matches_its_key() {
    // Renewal keeps the key, so only certificates and the bundle move. After each rename
    // the pair on disk is a pair; recovery then finishes the install.
    let steps = 1 + 2 + 1; // bundle, two certificates, node.json (keys are not staged)
    for crash_after in 0..=steps {
        let (store, _, old) = enrolled(&format!("store-crash-renew-{crash_after}"));
        let certs = [
            cert_for(&store, Role::Storage, false, "b1"),
            cert_for(&store, Role::Daemon, false, "b2"),
        ];
        let new = node(&certs, 2);
        store.stage(&certs, b"CA TWO", &new).unwrap();
        store.commit_steps(crash_after).unwrap();
        for role in Role::ALL {
            assert!(
                pair_matches(&store, role),
                "step {crash_after}: {role} pair"
            );
        }
        // Until node.json moves, the old record is what a restart reads.
        let seen = store.load().unwrap();
        assert_eq!(
            seen.as_ref(),
            Some(if crash_after < steps { &old } else { &new }),
            "step {crash_after}"
        );
        // A restart finishes it.
        assert_eq!(store.recover().unwrap(), Some(new), "step {crash_after}");
        assert!(!store.staging().exists());
        assert_eq!(std::fs::read(store.ca_path()).unwrap(), b"CA TWO");
        for role in Role::ALL {
            assert!(pair_matches(&store, role));
        }
    }
}

#[test]
fn a_crash_during_a_first_install_is_finished_by_the_next_start() {
    let steps = 1 + 2 * 2 + 1; // bundle, key and certificate per role, node.json
    for crash_after in 0..steps {
        let (store, _) = store(&format!("store-crash-first-{crash_after}"));
        store.pending(both()).unwrap();
        let certs = [
            cert_for(&store, Role::Storage, true, "c1"),
            cert_for(&store, Role::Daemon, true, "c2"),
        ];
        let node = node(&certs, 1);
        store.stage(&certs, b"CA", &node).unwrap();
        store.commit_steps(crash_after).unwrap();
        assert_eq!(
            store.load().unwrap(),
            None,
            "not enrolled until node.json moves"
        );
        assert_eq!(store.recover().unwrap(), Some(node), "step {crash_after}");
        for role in Role::ALL {
            assert!(pair_matches(&store, role), "step {crash_after}: {role}");
            assert_eq!(mode(&store.key_path(role)), 0o600);
        }
        assert!(!store.staging().exists());
    }
}

#[test]
fn a_new_key_over_an_old_one_is_whole_again_after_recovery() {
    // A re-enrollment (`replace`) changes the key. The two renames of a role cannot be
    // one step, so between them the key and certificate differ; a restart repairs it.
    let (store, _, _) = enrolled("store-crash-replace");
    mkdir(&store.staging(), 0o700);
    let key = csr::new_key().unwrap();
    write_atomic(
        &store.staging(),
        "storage.key",
        key.serialize_pem().as_bytes(),
        0o600,
    )
    .unwrap();
    let cert = Cert {
        role: Role::Storage,
        pem: CertificateParams::new(vec!["node-02".to_owned()])
            .unwrap()
            .self_signed(&key)
            .unwrap()
            .pem(),
        serial: "d1".to_owned(),
        not_before: 1,
        not_after: 100,
    };
    let new = node(std::slice::from_ref(&cert), 3);
    store
        .stage(std::slice::from_ref(&cert), b"CA", &new)
        .unwrap();
    store.commit_steps(2).unwrap(); // the bundle and the new key
    assert!(!pair_matches(&store, Role::Storage), "the window exists");
    assert_eq!(store.recover().unwrap(), Some(new));
    assert!(pair_matches(&store, Role::Storage));
}

#[test]
fn damaged_records_are_reported_with_the_file() {
    let (store, _, _) = enrolled("store-damaged");
    let json = store.pki.join("node.json");
    std::fs::write(&json, b"{").unwrap();
    assert!(matches!(store.load(), Err(StoreError::Damaged(p)) if p.ends_with("node.json")));

    // Nothing staged to commit.
    assert!(matches!(store.commit(), Err(StoreError::Damaged(_))));
    // A staged record that does not parse.
    mkdir(&store.staging(), 0o700);
    std::fs::write(store.staging().join("node.json"), b"nope").unwrap();
    assert!(matches!(store.commit(), Err(StoreError::Damaged(_))));
    assert!(store.recover().is_err());

    // A path that cannot be read as a file (here a directory) is an I/O error.
    std::fs::remove_file(&json).unwrap();
    std::fs::create_dir(&json).unwrap();
    assert!(matches!(store.load(), Err(StoreError::Io { .. })));

    // Keys: missing, and not a key.
    assert!(store.key_pair(Role::Storage).is_ok());
    std::fs::write(store.key_path(Role::Storage), b"junk").unwrap();
    assert!(matches!(
        store.key_pair(Role::Storage),
        Err(StoreError::Damaged(_))
    ));
    std::fs::remove_file(store.key_path(Role::Storage)).unwrap();
    assert!(matches!(
        store.key_pair(Role::Storage),
        Err(StoreError::Io { .. })
    ));
}

#[test]
fn a_commit_that_cannot_move_a_file_says_which() {
    let (store, _) = store("store-rename-fail");
    store.pending(both()).unwrap();
    let certs = [cert_for(&store, Role::Storage, true, "e1")];
    let n = node(&certs, 1);
    store.stage(&certs, b"CA", &n).unwrap();
    // A directory in the way of the bundle's destination.
    std::fs::create_dir(store.pki.join("ca.pem")).unwrap();
    std::fs::write(store.pki.join("ca.pem").join("x"), b"x").unwrap();
    let err = store.commit().unwrap_err();
    assert!(
        matches!(&err, StoreError::Io { path, .. } if path.ends_with("ca.pem")),
        "{err:?}"
    );
}

#[test]
fn node_records_know_their_roles_pin_and_expiry() {
    let certs = [
        Cert {
            role: Role::Daemon,
            pem: String::new(),
            serial: "2".into(),
            not_before: 1,
            not_after: 90,
        },
        Cert {
            role: Role::Storage,
            pem: String::new(),
            serial: "1".into(),
            not_before: 1,
            not_after: 80,
        },
    ];
    let n = node(&certs, 1);
    assert_eq!(
        n.roles(),
        vec![Role::Storage, Role::Daemon],
        "in role order, not answer order"
    );
    assert_eq!(n.earliest_not_after(), 80);
    assert_eq!(n.pin().unwrap(), CaPin::from_bytes([3; 32]));
    assert_eq!(node(&[], 1).earliest_not_after(), 0);
    for bad in ["", "zz", &"ab".repeat(31), &"ab".repeat(33)] {
        let mut broken = n.clone();
        broken.pin = bad.to_owned();
        assert!(
            matches!(broken.pin(), Err(StoreError::Damaged(_))),
            "{bad:?}"
        );
    }
}

#[test]
fn the_stored_configuration_keeps_what_the_frontend_sent() {
    let proto = NodeConfig {
        generation: 7,
        roles: vec!["storage".into()],
        worker_endpoint: "https://w:1".into(),
        cas_endpoint: "https://c:2".into(),
        storage_port: 3,
        cold_store: "s3://b/p".into(),
        credential_mode: CredentialMode::Keys as i32,
        min_protocol: 4,
        tls_reload_interval_s: 5,
        frontend_url: "https://new".into(),
    };
    let stored = StoredConfig::from_proto(&proto);
    assert_eq!(
        (stored.generation, stored.storage_port, stored.min_protocol),
        (7, 3, 4)
    );
    assert_eq!(stored.worker_endpoint, "https://w:1");
    assert_eq!(stored.cold_store, "s3://b/p");
    assert_eq!(stored.credential_mode(), Some(CredentialMode::Keys));
    let json = serde_json::to_string(&stored).unwrap();
    assert_eq!(serde_json::from_str::<StoredConfig>(&json).unwrap(), stored);
    assert!(!json.contains("secret"), "no key material in a config");
    let future = StoredConfig {
        credential_mode: 99,
        ..stored
    };
    assert_eq!(
        future.credential_mode(),
        None,
        "an unknown mode is kept, not guessed"
    );
}
