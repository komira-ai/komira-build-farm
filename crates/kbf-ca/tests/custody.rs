//! Where the CA key lives: the directory and files that hold it, what is refused on load,
//! what is never printed, and the import of a root made elsewhere.

mod support;

use std::fs;
use std::io::ErrorKind;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;
use std::sync::Arc;

use kbf_ca::{
    Algorithm, Authority, Bundle, CaError, Custody, FileSigner, KeyFileFault, NodeCert, Policy,
    Signer,
};
use kbf_types::Role;
use rcgen::{KeyPair, PKCS_ECDSA_P384_SHA384};
use ring::signature::{ECDSA_P256_SHA256_ASN1, UnparsedPublicKey};
use support::{External, NOW, Scratch, Stream, public_of, scratch, view};

fn mode(path: &Path) -> u32 {
    fs::metadata(path).expect("metadata").permissions().mode() & 0o777
}

fn chmod(path: &Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("chmod");
}

fn euid() -> u32 {
    rustix::process::geteuid().as_raw()
}

/// The scratch directory (keep it bound: it is removed on drop), the CA's directory in it,
/// and the CA.
fn init(name: &str) -> (Scratch, std::path::PathBuf, Authority) {
    let root = scratch(name);
    let dir = root.join("ca");
    let ca = Authority::init_dir(&dir, Policy::default(), NOW, &mut Stream(4)).expect("init");
    (root, dir, ca)
}

fn key_fault(result: Result<impl Sized, CaError>) -> KeyFileFault {
    match result.err().expect("an error") {
        CaError::KeyFile(fault) => fault,
        other => panic!("not a key file fault: {other:?}"),
    }
}

// Catches: a key or directory made readable by others (even for a moment), a certificate
// that is not world readable, and a reload that does not give back the same CA.
#[test]
fn init_makes_a_closed_directory_a_closed_key_and_an_open_certificate() {
    let (_root, dir, ca) = init("init");
    assert_eq!(mode(&dir), 0o700);
    assert_eq!(mode(&dir.join("ca.key")), 0o600);
    assert_eq!(mode(&dir.join("ca.pem")), 0o644);
    assert_eq!(FileSigner::KEY_FILE, "ca.key");
    assert_eq!(Authority::CERT_FILE, "ca.pem");

    let pem = fs::read(dir.join("ca.pem")).expect("pem");
    assert_eq!(
        Bundle::from_pem(&pem).expect("pem").certs(),
        [ca.root_der().to_vec()]
    );

    let loaded = Authority::load_dir(&dir, Policy::default()).expect("load");
    assert_eq!(loaded.root_der(), ca.root_der());
    assert_eq!(loaded.pin(), ca.pin());
    // The reloaded CA signs for the same key: its certificates chain to the one root.
    let key = public_of(&KeyPair::generate().expect("key"));
    let issued = loaded
        .issue_node(
            &NodeCert {
                node_id: &"n".parse().expect("id"),
                role: Role::Daemon,
                key: &key,
                advertise: None,
            },
            NOW,
            &mut Stream(5),
        )
        .expect("issued");
    let (_, leaf) = x509_parser::parse_x509_certificate(&issued.der).expect("leaf");
    let (_, root) = x509_parser::parse_x509_certificate(ca.root_der()).expect("root");
    assert!(leaf.verify_signature(Some(root.public_key())).is_ok());
}

