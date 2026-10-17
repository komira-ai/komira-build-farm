//! A throwaway farm CA and the certificates a frontend issues, built the way design
//! section 5.5 says: from a request's public key alone.

use std::sync::atomic::{AtomicU64, Ordering};

use kbf_proto::kbf::enroll::v1::IssuedCert;
use kbf_types::{CaPin, Role};
use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, DnType, ExtendedKeyUsagePurpose, IsCa,
    KeyPair, KeyUsagePurpose, SanType, SerialNumber, SubjectPublicKeyInfo,
};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use x509_parser::prelude::{FromDer, X509CertificationRequest};

pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("after 1970")
        .as_secs() as i64
}

fn at(secs: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(secs).expect("in range")
}

/// One CA: a self-signed root with `pathlen 0`.
pub struct Pki {
    ca: CertifiedIssuer<'static, KeyPair>,
    serial: AtomicU64,
}

/// What a frontend certificate (for the enroll listener) looks like.
pub struct FrontendCert {
    pub names: Vec<String>,
    pub role_uri: Option<&'static str>,
    pub eku: Vec<ExtendedKeyUsagePurpose>,
    pub not_before: i64,
    pub not_after: i64,
}

impl FrontendCert {
    pub fn good() -> Self {
        Self {
            names: vec!["localhost".to_owned(), "127.0.0.1".to_owned()],
            role_uri: Some("urn:kbf:role:frontend"),
            eku: vec![
                ExtendedKeyUsagePurpose::ServerAuth,
                ExtendedKeyUsagePurpose::ClientAuth,
            ],
            not_before: now() - 300,
            not_after: now() + 86_400,
        }
    }
}

/// A certificate and key as the PEM a listener is given.
pub struct Identity {
    pub chain_pem: String,
    pub key_pem: String,
    pub leaf_der: Vec<u8>,
}

/// How a certificate is issued; the defaults are section 5.5's table.
pub struct Issue<'a> {
    pub csr_der: &'a [u8],
    pub role: Role,
    pub node_name: &'a str,
    pub extra_dns: Option<&'a str>,
    pub role_uri: Option<String>,
    pub not_before: i64,
    pub not_after: i64,
}

impl Pki {
    pub fn new() -> Self {
        Self::named("kbf test CA")
    }

    pub fn named(name: &str) -> Self {
        let mut params = CertificateParams::new(Vec::<String>::new()).expect("params");
        params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
        params.distinguished_name.push(DnType::CommonName, name);
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        params.not_before = at(now() - 3600);
        params.not_after = at(now() + 10 * 365 * 86_400);
        let ca = CertifiedIssuer::self_signed(params, KeyPair::generate().expect("key"))
            .expect("self-signed");
        Self {
            ca,
            serial: AtomicU64::new(0x1000),
        }
    }

    pub fn root_der(&self) -> Vec<u8> {
        self.ca.der().to_vec()
    }

    pub fn root_pem(&self) -> String {
        self.ca.pem()
    }

    pub fn pin(&self) -> CaPin {
        CaPin::from_bytes(Sha256::digest(self.root_der()).into())
    }

    /// A certificate for the enroll listener, with the root appended to the chain it sends.
    pub fn frontend(&self, spec: &FrontendCert) -> Identity {
        let mut params = CertificateParams::new(Vec::<String>::new()).expect("params");
        for name in &spec.names {
            params.subject_alt_names.push(match name.parse() {
                Ok(ip) => SanType::IpAddress(ip),
                Err(_) => SanType::DnsName(name.clone().try_into().expect("dns name")),
            });
        }
        if let Some(uri) = spec.role_uri {
            params
                .subject_alt_names
                .push(SanType::URI(uri.try_into().expect("uri")));
        }
        params.extended_key_usages = spec.eku.clone();
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.not_before = at(spec.not_before);
        params.not_after = at(spec.not_after);
        let key = KeyPair::generate().expect("key");
        let cert = params.signed_by(&key, &self.ca).expect("signed");
        Identity {
            chain_pem: format!("{}{}", cert.pem(), self.ca.pem()),
            key_pem: key.serialize_pem(),
            leaf_der: cert.der().to_vec(),
        }
    }

    /// Issues a node certificate for the public key in a request, taking nothing else from it.
    pub fn issue(&self, how: &Issue<'_>) -> Result<IssuedCert, String> {
        let (_, csr) = X509CertificationRequest::from_der(how.csr_der)
            .map_err(|e| format!("csr does not parse: {e}"))?;
        csr.verify_signature()
            .map_err(|e| format!("csr signature: {e}"))?;
        let public_key =
            SubjectPublicKeyInfo::from_der(csr.certification_request_info.subject_pki.raw)
                .map_err(|e| format!("csr key: {e}"))?;
        let mut params = CertificateParams::new(Vec::<String>::new()).map_err(|e| e.to_string())?;
        params
            .distinguished_name
            .push(DnType::CommonName, how.node_name);
        params.subject_alt_names.push(SanType::URI(
            how.role_uri
                .clone()
                .unwrap_or_else(|| format!("urn:kbf:role:{}", how.role.name()))
                .try_into()
                .map_err(|e: rcgen::Error| e.to_string())?,
        ));
        params.subject_alt_names.push(SanType::DnsName(
            how.node_name
                .to_owned()
                .try_into()
                .map_err(|e: rcgen::Error| e.to_string())?,
        ));
        if let Some(extra) = how.extra_dns {
            params.subject_alt_names.push(SanType::DnsName(
                extra
                    .to_owned()
                    .try_into()
                    .map_err(|e: rcgen::Error| e.to_string())?,
            ));
        }
        params.extended_key_usages = match how.role {
            Role::Daemon => vec![ExtendedKeyUsagePurpose::ClientAuth],
            Role::Storage => vec![
                ExtendedKeyUsagePurpose::ServerAuth,
                ExtendedKeyUsagePurpose::ClientAuth,
            ],
        };
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.not_before = at(how.not_before);
        params.not_after = at(how.not_after);
        let serial = self.serial.fetch_add(1, Ordering::Relaxed);
        params.serial_number = Some(SerialNumber::from(serial));
        let cert = params
            .signed_by(&public_key, &self.ca)
            .map_err(|e| e.to_string())?;
        Ok(IssuedCert {
            role: how.role.name().to_owned(),
            leaf_der: cert.der().to_vec(),
            serial: format!("{serial:x}"),
            not_before: how.not_before,
            not_after: how.not_after,
        })
    }

    /// A second root with the same name and key as `next`, signed by this one: the
    /// cross-signature of a rotation (design section 6.4).
    pub fn cross_sign(&self, next: &Pki) -> Vec<u8> {
        let mut params = CertificateParams::new(Vec::<String>::new()).expect("params");
        params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
        params
            .distinguished_name
            .push(DnType::CommonName, "kbf test CA next");
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        params.not_before = at(now() - 3600);
        params.not_after = at(now() + 365 * 86_400);
        let next_key = SubjectPublicKeyInfo::from_der(
            x509_parser::parse_x509_certificate(&next.root_der())
                .expect("root parses")
                .1
                .public_key()
                .raw,
        )
        .expect("spki");
        params
            .signed_by(&next_key, &self.ca)
            .expect("cross-signed")
            .der()
            .to_vec()
    }
}

/// PEM for a certificate.
pub fn pem(der: &[u8]) -> String {
    kbf_enroll::answer::pem_encode("CERTIFICATE", der)
}
