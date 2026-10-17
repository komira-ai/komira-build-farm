//! `verify-cert` against certificates made in the test the way the farm's issuer makes
//! them (an EC P-256 key; `CA:FALSE`, `digitalSignature`, the row's EKU, the authority
//! key id; the names and one role URI in the subjectAltName), and against each way one
//! can differ.

use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, ExtendedKeyUsagePurpose as Eku, IsCa,
    KeyPair, KeyUsagePurpose, SanType,
};

use super::*;

type Ca = CertifiedIssuer<'static, KeyPair>;

fn ca() -> Ca {
    let mut params = CertificateParams::new(Vec::<String>::new()).expect("params");
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    CertifiedIssuer::self_signed(params, KeyPair::generate().expect("key")).expect("CA")
}

/// A leaf of `ca` with `sans` (`DNS:`, `IP:`, `URI:` or `EMAIL:` and the value) and
/// `ekus`; `edit` changes anything else before it is signed.
fn leaf(
    ca: &Ca,
    sans: &[&str],
    ekus: Vec<Eku>,
    edit: impl FnOnce(&mut CertificateParams),
) -> String {
    let mut params = CertificateParams::default();
    for san in sans {
        let (kind, value) = san.split_once(':').expect("KIND:value");
        params.subject_alt_names.push(match kind {
            "DNS" => SanType::DnsName(value.try_into().expect("IA5")),
            "IP" => SanType::IpAddress(value.parse().expect("an address")),
            "URI" => SanType::URI(value.try_into().expect("IA5")),
            _ => SanType::Rfc822Name(value.try_into().expect("IA5")),
        });
    }
    params.is_ca = IsCa::ExplicitNoCa;
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    params.extended_key_usages = ekus;
    params.use_authority_key_identifier_extension = true;
    edit(&mut params);
    params
        .signed_by(&KeyPair::generate().expect("key"), ca)
        .expect("signed")
        .pem()
}

fn row_ekus(row: Row) -> Vec<Eku> {
    row.ekus()
        .iter()
        .map(|e| {
            if *e == SERVER_AUTH {
                Eku::ServerAuth
            } else {
                Eku::ClientAuth
            }
        })
        .collect()
}

/// The certificate of `row` as the issuer makes it: a daemon's names only its node id.
fn issued(ca: &Ca, row: Row) -> String {
    let role = format!("URI:{ROLE_URI_PREFIX}{}", row.role());
    let names: &[&str] = if row == Row::Daemon {
        &["DNS:node-1"]
    } else {
        &["DNS:frontend.farm.example", "IP:192.0.2.10"]
    };
    let sans: Vec<&str> = names.iter().copied().chain([role.as_str()]).collect();
    leaf(ca, &sans, row_ekus(row), |_| {})
}

fn node_id(row: Row) -> Option<&'static str> {
    (row == Row::Daemon).then_some("node-1")
}

fn check(ca: &Ca, cert: &str, row: Row) -> Vec<String> {
    verify(
        ca.pem().as_bytes(),
        cert.as_bytes(),
        row,
        node_id(row),
        SystemTime::now(),
    )
    .expect("checked")
    .iter()
    .map(ToString::to_string)
    .collect()
}

/// Catches a row's certificate refused (each role and EKU set as the issuer's table
/// has them), and any row's certificate passed as another row's: a frontend's for a
/// storage node's (the same EKU set, told apart by role alone), a daemon's for
/// anything that serves.
#[test]
fn each_rows_certificate_passes_as_its_row_and_no_other() {
    let ca = ca();
    for row in Row::ALL {
        let cert = issued(&ca, row);
        assert_eq!(check(&ca, &cert, row), Vec::<String>::new(), "{row:?}");
        for other in Row::ALL.into_iter().filter(|o| *o != row) {
            assert_ne!(
                check(&ca, &cert, other),
                Vec::<String>::new(),
                "{row:?} passed as {other:?}"
            );
        }
    }
    assert_eq!(
        Row::ALL.map(|r| (r.role(), r.ekus().join(","))),
        [
            ("frontend", "serverAuth,clientAuth".to_owned()),
            ("storage", "serverAuth,clientAuth".to_owned()),
            ("daemon", "clientAuth".to_owned()),
        ]
    );
}

/// Catches a certificate with no role, two roles (even two of the row's), another
/// role, or the role under another prefix passed, and a name of another kind (an
/// email address) read as a role.
#[test]
fn a_certificate_names_exactly_the_rows_one_role() {
    let ca = ca();
    let ekus = || row_ekus(Row::Storage);
    let dns = "DNS:storage-1";
    let rows = [
        (
            vec![dns],
            "FAIL role: it names 0 roles (urn:kbf:role:<role>), not exactly one",
        ),
        (
            vec![dns, "EMAIL:urn:kbf:role:storage@x"],
            "FAIL role: it names 0 roles (urn:kbf:role:<role>), not exactly one",
        ),
        (
            vec![dns, "URI:urn:kbf:storage"],
            "FAIL role: it names 0 roles (urn:kbf:role:<role>), not exactly one",
        ),
        (
            vec![dns, "URI:urn:kbf:role:storage", "URI:urn:kbf:role:storage"],
            "FAIL role: it names 2 roles (urn:kbf:role:<role>), not exactly one",
        ),
        (
            vec![dns, "URI:urn:kbf:role:frontend"],
            "FAIL role: it names the role urn:kbf:role:frontend, not urn:kbf:role:storage",
        ),
    ];
    for (sans, want) in rows {
        let cert = leaf(&ca, &sans, ekus(), |_| {});
        assert_eq!(check(&ca, &cert, Row::Storage), [want], "{sans:?}");
    }
}