// Catches: an init that overwrites a CA that exists (the key would be lost, and every node
// with it), and one that writes into an open directory.
#[test]
fn init_overwrites_nothing_and_refuses_an_open_directory() {
    let (_root, dir, ca) = init("twice");
    let before = fs::read(dir.join("ca.key")).expect("key");
    let again = Authority::init_dir(&dir, Policy::default(), NOW, &mut Stream(4));
    assert_eq!(key_fault(again), KeyFileFault::Io(ErrorKind::AlreadyExists));
    assert_eq!(fs::read(dir.join("ca.key")).expect("key"), before);
    assert_eq!(
        Authority::load_dir(&dir, Policy::default())
            .expect("load")
            .pin(),
        ca.pin()
    );

    let open_root = scratch("open");
    let open = open_root.join("ca");
    fs::create_dir(&open).expect("dir");
    chmod(&open, 0o755);
    let result = Authority::init_dir(&open, Policy::default(), NOW, &mut Stream(4));
    assert_eq!(key_fault(result), KeyFileFault::DirMode);
    assert!(
        !open.join("ca.key").exists(),
        "nothing is written into an open directory"
    );

    let closed_root = scratch("closed");
    let closed = closed_root.join("ca");
    fs::create_dir(&closed).expect("dir");
    chmod(&closed, 0o700);
    assert!(Authority::init_dir(&closed, Policy::default(), NOW, &mut Stream(4)).is_ok());

    let orphan_root = scratch("orphan");
    let missing_parent = orphan_root.join("nowhere").join("ca");
    let result = Authority::init_dir(&missing_parent, Policy::default(), NOW, &mut Stream(4));
    assert_eq!(key_fault(result), KeyFileFault::Io(ErrorKind::NotFound));
}

// Catches: a create_in that does not leave a loadable key: the key it returns is the key it
// stored, in a closed file in a closed directory.
#[test]
fn create_in_stores_the_key_it_returns() {
    let root = scratch("create-in");
    let dir = root.join("ca");
    let signer = FileSigner::create_in(&dir).expect("created");
    assert_eq!(mode(&dir), 0o700);
    assert_eq!(mode(&dir.join("ca.key")), 0o600);
    let loaded = FileSigner::load(&dir).expect("loaded");
    assert_eq!(loaded.public_key(), signer.public_key());
    assert_eq!(loaded.to_pem(), signer.to_pem());
}

// Catches: following a symbolic link planted where the key goes: the write would land in
// the link's target (a file the attacker chose), and the key would be readable there.
#[test]
fn creating_the_key_never_follows_a_link() {
    let base = scratch("plant");
    let dir = base.join("ca");
    fs::create_dir(&dir).expect("dir");
    chmod(&dir, 0o700);
    let victim = base.join("victim");
    fs::write(&victim, b"keep").expect("victim");
    symlink(&victim, dir.join("ca.key")).expect("link");
    let result = FileSigner::create_in(&dir);
    assert_eq!(
        key_fault(result),
        KeyFileFault::Io(ErrorKind::AlreadyExists)
    );
    assert_eq!(fs::read(&victim).expect("victim"), b"keep");

    let dangling = base.join("ca2");
    fs::create_dir(&dangling).expect("dir");
    chmod(&dangling, 0o700);
    symlink(base.join("not-there"), dangling.join("ca.key")).expect("link");
    assert_eq!(
        key_fault(FileSigner::create_in(&dangling)),
        KeyFileFault::Io(ErrorKind::AlreadyExists)
    );
    assert!(!base.join("not-there").exists());
}

