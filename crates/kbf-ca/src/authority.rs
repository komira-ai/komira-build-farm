//! The authority: a root, the signer behind it and the certificates it builds.

use std::fmt;
use std::net::IpAddr;
use std::path::Path;
use std::sync::Arc;

use kbf_types::{CaPin, HostRole, NodeId, Role};
use rcgen::string::Ia5String;
use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa,
    Issuer, KeyUsagePurpose, PKCS_ECDSA_P256_SHA256, PublicKeyData, SanType, SerialNumber,
    SignatureAlgorithm, SigningKey,
};
use rustls::pki_types::CertificateDer;
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use x509_parser::prelude::{FromDer, X509Certificate};

use crate::bundle::{Bundle, pin_of};
use crate::entropy::Entropy;
use crate::error::CaError;
use crate::key::PublicKey;
use crate::keyfile;
use crate::policy::Policy;
use crate::serial;
use crate::signer::{Algorithm, Custody, FileSigner, Signer};

/// How long a root lives: ten years.
const ROOT_LIFETIME_S: i64 = 3650 * 86_400;

/// How a certificate names its role: this, then the role's name.
const ROLE_URI_PREFIX: &str = "urn:kbf:role:";

/// The certificate asked for a node: the table of the design, from these fields only.
#[derive(Clone, Copy, Debug)]
pub struct NodeCert<'a> {
    /// The node: the certificate's one DNS name (and its subject, which nothing reads).
    pub node_id: &'a NodeId,
    /// The role: the certificate's one role URI.
    pub role: Role,
    /// The node's key, from a request that proved possession.
    pub key: &'a PublicKey,
    /// A storage node's advertise host, if different from its id. Syntax only is checked
    /// here (a lower-case DNS name, or an address); whether the host is allowed is the
    /// caller's policy.
    pub advertise: Option<&'a str>,
}

/// The certificate asked for a host's updater: the node's one DNS name, the role `updater`
/// and the host's role in a second URI, `urn:kbf:host-role:<name>`
/// (`docs/design/updates.md`, section 5).
#[derive(Clone, Copy, Debug)]
pub struct UpdaterCert<'a> {
    /// The host: the certificate's one DNS name (and its subject, which nothing reads).
    pub node_id: &'a NodeId,
    /// What the host runs, which the coordinator limits and orders it by.
    pub host_role: HostRole,
    /// The updater's key, from a request that proved possession.
    pub key: &'a PublicKey,
}

/// A certificate the authority built.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Issued {
    /// The certificate, DER.
    pub der: Vec<u8>,
    /// The serial, lower-case hex without leading zeros: the deny list's spelling.
    pub serial: String,
    /// Unix seconds.
    pub not_before: i64,
    /// Unix seconds.
    pub not_after: i64,
}

/// A CA root and the signer behind it.
pub struct Authority {
    signer: Arc<dyn Signer>,
    key: PublicKey,
    root: Vec<u8>,
    policy: Policy,
}

/// A staged replacement of the root: the new authority, and its root certified by the old.
pub struct Rotation {
    next: Authority,
    cross: Vec<u8>,
    current: Vec<u8>,
}

/// A public key as rcgen reads one.
struct Subject<'a>(&'a PublicKey);

impl PublicKeyData for Subject<'_> {
    fn der_bytes(&self) -> &[u8] {
        self.0.point().as_slice()
    }

    fn algorithm(&self) -> &'static SignatureAlgorithm {
        &PKCS_ECDSA_P256_SHA256
    }
}

/// The CA key as rcgen signs with it: the public half is known, the signature comes
/// through the [`Signer`] seam.
struct Remote<'a> {
    signer: &'a dyn Signer,
    key: &'a PublicKey,
}

impl PublicKeyData for Remote<'_> {
    fn der_bytes(&self) -> &[u8] {
        self.key.point().as_slice()
    }

    fn algorithm(&self) -> &'static SignatureAlgorithm {
        &PKCS_ECDSA_P256_SHA256
    }
}

impl SigningKey for Remote<'_> {
    fn sign(&self, msg: &[u8]) -> Result<Vec<u8>, rcgen::Error> {
        self.signer
            .sign(msg, Algorithm::EcdsaP256Sha256)
            .map_err(|_| rcgen::Error::RemoteKeyError)
    }
}

