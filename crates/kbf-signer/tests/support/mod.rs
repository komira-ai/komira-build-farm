//! Helpers the tests share: a CA directory made once, a signer served in a thread, the
//! to-be-signed structures the farm's own builder makes, and ones it never would.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use kbf_ca::{
    Algorithm, Authority, Custody, Entropy, FileSigner, NodeCert, Policy, PublicKey, SignError,
    Signer, SystemEntropy,
};
use kbf_signer::{SignerServer, bind};
use kbf_testkit::dir::TestDir;
use rcgen::string::Ia5String;
use rcgen::{
    BasicConstraints, CertificateParams, CustomExtension, DistinguishedName, DnType,
    ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair, KeyUsagePurpose, PKCS_ECDSA_P256_SHA256,
    PublicKeyData, SanType, SerialNumber, SignatureAlgorithm, SigningKey,
};
use rustls::pki_types::CertificateDer;
use time::OffsetDateTime;
use x509_parser::prelude::{FromDer, X509Certificate};

/// A clock reading well past the build's floor: 2027-01-15.
pub const NOW: i64 = 1_800_000_000;

/// The system clock in Unix seconds: what a started binary reads.
pub fn real_now() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("after 1970")
            .as_secs(),
    )
    .expect("fits")
}

/// The account the tests run as.
pub fn euid() -> u32 {
    rustix::process::geteuid().as_raw()
}

/// A scratch directory, new and empty, removed when this is dropped. `path` is resolved
/// through symlinks.
pub struct Scratch {
    pub path: PathBuf,
    _dir: Option<TestDir>,
}

impl std::ops::Deref for Scratch {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.path
    }
}

/// The directory is under the system's temporary directory and not the target directory:
/// the signer's sockets are inside it, and a socket's path must fit in 108 bytes, which a
/// deep checkout's target directory can exceed.
pub fn scratch(name: &str) -> Scratch {
    let dir = TestDir::under(std::env::temp_dir(), &format!("kbf-signer-{name}"));
    let path = std::fs::canonicalize(&dir).expect("canonical scratch dir");
    Scratch {
        path,
        _dir: Some(dir),
    }
}

/// A splitmix64 stream: deterministic serials.
pub struct Stream(pub u64);

impl Entropy for Stream {
    fn fill(&mut self, dest: &mut [u8]) {
        for chunk in dest.chunks_mut(8) {
            self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^= z >> 31;
            chunk.copy_from_slice(&z.to_le_bytes()[..chunk.len()]);
        }
    }
}

/// A CA made by `Authority::init_dir`.
pub struct Ca {
    pub dir: PathBuf,
    pub root: Vec<u8>,
    pub spki: Vec<u8>,
    pub root_subject: Vec<u8>,
    pub scratch: Scratch,
}

impl Ca {
    /// Removes the CA's files now, keeping only what was read from them. For a CA held in
    /// a `static`, which is never dropped and would leave its directory behind.
    pub fn without_files(mut self) -> Ca {
        self.scratch._dir = None;
        self
    }
}

pub fn make_ca(name: &str) -> Ca {
    make_ca_at(name, NOW)
}

/// A CA whose root was made at the clock reading `now`.
pub fn make_ca_at(name: &str, now: i64) -> Ca {
    let scratch = scratch(name);
    let dir = scratch.join("ca");
    let ca = Authority::init_dir(&dir, Policy::default(), now, &mut SystemEntropy).expect("init");
    let root = ca.root_der().to_vec();
    let (_, cert) = X509Certificate::from_der(&root).expect("root");
    Ca {
        spki: cert.public_key().raw.to_vec(),
        root_subject: cert.subject().as_raw().to_vec(),
        dir,
        root,
        scratch,
    }
}

/// Serves the CA's key on `scratch/run/sock` to peers running as `allowed_uid`, in a
/// thread that lives as long as the test process. Returns the socket's path.
pub fn serve(ca: &Ca, allowed_uid: u32) -> PathBuf {
    let socket = ca.scratch.join(format!("run-{allowed_uid}")).join("sock");
    let server = SignerServer::from_dir(&ca.dir, allowed_uid, Policy::default())
        .expect("the server")
        .with_clock(|| NOW);
    let listener = bind(&socket).expect("bind");
    std::thread::spawn(move || {
        let _ = server.serve(&listener);
    });
    socket
}