// Catches: a load that trusts a key anyone could have read or replaced: a mode that lets
// group or others in (either bit, read or write), another owner, a symbolic link (to the
// file, or in place of the directory), something that is not a regular file, and a
// directory that is not closed.
#[test]
fn a_key_that_is_not_closed_is_not_loaded() {
    let (_root, dir, _) = init("refuse");
    let key = dir.join("ca.key");
    assert!(FileSigner::load(&dir).is_ok());

    for bad in [0o640, 0o604, 0o660, 0o602, 0o644, 0o666, 0o601] {
        chmod(&key, bad);
        assert_eq!(
            key_fault(FileSigner::load(&dir)),
            KeyFileFault::Mode,
            "{bad:o}"
        );
    }
    for fine in [0o400, 0o600, 0o200 | 0o400] {
        chmod(&key, fine);
        assert!(FileSigner::load(&dir).is_ok(), "{fine:o}");
    }
    chmod(&key, 0o600);

    assert_eq!(
        key_fault(FileSigner::load_owned_by(&dir, euid() + 1)),
        KeyFileFault::Owner,
        "another owner"
    );
    assert!(FileSigner::load_owned_by(&dir, euid()).is_ok());

    chmod(&dir, 0o750);
    assert_eq!(key_fault(FileSigner::load(&dir)), KeyFileFault::DirMode);
    chmod(&dir, 0o705);
    assert_eq!(key_fault(FileSigner::load(&dir)), KeyFileFault::DirMode);
    chmod(&dir, 0o700);

    // The key file replaced by a link to a good key elsewhere.
    let elsewhere = dir.parent().expect("parent").join("elsewhere.key");
    fs::rename(&key, &elsewhere).expect("move");
    symlink(&elsewhere, &key).expect("link");
    assert_eq!(key_fault(FileSigner::load(&dir)), KeyFileFault::Symlink);
    fs::remove_file(&key).expect("unlink");

    // A directory where the key goes.
    fs::create_dir(&key).expect("dir");
    assert_eq!(key_fault(FileSigner::load(&dir)), KeyFileFault::NotRegular);
    fs::remove_dir(&key).expect("rmdir");

    // The whole CA directory a link to a good one.
    fs::rename(&elsewhere, &key).expect("back");
    let alias = dir.parent().expect("parent").join("alias");
    symlink(&dir, &alias).expect("link");
    assert_eq!(key_fault(FileSigner::load(&alias)), KeyFileFault::Symlink);

    // A file where the directory goes.
    let file = dir.parent().expect("parent").join("file");
    fs::write(&file, b"x").expect("file");
    assert_eq!(key_fault(FileSigner::load(&file)), KeyFileFault::NotRegular);

    // Nothing there at all.
    let empty = dir.parent().expect("parent").join("empty");
    fs::create_dir(&empty).expect("dir");
    chmod(&empty, 0o700);
    assert_eq!(
        key_fault(FileSigner::load(&empty)),
        KeyFileFault::Io(ErrorKind::NotFound)
    );
    assert_eq!(
        key_fault(FileSigner::load(&dir.join("nope"))),
        KeyFileFault::Io(ErrorKind::NotFound)
    );
}

// Catches: a key file that is not a key being treated as one, and a key of another type
// being taken for the CA's.
#[test]
fn a_file_that_is_not_a_p256_key_is_not_loaded() {
    let (_root, dir, _) = init("contents");
    let key = dir.join("ca.key");
    fs::write(&key, "not a key at all\n").expect("write");
    assert!(matches!(FileSigner::load(&dir), Err(CaError::Build(_))));
    fs::write(&key, [0xff, 0xfe, 0x00]).expect("write");
    assert_eq!(
        key_fault(FileSigner::load(&dir)),
        KeyFileFault::Io(ErrorKind::InvalidData)
    );
    let p384 = KeyPair::generate_for(&PKCS_ECDSA_P384_SHA384).expect("p384");
    fs::write(&key, p384.serialize_pem()).expect("write");
    assert_eq!(FileSigner::load(&dir).err(), Some(CaError::KeyType));
    assert_eq!(
        FileSigner::from_pem(&p384.serialize_pem()).err(),
        Some(CaError::KeyType)
    );
}