/// Parameters with the validity and serial stamped, and the numbers that went in.
struct Stamped {
    params: CertificateParams,
    serial: String,
    not_before: i64,
    not_after: i64,
}

fn instant(t: i64) -> Result<OffsetDateTime, CaError> {
    OffsetDateTime::from_unix_timestamp(t).map_err(|_| CaError::TimeRange)
}

/// Fresh parameters valid from `now` minus the backdate for `lifetime` seconds, with a
/// random serial. Refuses a clock before the policy's floor.
fn stamp(
    policy: Policy,
    lifetime: i64,
    now: i64,
    entropy: &mut dyn Entropy,
) -> Result<Stamped, CaError> {
    if now < policy.clock_floor() {
        return Err(CaError::ClockBeforeFloor {
            now,
            floor: policy.clock_floor(),
        });
    }
    let not_before = now.saturating_sub(Policy::BACKDATE_S);
    let not_after = not_before.checked_add(lifetime).ok_or(CaError::TimeRange)?;
    let mut params = CertificateParams::default();
    params.not_before = instant(not_before)?;
    params.not_after = instant(not_after)?;
    let serial = serial::draw(entropy);
    let be = serial.to_be_bytes();
    // The serial is nonzero, so at most 15 leading bytes are zero; DER wants none.
    let skip = (serial.leading_zeros() / 8) as usize;
    params.serial_number = Some(SerialNumber::from_slice(&be[skip..]));
    Ok(Stamped {
        params,
        serial: format!("{serial:x}"),
        not_before,
        not_after,
    })
}

fn common_name(cn: &str) -> DistinguishedName {
    let mut name = DistinguishedName::new();
    name.push(DnType::CommonName, cn);
    name
}

/// Parameters of a CA certificate for `key`: pathlen 0, `keyCertSign` and `cRLSign`, ten
/// years, named after the key so two roots never share a name.
fn ca_params(
    key: &PublicKey,
    policy: Policy,
    now: i64,
    entropy: &mut dyn Entropy,
) -> Result<CertificateParams, CaError> {
    let mut stamped = stamp(policy, ROOT_LIFETIME_S, now, entropy)?;
    let digest = Sha256::digest(key.spki());
    let tag: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
    stamped.params.distinguished_name = common_name(&format!("kbf-ca-{tag}"));
    stamped.params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    stamped.params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    stamped.params.use_authority_key_identifier_extension = true;
    Ok(stamped.params)
}

/// An IA5 string, or the refusal of a name that is not ASCII.
fn ia5(text: &str) -> Result<Ia5String, CaError> {
    Ia5String::try_from(text).map_err(|_| CaError::Name)
}

/// One DNS label: 1 to 63 of `a-z`, `0-9` and `-`, not starting or ending with `-`.
fn valid_label(label: &str) -> bool {
    (1..=63).contains(&label.len())
        && label
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !label.starts_with('-')
        && !label.ends_with('-')
}

/// A lower-case DNS name of at most 253 characters whose labels are valid and whose last
/// label is not all digits (so `1.2.3.4.5` is not taken for a name).
fn is_dns_name(text: &str) -> bool {
    text.len() <= 253
        && text.split('.').all(valid_label)
        && !text
            .rsplit('.')
            .next()
            .is_some_and(|last| last.bytes().all(|b| b.is_ascii_digit()))
}

/// The subjectAltName for a host: an address, or a DNS name.
fn san_of(host: &str) -> Result<SanType, CaError> {
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(SanType::IpAddress(ip));
    }
    let name = ia5(host)?;
    if !is_dns_name(host) {
        return Err(CaError::Name);
    }
    Ok(SanType::DnsName(name))
}

/// What a leaf is made from.
struct Leaf<'a> {
    key: &'a PublicKey,
    common_name: &'a str,
    role: &'a str,
    names: Vec<SanType>,
    ekus: Vec<ExtendedKeyUsagePurpose>,
}

impl Authority {
    /// The CA certificate's name in a CA directory.
    pub const CERT_FILE: &'static str = "ca.pem";