/// Catches an EKU set compared as a subset (an extra use passed: a serving certificate
/// passed for a daemon; or a missing one: a server-only certificate passed for the
/// worker listener), a certificate with no EKU extension passed (TLS takes it for every
/// use; the table does not), and an EKU outside the two shown unnamed.
#[test]
fn the_eku_set_is_the_rows_exactly() {
    let ca = ca();
    let daemon = ["DNS:node-1", "URI:urn:kbf:role:daemon"];
    let serving = leaf(&ca, &daemon, row_ekus(Row::FrontendWorker), |_| {});
    assert_eq!(
        check(&ca, &serving, Row::Daemon),
        ["FAIL eku: its extended key usage is serverAuth,clientAuth, not clientAuth"]
    );
    let names = ["DNS:frontend.farm.example", "URI:urn:kbf:role:frontend"];
    let server_only = leaf(&ca, &names, vec![Eku::ServerAuth], |_| {});
    let lines = check(&ca, &server_only, Row::FrontendWorker);
    assert_eq!(
        lines[0],
        "FAIL eku: its extended key usage is serverAuth, not serverAuth,clientAuth"
    );
    // And a TLS client refuses to present it.
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert!(lines[1].starts_with("FAIL chain: it does not chain to the CA for clientAuth: "));
    let none = leaf(&ca, &names, Vec::new(), |_| {});
    assert_eq!(
        check(&ca, &none, Row::FrontendWorker),
        ["FAIL eku: its extended key usage is none, not serverAuth,clientAuth"]
    );
    let every = vec![
        Eku::Any,
        Eku::ServerAuth,
        Eku::ClientAuth,
        Eku::CodeSigning,
        Eku::EmailProtection,
        Eku::TimeStamping,
        Eku::OcspSigning,
        Eku::Other(vec![1, 3, 6, 1, 4, 1, 99999, 1]),
    ];
    let extra = leaf(&ca, &names, every, |_| {});
    assert_eq!(
        check(&ca, &extra, Row::FrontendWorker),
        [
            "FAIL eku: its extended key usage is serverAuth,clientAuth,anyExtendedKeyUsage,codeSigning,emailProtection,timeStamping,OCSPSigning,1.3.6.1.4.1.99999.1, not serverAuth,clientAuth"
        ]
    );
}

/// Catches a daemon's certificate that names another node, a second name, an address
/// or no name at all passed (kbf binds a daemon's certificate to its one DNS name).
#[test]
fn a_daemons_certificate_names_its_node_id_alone() {
    let ca = ca();
    let role = "URI:urn:kbf:role:daemon";
    let ekus = || row_ekus(Row::Daemon);
    let rows = [
        (vec!["DNS:node-2", role], "node-2"),
        (
            vec!["DNS:node-1", "DNS:node-1.farm.example", role],
            "node-1 node-1.farm.example",
        ),
        (
            vec!["DNS:node-1", "IP:2001:db8::1", role],
            "node-1 2001:db8::1",
        ),
        (vec![role], "no DNS name or address"),
    ];
    for (sans, got) in rows {
        let cert = leaf(&ca, &sans, ekus(), |_| {});
        assert_eq!(
            check(&ca, &cert, Row::Daemon),
            [format!(
                "FAIL name: it names {got}, not exactly the node id node-1 (a daemon's one DNS name)"
            )],
            "{sans:?}"
        );
    }
}

/// Catches the chain not checked (another CA's certificate passed, for each use of the
/// row), the validity period not checked (an expired certificate passed), and a
/// serving certificate with no name a client could check passed; an IPv6 name is one.
#[test]
fn the_certificate_chains_to_the_ca_now_for_each_use() {
    let ca = ca();
    let other = issued(&self::ca(), Row::Storage);
    let lines = check(&ca, &other, Row::Storage);
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert!(lines[0].starts_with("FAIL chain: it does not chain to the CA for serverAuth: "));
    assert!(lines[1].starts_with("FAIL chain: it does not chain to the CA for clientAuth: "));
    let names = ["DNS:storage-1", "URI:urn:kbf:role:storage"];
    let expired = leaf(&ca, &names, row_ekus(Row::Storage), |p| {
        p.not_before = rcgen::date_time_ymd(2026, 9, 1);
        p.not_after = rcgen::date_time_ymd(2026, 9, 2);
    });
    let lines = check(&ca, &expired, Row::Storage);
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert!(lines.iter().all(|l| l.contains("expired")), "{lines:?}");
    let worker = || row_ekus(Row::FrontendWorker);
    let nameless = leaf(&ca, &["URI:urn:kbf:role:frontend"], worker(), |_| {});
    assert_eq!(
        check(&ca, &nameless, Row::FrontendWorker),
        ["FAIL chain: it names no DNS name or address, so no TLS client can check it as a server"]
    );
    let v6 = leaf(
        &ca,
        &["IP:2001:db8::1", "URI:urn:kbf:role:frontend"],
        worker(),
        |_| {},
    );
    assert_eq!(check(&ca, &v6, Row::FrontendWorker), Vec::<String>::new());
}