// Catches: a CA directory whose certificate is not the key's (a restore from two different
// backups): the CA would hand out certificates its own root cannot verify.
#[test]
fn a_certificate_that_is_not_the_keys_is_not_loaded() {
    let (_root, dir, _) = init("mismatch");
    let (_other_root, other_dir, _) = init("mismatch-other");
    fs::copy(other_dir.join("ca.pem"), dir.join("ca.pem")).expect("copy");
    assert_eq!(
        Authority::load_dir(&dir, Policy::default()).err(),
        Some(CaError::KeyMismatch)
    );

    fs::write(dir.join("ca.pem"), "garbage").expect("write");
    assert_eq!(
        Authority::load_dir(&dir, Policy::default()).err(),
        Some(CaError::Bundle)
    );
    fs::remove_file(dir.join("ca.pem")).expect("rm");
    assert_eq!(
        key_fault(Authority::load_dir(&dir, Policy::default())),
        KeyFileFault::Io(ErrorKind::NotFound)
    );

    // A link in place of the certificate.
    symlink(other_dir.join("ca.pem"), dir.join("ca.pem")).expect("link");
    assert_eq!(
        key_fault(Authority::load_dir(&dir, Policy::default())),
        KeyFileFault::Symlink
    );

    // Another owner is refused through the authority's loader too.
    assert_eq!(
        key_fault(Authority::load_dir_owned_by(
            &other_dir,
            Policy::default(),
            euid() + 1
        )),
        KeyFileFault::Owner
    );
}

// Catches: key bytes in a Debug output (a log line, a panic message, an `{:?}` in a test).
#[test]
fn debug_prints_no_key() {
    let signer = FileSigner::generate().expect("key");
    assert_eq!(format!("{signer:?}"), "FileSigner");
    assert_eq!(format!("{signer:#?}"), "FileSigner");
    let pem = signer.to_pem();
    assert!(pem.starts_with("-----BEGIN PRIVATE KEY-----"));
    let body: String = pem.lines().filter(|l| !l.starts_with("-----")).collect();
    let ca =
        Authority::create(Arc::new(signer), Policy::default(), NOW, &mut Stream(1)).expect("ca");
    for text in [format!("{ca:?}"), format!("{ca:#?}")] {
        assert!(!text.contains(&body[..32]), "{text}");
    }
    let err = CaError::from(KeyFileFault::Mode);
    assert!(!format!("{err:?}").contains(&body[..32]));
}

// Catches: a file signer whose signatures do not verify under the key it reports, that
// forgets its pem, or that claims to keep the key outside the process.
#[test]
fn the_file_signer_signs_for_the_key_it_reports() {
    let signer = FileSigner::generate().expect("key");
    assert_eq!(signer.custody(), Custody::InProcess);
    let public = public_of_spki(signer.public_key());
    let signature = signer
        .sign(b"to be signed", Algorithm::EcdsaP256Sha256)
        .expect("sign");
    let verifier = UnparsedPublicKey::new(&ECDSA_P256_SHA256_ASN1, public.point().as_slice());
    assert!(verifier.verify(b"to be signed", &signature).is_ok());
    assert!(verifier.verify(b"another message", &signature).is_err());

    let again = FileSigner::from_pem(&signer.to_pem()).expect("round trip");
    assert_eq!(again.public_key(), signer.public_key());
}

fn public_of_spki(spki: &[u8]) -> kbf_ca::PublicKey {
    kbf_ca::PublicKey::from_spki(spki).expect("P-256")
}

// ---- import -----------------------------------------------------------------------------

struct NotP256(Vec<u8>);

impl Signer for NotP256 {
    fn public_key(&self) -> &[u8] {
        &self.0
    }

    fn custody(&self) -> Custody {
        Custody::External
    }

    fn sign(&self, _: &[u8], _: Algorithm) -> Result<Vec<u8>, kbf_ca::SignError> {
        Err(kbf_ca::SignError("no".to_owned()))
    }
}

