//! The farm's certificate authority and the certificates a rehearsal's processes hold,
//! made as the farm's issuer makes them: one per row of the certificate table, each
//! checked by `kbf-deploy linux verify-cert`'s own function before a process reads it, so
//! a certificate that a deployed node would be refused never reaches the farm.

use std::net::IpAddr;
use std::path::Path;
use std::time::SystemTime;

use kbf_deploy::linux::verify_cert::{CLIENT_AUTH, ROLE_URI_PREFIX, Row, verify};
use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, ExtendedKeyUsagePurpose as Eku, IsCa,
    KeyPair, KeyUsagePurpose, SanType,
};

pub(crate) struct Pki {
    ca: CertifiedIssuer<'static, KeyPair>,
}

/// The name as a subjectAltName: an address, or a DNS name.
fn san(name: &str) -> SanType {
    match name.parse::<IpAddr>() {
        Ok(ip) => SanType::IpAddress(ip),
        Err(_) => SanType::DnsName(name.to_owned().try_into().expect("IA5")),
    }
}

impl Pki {
    pub(crate) fn new() -> Self {
        let mut params = CertificateParams::new(Vec::<String>::new()).expect("CA params");
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        let ca = CertifiedIssuer::self_signed(params, KeyPair::generate().expect("key"))
            .expect("the CA");
        Self { ca }
    }

    /// Writes the CA certificate as `ca.pem` in `etc`.
    pub(crate) fn write_ca(&self, etc: &Path) {
        std::fs::write(etc.join("ca.pem"), self.ca.pem()).expect("write the CA");
    }

    /// The certificate of `row` for `names` (a daemon's: its one name, the node id), as
    /// `<stem>.pem` and `<stem>.key` in `etc`. Panics if `verify-cert` would fail it.
    pub(crate) fn issue(&self, etc: &Path, stem: &str, row: Row, names: &[&str]) {
        let mut leaf = CertificateParams::new(Vec::<String>::new()).expect("params");
        leaf.subject_alt_names = names.iter().map(|n| san(n)).collect();
        leaf.subject_alt_names.push(SanType::URI(
            format!("{ROLE_URI_PREFIX}{}", row.role())
                .try_into()
                .expect("IA5"),
        ));
        self.sign(etc, stem, leaf, row.ekus());
        let read = |name: &str| std::fs::read(etc.join(name)).expect("read");
        let node_id = (row == Row::Daemon).then(|| names[0]);
        let failures = verify(
            self.ca.pem().as_bytes(),
            &read(&format!("{stem}.pem")),
            row,
            node_id,
            SystemTime::now(),
        )
        .expect("verify-cert checks it");
        assert!(failures.is_empty(), "{stem} is no {row:?}: {failures:?}");
    }

    /// The frontend's REAPI certificate, which is no row's (the farm CA signs it here;
    /// a deployment may use another authority): a TLS server for `names`.
    pub(crate) fn issue_reapi(&self, etc: &Path, names: &[&str]) {
        let mut leaf = CertificateParams::new(Vec::<String>::new()).expect("params");
        leaf.subject_alt_names = names.iter().map(|n| san(n)).collect();
        self.sign(etc, "reapi", leaf, &["serverAuth"]);
    }

    fn sign(&self, etc: &Path, stem: &str, mut leaf: CertificateParams, ekus: &[&str]) {
        leaf.is_ca = IsCa::ExplicitNoCa;
        leaf.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        leaf.extended_key_usages = ekus
            .iter()
            .map(|e| {
                if *e == CLIENT_AUTH {
                    Eku::ClientAuth
                } else {
                    Eku::ServerAuth
                }
            })
            .collect();
        leaf.use_authority_key_identifier_extension = true;
        let key = KeyPair::generate().expect("key");
        let cert = leaf.signed_by(&key, &self.ca).expect("sign");
        std::fs::write(etc.join(format!("{stem}.pem")), cert.pem()).expect("write");
        std::fs::write(etc.join(format!("{stem}.key")), key.serialize_pem()).expect("write");
    }
}