// ---- to-be-signed structures ----------------------------------------------------------

/// A signer that signs nothing: it keeps the structure it is asked to sign. Its public key
/// is the CA's, so an `Authority` over it builds exactly what the frontend would send.
pub struct Spy {
    spki: Vec<u8>,
    pub seen: Mutex<Vec<Vec<u8>>>,
}

impl Spy {
    pub fn new(spki: &[u8]) -> Arc<Self> {
        Arc::new(Self {
            spki: spki.to_vec(),
            seen: Mutex::new(Vec::new()),
        })
    }

    pub fn last(&self) -> Vec<u8> {
        self.seen.lock().expect("lock").last().expect("one").clone()
    }
}

impl Signer for Spy {
    fn public_key(&self) -> &[u8] {
        &self.spki
    }

    fn custody(&self) -> Custody {
        Custody::External
    }

    fn sign(&self, tbs: &[u8], _: Algorithm) -> Result<Vec<u8>, SignError> {
        self.seen.lock().expect("lock").push(tbs.to_vec());
        // A well-formed ECDSA signature of r = s = 1.
        Ok(vec![0x30, 0x06, 0x02, 0x01, 0x01, 0x02, 0x01, 0x01])
    }
}

pub fn fresh_key() -> PublicKey {
    PublicKey::from_spki(&KeyPair::generate().expect("key").subject_public_key_info())
        .expect("P-256")
}

fn authority_over(ca: &Ca, spy: &Arc<Spy>) -> Authority {
    Authority::import(spy.clone(), Policy::default(), &ca.root).expect("authority")
}

/// The to-be-signed structure `Authority::issue_node` builds.
pub fn node_tbs(ca: &Ca, role: kbf_types::Role, advertise: Option<&str>) -> Vec<u8> {
    node_tbs_at(ca, role, advertise, NOW)
}

/// The same, issued at the clock reading `now`.
pub fn node_tbs_at(ca: &Ca, role: kbf_types::Role, advertise: Option<&str>, now: i64) -> Vec<u8> {
    let spy = Spy::new(&ca.spki);
    let id = "node-a".parse().expect("node id");
    authority_over(ca, &spy)
        .issue_node(
            &NodeCert {
                node_id: &id,
                role,
                key: &fresh_key(),
                advertise,
            },
            now,
            &mut Stream(7),
        )
        .expect("issue");
    spy.last()
}

/// The to-be-signed structure `Authority::issue_frontend` builds.
pub fn frontend_tbs(ca: &Ca) -> Vec<u8> {
    let spy = Spy::new(&ca.spki);
    authority_over(ca, &spy)
        .issue_frontend(&["frontend.example"], &fresh_key(), NOW, &mut Stream(8))
        .expect("issue");
    spy.last()
}

/// The to-be-signed structure of the root itself (`CA:TRUE`).
pub fn root_tbs(ca: &Ca) -> Vec<u8> {
    let (_, cert) = X509Certificate::from_der(&ca.root).expect("root");
    cert.tbs_certificate.as_ref().to_vec()
}

/// An rcgen signing key that signs nothing and keeps the structure it is asked to sign.
struct Recorder<'a> {
    point: &'a [u8],
    algorithm: &'static SignatureAlgorithm,
    seen: &'a Mutex<Option<Vec<u8>>>,
}

impl PublicKeyData for Recorder<'_> {
    fn der_bytes(&self) -> &[u8] {
        self.point
    }

    fn algorithm(&self) -> &'static SignatureAlgorithm {
        self.algorithm
    }
}

impl SigningKey for Recorder<'_> {
    fn sign(&self, msg: &[u8]) -> Result<Vec<u8>, rcgen::Error> {
        *self.seen.lock().expect("lock") = Some(msg.to_vec());
        Ok(vec![0; 64])
    }
}

/// The certificate parameters of a daemon certificate, as the table has them.
pub fn daemon_params() -> CertificateParams {
    let mut p = CertificateParams::default();
    p.not_before = OffsetDateTime::from_unix_timestamp(NOW - 300).expect("time");
    p.not_after = p.not_before + time::Duration::days(7);
    p.serial_number = Some(SerialNumber::from_slice(&[0x11; 8]));
    let mut name = DistinguishedName::new();
    name.push(DnType::CommonName, "node-a");
    p.distinguished_name = name;
    p.is_ca = IsCa::ExplicitNoCa;
    p.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    p.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
    p.use_authority_key_identifier_extension = true;
    p.subject_alt_names = vec![role_uri("daemon"), dns("node-a")];
    p
}