// Catches: an import that takes a certificate it should not: one that does not parse, has
// bytes after it, is not a CA, belongs to another key, or is not self-signed (a
// cross-signed root is a certificate of the bundle, not a root to issue from).
#[test]
fn import_takes_only_a_self_signed_ca_for_the_signers_key() {
    let signer = External::new();
    let ca =
        Authority::create(signer.clone(), Policy::default(), NOW, &mut Stream(1)).expect("root");
    let imported =
        Authority::import(signer.clone(), Policy::default(), ca.root_der()).expect("import");
    assert_eq!(imported.root_der(), ca.root_der());

    let go = |der: &[u8], s: Arc<External>| Authority::import(s, Policy::default(), der).err();
    let mut trailing = ca.root_der().to_vec();
    trailing.push(0);
    assert_eq!(go(&trailing, signer.clone()), Some(CaError::Parse));
    assert_eq!(go(b"garbage", signer.clone()), Some(CaError::Parse));
    assert_eq!(go(&[], signer.clone()), Some(CaError::Parse));
    assert_eq!(
        go(ca.root_der(), External::new()),
        Some(CaError::KeyMismatch)
    );

    let leaf = ca
        .issue_node(
            &NodeCert {
                node_id: &"n".parse().expect("id"),
                role: Role::Daemon,
                key: &public_of(&KeyPair::generate().expect("key")),
                advertise: None,
            },
            NOW,
            &mut Stream(1),
        )
        .expect("leaf");
    assert_eq!(go(&leaf.der, signer.clone()), Some(CaError::NotCa));

    let next = External::new();
    let rotation = ca
        .stage_rotation(next.clone(), NOW, &mut Stream(2))
        .expect("stage");
    assert_eq!(
        go(rotation.cross_signed_der(), next),
        Some(CaError::NotSelfSigned)
    );

    let odd = NotP256(vec![1, 2, 3]);
    assert_eq!(
        Authority::import(Arc::new(odd), Policy::default(), ca.root_der()).err(),
        Some(CaError::KeyType)
    );
    let odd = NotP256(vec![1, 2, 3]);
    assert_eq!(
        Authority::create(Arc::new(odd), Policy::default(), NOW, &mut Stream(1)).err(),
        Some(CaError::KeyType)
    );
    assert_eq!(view(ca.root_der()).ca, Some((true, true, Some(0))));
}

// A directory shared with a group: the group may read and search it, nothing else about the
// key's protection is relaxed, and the plain load still refuses it.
#[test]
fn a_shared_directory_admits_the_group_to_read_and_nothing_more() {
    let (_root, dir, ca) = init("shared");
    let key = dir.join("ca.key");
    let plain_refuses = |dir: &Path| key_fault(FileSigner::load(dir));

    for open in [0o700, 0o750] {
        chmod(&dir, open);
        let loaded = Authority::load_dir_shared(&dir, Policy::default()).expect("shared load");
        assert_eq!(loaded.root_der(), ca.root_der(), "{open:o}");
        assert!(FileSigner::load_in_shared_dir(&dir).is_ok(), "{open:o}");
    }
    assert_eq!(plain_refuses(&dir), KeyFileFault::DirMode);

    for bad in [0o770, 0o760, 0o755, 0o705, 0o701, 0o714] {
        chmod(&dir, bad);
        assert_eq!(
            key_fault(FileSigner::load_in_shared_dir(&dir)),
            KeyFileFault::DirMode,
            "{bad:o}"
        );
        assert_eq!(
            key_fault(Authority::load_dir_shared(&dir, Policy::default())),
            KeyFileFault::DirMode,
            "{bad:o}"
        );
    }

    // The key stays the account's own: a group-readable key is refused in a shared directory.
    chmod(&dir, 0o750);
    for bad in [0o640, 0o660, 0o604] {
        chmod(&key, bad);
        assert_eq!(
            key_fault(FileSigner::load_in_shared_dir(&dir)),
            KeyFileFault::Mode,
            "{bad:o}"
        );
    }
    chmod(&key, 0o600);

    // A link in place of the directory is refused, and so is a directory of another account.
    let link = dir.parent().expect("parent").join("link");
    symlink(&dir, &link).expect("link");
    assert_eq!(
        key_fault(FileSigner::load_in_shared_dir(&link)),
        KeyFileFault::Symlink
    );
    let file = dir.parent().expect("parent").join("file");
    fs::write(&file, b"x").expect("file");
    assert_eq!(
        key_fault(FileSigner::load_in_shared_dir(&file)),
        KeyFileFault::NotRegular
    );
}