    /// A new self-signed root for the signer's key (basicConstraints `CA:TRUE, pathlen 0`,
    /// keyUsage `keyCertSign, cRLSign`, ten years).
    ///
    /// # Errors
    /// [`CaError`].
    pub fn create(
        signer: Arc<dyn Signer>,
        policy: Policy,
        now: i64,
        entropy: &mut dyn Entropy,
    ) -> Result<Self, CaError> {
        let key = PublicKey::from_spki(signer.public_key()).map_err(|_| CaError::KeyType)?;
        let params = ca_params(&key, policy, now, entropy)?;
        let root = params.self_signed(&Remote {
            signer: &*signer,
            key: &key,
        })?;
        Ok(Self {
            root: root.der().to_vec(),
            signer,
            key,
            policy,
        })
    }

    /// The authority of an existing self-signed root and the signer for its key.
    ///
    /// # Errors
    /// [`CaError`]: the certificate does not parse (or has bytes after it), is not a CA,
    /// is for another key than the signer's, or is not signed by that key.
    pub fn import(
        signer: Arc<dyn Signer>,
        policy: Policy,
        root_der: &[u8],
    ) -> Result<Self, CaError> {
        let key = PublicKey::from_spki(signer.public_key()).map_err(|_| CaError::KeyType)?;
        let (rest, cert) = X509Certificate::from_der(root_der).map_err(|_| CaError::Parse)?;
        if !rest.is_empty() {
            return Err(CaError::Parse);
        }
        let is_ca = cert
            .basic_constraints()
            .ok()
            .flatten()
            .is_some_and(|b| b.value.ca);
        if !is_ca {
            return Err(CaError::NotCa);
        }
        if cert.public_key().raw != key.spki() {
            return Err(CaError::KeyMismatch);
        }
        if cert.verify_signature(None).is_err() {
            return Err(CaError::NotSelfSigned);
        }
        Ok(Self {
            signer,
            key,
            root: root_der.to_vec(),
            policy,
        })
    }

    /// Makes the CA directory with a new file key and root (`ca.key` 0600, `ca.pem` 0644).
    /// The root is built first, so a refused clock leaves nothing on disk.
    ///
    /// # Errors
    /// [`CaError`]; nothing is overwritten.
    pub fn init_dir(
        dir: &Path,
        policy: Policy,
        now: i64,
        entropy: &mut dyn Entropy,
    ) -> Result<Self, CaError> {
        let signer = Arc::new(FileSigner::generate()?);
        let ca = Self::create(signer.clone(), policy, now, entropy)?;
        signer.store_in(dir)?;
        keyfile::write_new(
            &dir.join(Self::CERT_FILE),
            ca.bundle().to_pem().as_bytes(),
            0o644,
        )?;
        Ok(ca)
    }

    /// Loads the CA directory for the running account.
    ///
    /// # Errors
    /// [`CaError`].
    pub fn load_dir(dir: &Path, policy: Policy) -> Result<Self, CaError> {
        Self::load_dir_owned_by(dir, policy, keyfile::effective_uid())
    }

    /// Loads the CA directory, expecting it and its key to belong to `uid`. The root is the
    /// first certificate of `ca.pem`.
    ///
    /// # Errors
    /// [`CaError`].
    pub fn load_dir_owned_by(dir: &Path, policy: Policy, uid: u32) -> Result<Self, CaError> {
        Self::load_with(dir, policy, FileSigner::load_owned_by(dir, uid)?)
    }

    /// Loads the CA directory of the running account when the directory is shared with a
    /// group (`FileSigner::load_in_shared_dir`): the key is as private as ever.
    ///
    /// # Errors
    /// [`CaError`].
    pub fn load_dir_shared(dir: &Path, policy: Policy) -> Result<Self, CaError> {
        Self::load_with(dir, policy, FileSigner::load_in_shared_dir(dir)?)
    }

    fn load_with(dir: &Path, policy: Policy, signer: FileSigner) -> Result<Self, CaError> {
        let pem = keyfile::read_public(&dir.join(Self::CERT_FILE))?;
        let bundle = Bundle::from_pem(&pem)?;
        Self::import(Arc::new(signer), policy, &bundle.certs()[0])
    }

    /// Passes the authority on if its key is outside this process; the production gate.
    ///
    /// # Errors
    /// [`CaError::InProcessSigner`].
    pub fn require_external(self) -> Result<Self, CaError> {
        match self.signer.custody() {
            Custody::External => Ok(self),
            Custody::InProcess => Err(CaError::InProcessSigner),
        }
    }