/// Catches what cannot be checked read as a broken certificate (exit 1) instead of
/// refused: no certificate, a block that is not one, a CA file with none or a bad one,
/// and `--node-id` for a row that is not a daemon's.
#[test]
fn what_cannot_be_checked_is_refused() {
    let ca = ca();
    let cert = issued(&ca, Row::Storage);
    let junk = "-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n";
    let torn = "-----BEGIN CERTIFICATE-----\nAAAA\n";
    let run = |ca_pem: &str, cert_pem: &str, row: Row, id: Option<&str>| {
        verify(
            ca_pem.as_bytes(),
            cert_pem.as_bytes(),
            row,
            id,
            SystemTime::now(),
        )
        .map(|f| f.len())
        .map_err(|e| e.to_string())
    };
    let ca_pem = ca.pem();
    assert_eq!(run(&ca_pem, &cert, Row::Storage, None), Ok(0));
    assert_eq!(
        run(&ca_pem, &cert, Row::Storage, Some("node-1")),
        Err("--node-id is for --row daemon only".to_owned())
    );
    assert_eq!(
        run(&ca_pem, "", Row::Storage, None),
        Err("the certificate cannot be read: no CERTIFICATE block".to_owned())
    );
    for bad in [junk, torn] {
        let got = run(&ca_pem, bad, Row::Storage, None).expect_err("refused");
        assert!(got.starts_with("the certificate cannot be read: "), "{got}");
        let got = run(bad, &cert, Row::Storage, None).expect_err("refused");
        assert!(
            got.starts_with("the CA certificate holds no usable certificate: "),
            "{got}"
        );
    }
    let got = run("", &cert, Row::Storage, None).expect_err("refused");
    assert!(
        got.starts_with("the CA certificate holds no usable certificate: "),
        "{got}"
    );
}

/// Catches the verb printing OK for a broken certificate or exiting 0 for one, an
/// unreadable file or one with no certificate not refused (exit 2), and the OK line not
/// naming the row's role and uses.
#[test]
fn the_verb_prints_its_verdict_and_exits_by_it() {
    let dir = kbf_host::testkit::scratch("linux_verify_cert");
    let ca = ca();
    std::fs::write(dir.join("ca.pem"), ca.pem()).expect("write");
    std::fs::write(dir.join("storage.pem"), issued(&ca, Row::Storage)).expect("write");
    let args = |row: Row, cert: &str| VerifyArgs {
        row,
        cert: dir.join(cert),
        ca_cert: dir.join("ca.pem"),
        node_id: node_id(row).map(str::to_owned),
    };
    let verb = |a: &VerifyArgs| {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = cli(a, &mut out, &mut err);
        (
            code,
            String::from_utf8(out).expect("utf-8"),
            String::from_utf8(err).expect("utf-8"),
        )
    };
    let (code, out, err) = verb(&args(Row::Storage, "storage.pem"));
    assert_eq!((code, err.as_str()), (ExitCode::SUCCESS, ""));
    assert_eq!(
        out,
        format!(
            "CERT OK: {} is a storage certificate: role urn:kbf:role:storage, extended key usage serverAuth,clientAuth, chains to {}\n",
            dir.join("storage.pem").display(),
            dir.join("ca.pem").display()
        )
    );
    let (code, out, _) = verb(&args(Row::FrontendWorker, "storage.pem"));
    assert_eq!(code, ExitCode::from(1));
    assert_eq!(
        out,
        "FAIL role: it names the role urn:kbf:role:storage, not urn:kbf:role:frontend\nCERT BROKEN: 1 failed check(s)\n"
    );
    std::fs::write(dir.join("junk.pem"), "no certificate here\n").expect("write");
    let (code, out, err) = verb(&args(Row::Storage, "junk.pem"));
    assert_eq!(
        (code, out.as_str(), err.as_str()),
        (
            ExitCode::from(2),
            "",
            "REFUSED: the certificate cannot be read: no CERTIFICATE block\n"
        )
    );
    let (code, out, err) = verb(&args(Row::Daemon, "absent.pem"));
    assert_eq!((code, out.as_str()), (ExitCode::from(2), ""));
    assert!(
        err.starts_with(&format!(
            "REFUSED: cannot read {}: ",
            dir.join("absent.pem").display()
        )),
        "{err}"
    );
}