pub fn role_uri(role: &str) -> SanType {
    SanType::URI(Ia5String::try_from(format!("urn:kbf:role:{role}")).expect("ia5"))
}

pub fn dns(name: &str) -> SanType {
    SanType::DnsName(Ia5String::try_from(name).expect("ia5"))
}

pub fn custom_extension() -> CustomExtension {
    CustomExtension::from_oid_content(&[1, 3, 6, 1, 4, 1, 99999, 1], vec![0x05, 0x00])
}

pub fn ca_true() -> IsCa {
    IsCa::Ca(BasicConstraints::Constrained(0))
}

/// What to sign, and under which key's name.
pub struct Evil<'a> {
    pub ca: &'a Ca,
    pub params: CertificateParams,
    /// The certified key.
    pub subject: KeyPair,
    /// The algorithm the structure names for its signature.
    pub algorithm: &'static SignatureAlgorithm,
    /// The name the structure says it was issued by, if not the root's.
    pub issuer: Option<CertificateParams>,
}

impl<'a> Evil<'a> {
    pub fn new(ca: &'a Ca) -> Self {
        Self {
            ca,
            params: daemon_params(),
            subject: KeyPair::generate().expect("key"),
            algorithm: &PKCS_ECDSA_P256_SHA256,
            issuer: None,
        }
    }

    /// The to-be-signed structure rcgen builds from the parameters, kept before signing.
    pub fn tbs(self) -> Vec<u8> {
        let seen = Mutex::new(None);
        let point = &self.ca.spki[self.ca.spki.len() - 65..];
        let recorder = Recorder {
            point,
            algorithm: self.algorithm,
            seen: &seen,
        };
        let issuer = match self.issuer {
            Some(params) => Issuer::new(params, recorder),
            None => {
                Issuer::from_ca_cert_der(&CertificateDer::from(self.ca.root.as_slice()), recorder)
                    .expect("issuer")
            }
        };
        self.params
            .signed_by(&self.subject, &issuer)
            .expect("rcgen signs");
        seen.into_inner().expect("lock").expect("signed")
    }
}

pub fn path_str(p: &Path) -> &str {
    p.to_str().expect("utf8")
}

/// FileSigner re-exported for the tests that need a second key.
pub fn another_key() -> FileSigner {
    FileSigner::generate().expect("key")
}

// ---- groups ---------------------------------------------------------------------------

/// The test's effective group id.
pub fn egid() -> u32 {
    rustix::process::getegid().as_raw()
}

/// A group this account may give files to: one of its supplementary groups other than its
/// effective group if it has one (so a hand-over is visible), else the effective group.
pub fn a_group_to_share_with() -> u32 {
    rustix::process::getgroups()
        .expect("getgroups")
        .into_iter()
        .map(|g| g.as_raw())
        .find(|g| *g != egid())
        .unwrap_or_else(egid)
}

/// The name in `/etc/group` of a group this account belongs to, with its id: what a
/// started binary can resolve by `--client-group`.
pub fn a_named_group_of_mine() -> (String, u32) {
    let mut mine: Vec<u32> = rustix::process::getgroups()
        .expect("getgroups")
        .into_iter()
        .map(|g| g.as_raw())
        .collect();
    mine.push(egid());
    std::fs::read_to_string("/etc/group")
        .expect("/etc/group")
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split(':').collect();
            Some((f.first()?.to_string(), f.get(2)?.parse::<u32>().ok()?))
        })
        .find(|(_, gid)| mine.contains(gid))
        .expect("this account belongs to a group that /etc/group names")
}

/// `passwd` and `group` files in `dir` with exactly these lines.
pub fn accounts(dir: &Path, passwd: &[&str], group: &[&str]) -> kbf_signer::Accounts {
    std::fs::create_dir_all(dir).expect("dir");
    std::fs::write(dir.join("passwd"), passwd.join("\n")).expect("passwd");
    std::fs::write(dir.join("group"), group.join("\n")).expect("group");
    kbf_signer::Accounts::in_dir(dir)
}