    /// The root, DER.
    #[must_use]
    pub fn root_der(&self) -> &[u8] {
        &self.root
    }

    /// The pin of the root: what a token carries.
    #[must_use]
    pub fn pin(&self) -> CaPin {
        pin_of(&self.root)
    }

    /// The bundle of the root alone.
    #[must_use]
    pub fn bundle(&self) -> Bundle {
        Bundle::trusted(vec![self.root.clone()])
    }

    /// The policy.
    #[must_use]
    pub const fn policy(&self) -> Policy {
        self.policy
    }

    /// The issuer rcgen signs as: the root's name and key identifier, the signer's key.
    fn issuer(&self) -> Result<Issuer<'static, Remote<'_>>, CaError> {
        Ok(Issuer::from_ca_cert_der(
            &CertificateDer::from(self.root.as_slice()),
            Remote {
                signer: &*self.signer,
                key: &self.key,
            },
        )?)
    }

    /// Builds and signs the one end-entity shape of the design's table.
    fn sign_leaf(
        &self,
        leaf: Leaf<'_>,
        now: i64,
        entropy: &mut dyn Entropy,
    ) -> Result<Issued, CaError> {
        let mut stamped = stamp(self.policy, self.policy.lifetime_s(), now, entropy)?;
        let params = &mut stamped.params;
        params.distinguished_name = common_name(leaf.common_name);
        params.is_ca = IsCa::ExplicitNoCa;
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = leaf.ekus;
        params.use_authority_key_identifier_extension = true;
        params.subject_alt_names = vec![SanType::URI(ia5(&format!(
            "{ROLE_URI_PREFIX}{}",
            leaf.role
        ))?)];
        params.subject_alt_names.extend(leaf.names);
        let cert = params.signed_by(&Subject(leaf.key), &self.issuer()?)?;
        Ok(Issued {
            der: cert.der().to_vec(),
            serial: stamped.serial,
            not_before: stamped.not_before,
            not_after: stamped.not_after,
        })
    }

    /// A certificate for a node.
    ///
    /// # Errors
    /// [`CaError`]: a clock or time the policy refuses, a name that is not a name, an
    /// advertise host for a daemon, the role `updater` (see [`Authority::issue_updater`]), or a
    /// signer that fails.
    pub fn issue_node(
        &self,
        cert: &NodeCert<'_>,
        now: i64,
        entropy: &mut dyn Entropy,
    ) -> Result<Issued, CaError> {
        if cert.role == Role::Updater {
            return Err(CaError::NotANodeRole);
        }
        let id = cert.node_id.as_str();
        let mut names = vec![SanType::DnsName(ia5(id)?)];
        if let Some(host) = cert.advertise {
            if cert.role == Role::Daemon {
                return Err(CaError::AdvertiseForDaemon);
            }
            if host != id {
                names.push(san_of(host)?);
            }
        }
        let ekus = match cert.role {
            Role::Storage => vec![
                ExtendedKeyUsagePurpose::ServerAuth,
                ExtendedKeyUsagePurpose::ClientAuth,
            ],
            Role::Daemon | Role::Updater => vec![ExtendedKeyUsagePurpose::ClientAuth],
        };
        self.sign_leaf(
            Leaf {
                key: cert.key,
                common_name: id,
                role: cert.role.name(),
                names,
                ekus,
            },
            now,
            entropy,
        )
    }

    /// The frontend's own certificate (role `frontend`, serverAuth and clientAuth) for
    /// `names`, DNS names or addresses. Not reachable from a token: the role type a token
    /// grants has no frontend variant.
    ///
    /// # Errors
    /// [`CaError`].
    pub fn issue_frontend(
        &self,
        names: &[&str],
        key: &PublicKey,
        now: i64,
        entropy: &mut dyn Entropy,
    ) -> Result<Issued, CaError> {
        let ekus = vec![
            ExtendedKeyUsagePurpose::ServerAuth,
            ExtendedKeyUsagePurpose::ClientAuth,
        ];
        self.issue_named("frontend", names, ekus, key, now, entropy)
    }

    /// The update coordinator's own certificate (role `coordinator`, serverAuth only: it
    /// serves its hosts' updaters and calls nobody) for `names`, DNS names or addresses. Not
    /// reachable from a token.
    ///
    /// # Errors
    /// [`CaError`].
    pub fn issue_coordinator(
        &self,
        names: &[&str],
        key: &PublicKey,
        now: i64,
        entropy: &mut dyn Entropy,
    ) -> Result<Issued, CaError> {
        let ekus = vec![ExtendedKeyUsagePurpose::ServerAuth];
        self.issue_named("coordinator", names, ekus, key, now, entropy)
    }

    /// A host's updater certificate (role `updater`, clientAuth only: it dials the
    /// coordinator and nobody dials it).
    ///
    /// # Errors
    /// [`CaError`]: a clock or time the policy refuses, a node id that is not a name, or a
    /// signer that fails.
    pub fn issue_updater(
        &self,
        cert: &UpdaterCert<'_>,
        now: i64,
        entropy: &mut dyn Entropy,
    ) -> Result<Issued, CaError> {
        let id = cert.node_id.as_str();
        let host_role = SanType::URI(ia5(&format!(
            "{}{}",
            HostRole::URI_PREFIX,
            cert.host_role.name()
        ))?);
        self.sign_leaf(
            Leaf {
                key: cert.key,
                common_name: id,
                role: "updater",
                names: vec![SanType::DnsName(ia5(id)?), host_role],
                ekus: vec![ExtendedKeyUsagePurpose::ClientAuth],
            },
            now,
            entropy,
        )
    }

    /// A certificate with `role` for the host names in `names`.
    fn issue_named(
        &self,
        role: &str,
        names: &[&str],
        ekus: Vec<ExtendedKeyUsagePurpose>,
        key: &PublicKey,
        now: i64,
        entropy: &mut dyn Entropy,
    ) -> Result<Issued, CaError> {
        let Some(first) = names.first() else {
            return Err(CaError::NoNames);
        };
        let names_san = names
            .iter()
            .map(|n| san_of(n))
            .collect::<Result<Vec<_>, _>>()?;
        self.sign_leaf(
            Leaf {
                key,
                common_name: first,
                role,
                names: names_san,
                ekus,
            },
            now,
            entropy,
        )
    }

    /// Stages a root for `next`'s key: self-signed, and cross-signed by this root so a node
    /// that holds only this root's pin can verify certificates of the new one.
    ///
    /// # Errors
    /// [`CaError::SameKey`] for the key already in use; otherwise as [`Self::create`].
    pub fn stage_rotation(
        &self,
        next: Arc<dyn Signer>,
        now: i64,
        entropy: &mut dyn Entropy,
    ) -> Result<Rotation, CaError> {
        if next.public_key() == self.signer.public_key() {
            return Err(CaError::SameKey);
        }
        let next = Self::create(next, self.policy, now, entropy)?;
        let params = ca_params(&next.key, self.policy, now, entropy)?;
        let cross = params.signed_by(&Subject(&next.key), &self.issuer()?)?;
        Ok(Rotation {
            cross: cross.der().to_vec(),
            current: self.root.clone(),
            next,
        })
    }
}

impl fmt::Debug for Authority {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Authority {{ pin: {} }}", self.pin())
    }
}

impl Rotation {
    /// The staged authority: issues from the new root once activated.
    #[must_use]
    pub const fn next(&self) -> &Authority {
        &self.next
    }

    /// The new root's key certified by the old root, DER.
    #[must_use]
    pub fn cross_signed_der(&self) -> &[u8] {
        &self.cross
    }

    /// Step 1, stage: `[current, next, next cross-signed by current]`.
    #[must_use]
    pub fn staged_bundle(&self) -> Bundle {
        Bundle::trusted(vec![
            self.current.clone(),
            self.next.root.clone(),
            self.cross.clone(),
        ])
    }

    /// Step 3, activate: `[next, current, next cross-signed by current]`.
    #[must_use]
    pub fn activated_bundle(&self) -> Bundle {
        Bundle::trusted(vec![
            self.next.root.clone(),
            self.current.clone(),
            self.cross.clone(),
        ])
    }

    /// Step 4, retire: `[next]`.
    #[must_use]
    pub fn retired_bundle(&self) -> Bundle {
        self.next.bundle()
    }

    /// The new authority, to issue from.
    #[must_use]
    pub fn into_next(self) -> Authority {
        self.next
    }
}
