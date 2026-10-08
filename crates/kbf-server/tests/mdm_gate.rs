//! `GateClient` against a fake `kbf-mdm-gate` served over mutual TLS: what each verb
//! sends, how each answer is checked, and that the client reaches only a gate whose
//! certificate it trusts.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use kbf_mdm_api::names::{Date, LocalDateTime, OsVersion, Serial, Sha256Hex};
use kbf_mdm_api::pb::{
    self, enforce_response, mdm_gate_server::MdmGate as GateService,
    mdm_gate_server::MdmGateServer, profile_response, withdraw_response,
};
use kbf_server::mdm::{
    EnforceOrder, Enforcement, GateClient, GateEndpoint, GateError, MdmGate, RefusalReason,
};
use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, DnType, ExtendedKeyUsagePurpose, IsCa,
    KeyPair, KeyUsagePurpose,
};
use tonic::transport::server::TcpIncoming;
use tonic::transport::{Certificate, Identity, Server, ServerTlsConfig};
use tonic::{Request, Response, Status};

/// A certificate authority and leaves it signs.
struct Ca(CertifiedIssuer<'static, KeyPair>);

impl Ca {
    fn new(name: &str) -> Self {
        let mut params = CertificateParams::new(Vec::<String>::new()).expect("CA params");
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.distinguished_name.push(DnType::CommonName, name);
        params.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::DigitalSignature,
        ];
        Self(CertifiedIssuer::self_signed(params, KeyPair::generate().expect("key")).expect("CA"))
    }

    fn pem(&self) -> String {
        self.0.pem()
    }

    /// A leaf for `name`: (certificate, key), PEM.
    fn leaf(&self, name: &str, eku: ExtendedKeyUsagePurpose) -> (String, String) {
        let mut params = CertificateParams::new(vec![name.to_owned()]).expect("leaf params");
        params.distinguished_name.push(DnType::CommonName, name);
        params.extended_key_usages = vec![eku];
        let key = KeyPair::generate().expect("key");
        let cert = params.signed_by(&key, &self.0).expect("sign");
        (cert.pem(), key.serialize_pem())
    }
}

/// The answers the fake gives, and the requests it saw.
#[derive(Default)]
struct Script {
    status: Option<pb::StatusResponse>,
    enforce: Option<pb::EnforceResponse>,
    withdraw: Option<pb::WithdrawResponse>,
    profile: Option<pb::ProfileResponse>,
    seen: Vec<String>,
}

/// A gate that answers from its script; a verb with no scripted answer fails with
/// `UNAVAILABLE`.
#[derive(Clone, Default)]
struct FakeGate(Arc<Mutex<Script>>);

impl FakeGate {
    fn script(&self) -> std::sync::MutexGuard<'_, Script> {
        self.0.lock().expect("script")
    }

    fn answer<T: Clone>(
        &self,
        seen: String,
        pick: impl Fn(&Script) -> Option<T>,
    ) -> Result<Response<T>, Status> {
        let mut script = self.script();
        script.seen.push(seen);
        pick(&script)
            .map(Response::new)
            .ok_or_else(|| Status::unavailable("no answer scripted"))
    }
}

#[tonic::async_trait]
impl GateService for FakeGate {
    async fn status(
        &self,
        r: Request<pb::StatusRequest>,
    ) -> Result<Response<pb::StatusResponse>, Status> {
        self.answer(format!("status {:?}", r.get_ref().serials), |s| {
            s.status.clone()
        })
    }
    async fn enforce(
        &self,
        r: Request<pb::EnforceRequest>,
    ) -> Result<Response<pb::EnforceResponse>, Status> {
        let r = r.into_inner();
        let seen = format!(
            "enforce {} {} {}",
            r.serial,
            String::from_utf8_lossy(&r.signed_set),
            r.target_local_date_time
        );
        self.answer(seen, |s| s.enforce.clone())
    }
    async fn withdraw(
        &self,
        r: Request<pb::WithdrawRequest>,
    ) -> Result<Response<pb::WithdrawResponse>, Status> {
        self.answer(format!("withdraw {}", r.get_ref().serial), |s| {
            s.withdraw.clone()
        })
    }
    async fn profile(
        &self,
        r: Request<pb::ProfileRequest>,
    ) -> Result<Response<pb::ProfileResponse>, Status> {
        let r = r.into_inner();
        self.answer(format!("profile {} {}", r.serial, r.sha256), |s| {
            s.profile.clone()
        })
    }
}

/// A fake gate on a loopback port, requiring a client certificate from `ca`; and a
/// client endpoint for it that trusts `ca`.
struct Harness {
    gate: FakeGate,
    endpoint: GateEndpoint,
    addr: SocketAddr,
}

async fn harness() -> Harness {
    let ca = Ca::new("kbf test CA");
    let (gate_cert, gate_key) = ca.leaf("localhost", ExtendedKeyUsagePurpose::ServerAuth);
    let (server_cert, server_key) = ca.leaf("kbf-server", ExtendedKeyUsagePurpose::ClientAuth);
    let tls = ServerTlsConfig::new()
        .identity(Identity::from_pem(gate_cert, gate_key))
        .client_ca_root(Certificate::from_pem(ca.pem()));
    let incoming = TcpIncoming::bind(SocketAddr::from(([127, 0, 0, 1], 0))).expect("bind");
    let addr = incoming.local_addr().expect("addr");
    let gate = FakeGate::default();
    let service = MdmGateServer::new(gate.clone());
    tokio::spawn(
        Server::builder()
            .tls_config(tls)
            .expect("gate TLS")
            .add_service(service)
            .serve_with_incoming(incoming),
    );
    let endpoint = GateEndpoint {
        url: format!("https://{addr}"),
        domain: "localhost".to_owned(),
        ca_pem: ca.pem().into_bytes(),
        cert_pem: server_cert.into_bytes(),
        key_pem: server_key.into_bytes(),
        timeout: Duration::from_secs(10),
    };
    Harness {
        gate,
        endpoint,
        addr,
    }
}

fn serial(s: &str) -> Serial {
    Serial::new(s).expect("serial")
}

fn pb_enforcement(serial: &str, build: &str, at: &str) -> pb::Enforcement {
    pb::Enforcement {
        declaration_identifier: format!("kbf.osupdate.{serial}"),
        target_os_version: "27.0.1".to_owned(),
        target_build: build.to_owned(),
        target_local_date_time: at.to_owned(),
    }
}

fn enforcement(serial: &str, build: &str, at: &str) -> Enforcement {
    Enforcement {
        declaration_identifier: format!("kbf.osupdate.{serial}"),
        target_os_version: OsVersion::parse("27.0.1").unwrap(),
        target_build: build.to_owned(),
        target_local_date_time: LocalDateTime::parse(at).unwrap(),
    }
}

fn refusal(reason: i32, detail: &str) -> pb::Refusal {
    pb::Refusal {
        reason,
        detail: detail.to_owned(),
    }
}

const AT: &str = "2026-10-08T14:05:00";

fn order() -> EnforceOrder {
    EnforceOrder {
        serial: serial("C02X"),
        signed_set: b"signed-set-bytes".to_vec(),
        by: LocalDateTime::parse(AT).unwrap(),
    }
}

/// Catches: the inventory's fields dropped or crossed, the asked-for serials not sent,
/// "never" (0) read as the epoch, a missing catalogue or update status refused.
#[tokio::test]
async fn status_reads_the_inventory_and_catalogue() {
    let h = harness().await;
    let full = pb::MacStatus {
        serial: "C02X".to_owned(),
        platform_uuid: "uuid-1".to_owned(),
        pool: "mac-arm64".to_owned(),
        enrolled: true,
        supervised: true,
        bootstrap_token_escrowed: true,
        last_check_in_unix_ms: 1_791_462_896_000,
        os_version: "27.0".to_owned(),
        os_build: "26A400".to_owned(),
        supplemental_build: "26A400a".to_owned(),
        software_update: Some(pb::SoftwareUpdateStatus {
            install_state: "downloading".to_owned(),
            pending_os_version: "27.0.1".to_owned(),
            pending_build: "26A434".to_owned(),
            failure_reason: String::new(),
            failure_count: 0,
            install_reasons: vec!["declaration".to_owned()],
        }),
        profiles: vec![pb::InstalledProfile {
            identifier: "kbf.fda".to_owned(),
            sha256: "ab".to_owned(),
        }],
        enforcement: Some(pb_enforcement("C02X", "26A434", AT)),
    };
    let bare = pb::MacStatus {
        serial: "C02Y".to_owned(),
        ..pb::MacStatus::default()
    };
    h.gate.script().status = Some(pb::StatusResponse {
        macs: vec![full, bare],
        catalogue: Some(pb::Catalogue {
            fetched_at_unix_ms: 5,
            entries: vec![pb::CatalogueEntry {
                product_version: "27.0.1".to_owned(),
                build: "26A434".to_owned(),
                posting_date: "2026-09-28".to_owned(),
                expiration_date: "2027-01-06".to_owned(),
                supported_devices: vec!["J274AP".to_owned()],
                public: true,
            }],
        }),
    });
    let client = GateClient::new(&h.endpoint).unwrap();
    let inv = client
        .status(&[serial("C02X"), serial("C02Y")])
        .await
        .unwrap();
    assert_eq!(
        h.gate.script().seen,
        vec![r#"status ["C02X", "C02Y"]"#.to_owned()]
    );

    let x = inv.mac(&serial("C02X")).unwrap();
    assert_eq!(
        (x.platform_uuid.as_str(), x.pool.as_str()),
        ("uuid-1", "mac-arm64")
    );
    assert!(x.enrolled && x.supervised && x.bootstrap_token_escrowed);
    assert_eq!(x.last_check_in_unix_ms, Some(1_791_462_896_000));
    assert_eq!(
        (
            x.os_version.as_str(),
            x.os_build.as_str(),
            x.supplemental_build.as_str()
        ),
        ("27.0", "26A400", "26A400a")
    );
    assert_eq!(x.software_update.install_state, "downloading");
    assert_eq!(x.software_update.pending_build, "26A434");
    assert_eq!(
        x.software_update.install_reasons,
        vec!["declaration".to_owned()]
    );
    assert_eq!(x.profiles[0].identifier, "kbf.fda");
    assert_eq!(x.enforcement, Some(enforcement("C02X", "26A434", AT)));

    let y = inv.mac(&serial("C02Y")).unwrap();
    assert!(!y.enrolled && !y.supervised && !y.bootstrap_token_escrowed);
    assert_eq!((y.last_check_in_unix_ms, &y.enforcement), (None, &None));
    assert_eq!(y.software_update, pb::SoftwareUpdateStatus::default());
    assert_eq!(inv.mac(&serial("C02Z")), None);

    assert_eq!(inv.catalogue.fetched_at_unix_ms, Some(5));
    let e = &inv.catalogue.entries[0];
    assert_eq!(
        (e.product_version.as_str(), e.build.as_str(), e.public),
        ("27.0.1", "26A434", true)
    );
    assert_eq!(e.posting_date, Date::parse("2026-09-28").unwrap());
    assert_eq!(e.expiration_date, Date::parse("2027-01-06").unwrap());
    assert_eq!(e.supported_devices, vec!["J274AP".to_owned()]);

    // A gate that never read the catalogue sends none.
    h.gate.script().status = Some(pb::StatusResponse::default());
    let inv = client.status(&[]).await.unwrap();
    assert_eq!(
        (inv.catalogue.fetched_at_unix_ms, inv.macs.len()),
        (None, 0)
    );
    assert_eq!(h.gate.script().seen[1], "status []");
}

/// Catches: an answer taken on trust: a malformed serial, an enforcement under
/// another Mac's or a non-kbf identifier, an unreadable build, version or date-time,
/// or a malformed catalogue entry passed on as data.
#[tokio::test]
async fn a_malformed_status_answer_is_refused() {
    let h = harness().await;
    let client = GateClient::new(&h.endpoint).unwrap();
    let mac = |serial: &str, e: Option<pb::Enforcement>| pb::MacStatus {
        serial: serial.to_owned(),
        enforcement: e,
        ..pb::MacStatus::default()
    };
    let good_entry = pb::CatalogueEntry {
        product_version: "27.0.1".to_owned(),
        build: "26A434".to_owned(),
        posting_date: "2026-09-28".to_owned(),
        expiration_date: "2027-01-06".to_owned(),
        ..pb::CatalogueEntry::default()
    };
    let with_entry = |e: pb::CatalogueEntry| pb::StatusResponse {
        macs: vec![],
        catalogue: Some(pb::Catalogue {
            fetched_at_unix_ms: 1,
            entries: vec![e],
        }),
    };
    let only = |m: pb::MacStatus| pb::StatusResponse {
        macs: vec![m],
        catalogue: None,
    };
    let mut bad_id = pb_enforcement("C02X", "26A434", AT);
    bad_id.declaration_identifier = "com.example.osupdate.C02X".to_owned();
    let mut bad_version = pb_enforcement("C02X", "26A434", AT);
    bad_version.target_os_version = "27.x".to_owned();
    let cases = [
        (only(mac("C02.X", None)), "serial"),
        (
            only(mac("C02X", Some(pb_enforcement("C02Y", "26A434", AT)))),
            "kbf.osupdate.C02Y",
        ),
        (only(mac("C02X", Some(bad_id))), "com.example"),
        (
            only(mac("C02X", Some(pb_enforcement("C02X", "", AT)))),
            "target build",
        ),
        (only(mac("C02X", Some(bad_version))), "macOS version"),
        (
            only(mac(
                "C02X",
                Some(pb_enforcement("C02X", "26A434", "2026-10-08 14:05")),
            )),
            "local date-time",
        ),
        (
            with_entry(pb::CatalogueEntry {
                product_version: "x".to_owned(),
                ..good_entry.clone()
            }),
            "macOS version",
        ),
        (
            with_entry(pb::CatalogueEntry {
                build: "26 A".to_owned(),
                ..good_entry.clone()
            }),
            "build",
        ),
        (
            with_entry(pb::CatalogueEntry {
                posting_date: "2026-9-28".to_owned(),
                ..good_entry.clone()
            }),
            "2026-9-28",
        ),
        (
            with_entry(pb::CatalogueEntry {
                expiration_date: "2027-02-30".to_owned(),
                ..good_entry.clone()
            }),
            "2027-02-30",
        ),
    ];
    for (answer, want) in cases {
        h.gate.script().status = Some(answer.clone());
        match client.status(&[]).await {
            Err(GateError::Malformed(m)) => assert!(m.contains(want), "{m} lacks {want}"),
            other => panic!("{answer:?}: {other:?}"),
        }
    }
    h.gate.script().status = Some(with_entry(good_entry));
    assert_eq!(client.status(&[]).await.unwrap().catalogue.entries.len(), 1);
}

/// Catches: the server sending a build of its own instead of the signed set, the
/// deadline or serial not sent, an enforcement for another deadline or Mac accepted,
/// a refusal read as success, and an answer with no outcome taken as success.
#[tokio::test]
async fn enforce_sends_the_signed_set_and_checks_what_was_posted() {
    let h = harness().await;
    let client = GateClient::new(&h.endpoint).unwrap();
    let posted = |e: pb::Enforcement| pb::EnforceResponse {
        outcome: Some(enforce_response::Outcome::Enforced(e)),
    };

    h.gate.script().enforce = Some(posted(pb_enforcement("C02X", "26A434", AT)));
    assert_eq!(
        client.enforce(&order()).await,
        Ok(enforcement("C02X", "26A434", AT))
    );
    assert_eq!(
        h.gate.script().seen,
        vec![format!("enforce C02X signed-set-bytes {AT}")]
    );

    h.gate.script().enforce = Some(posted(pb_enforcement(
        "C02X",
        "26A434",
        "2026-10-08T15:00:00",
    )));
    match client.enforce(&order()).await {
        Err(GateError::Malformed(m)) => assert!(m.contains("2026-10-08T15:00:00"), "{m}"),
        other => panic!("{other:?}"),
    }
    h.gate.script().enforce = Some(posted(pb_enforcement("C02Y", "26A434", AT)));
    assert!(matches!(
        client.enforce(&order()).await,
        Err(GateError::Malformed(_))
    ));

    h.gate.script().enforce = Some(pb::EnforceResponse {
        outcome: Some(enforce_response::Outcome::Refused(refusal(
            RefusalReason::EnforcementOutstanding as i32,
            "C02Z is updating",
        ))),
    });
    assert_eq!(
        client.enforce(&order()).await,
        Err(GateError::Refused {
            reason: RefusalReason::EnforcementOutstanding,
            detail: "C02Z is updating".to_owned()
        })
    );
    h.gate.script().enforce = Some(pb::EnforceResponse { outcome: None });
    assert_eq!(
        client.enforce(&order()).await,
        Err(GateError::Malformed(
            "the enforce answer has no outcome".to_owned()
        ))
    );
}

/// Catches: withdraw reporting nothing removed when something was, a malformed
/// removed enforcement accepted, a refusal or an empty answer read as success.
#[tokio::test]
async fn withdraw_reports_what_it_removed() {
    let h = harness().await;
    let client = GateClient::new(&h.endpoint).unwrap();
    let withdrawn = |e: Option<pb::Enforcement>| pb::WithdrawResponse {
        outcome: Some(withdraw_response::Outcome::Withdrawn(pb::Withdrawn {
            removed: e,
        })),
    };
    h.gate.script().withdraw = Some(withdrawn(Some(pb_enforcement("C02X", "26A434", AT))));
    assert_eq!(
        client.withdraw(&serial("C02X")).await,
        Ok(Some(enforcement("C02X", "26A434", AT)))
    );
    assert_eq!(h.gate.script().seen, vec!["withdraw C02X".to_owned()]);

    h.gate.script().withdraw = Some(withdrawn(None));
    assert_eq!(client.withdraw(&serial("C02X")).await, Ok(None));

    h.gate.script().withdraw = Some(withdrawn(Some(pb_enforcement("C02Y", "26A434", AT))));
    assert!(matches!(
        client.withdraw(&serial("C02X")).await,
        Err(GateError::Malformed(_))
    ));

    h.gate.script().withdraw = Some(pb::WithdrawResponse {
        outcome: Some(withdraw_response::Outcome::Refused(refusal(
            RefusalReason::UnknownSerial as i32,
            "no C02X",
        ))),
    });
    assert_eq!(
        client.withdraw(&serial("C02X")).await,
        Err(GateError::Refused {
            reason: RefusalReason::UnknownSerial,
            detail: "no C02X".to_owned()
        })
    );
    h.gate.script().withdraw = Some(pb::WithdrawResponse { outcome: None });
    assert_eq!(
        client.withdraw(&serial("C02X")).await,
        Err(GateError::Malformed(
            "the withdraw answer has no outcome".to_owned()
        ))
    );
}

/// Catches: profile bytes or anything but the digest sent, a refusal read as
/// success, and an unknown refusal reason passed on as a known one.
#[tokio::test]
async fn install_profile_names_only_a_digest() {
    let h = harness().await;
    let client = GateClient::new(&h.endpoint).unwrap();
    let digest = Sha256Hex::new("ab".repeat(32)).unwrap();
    h.gate.script().profile = Some(pb::ProfileResponse {
        outcome: Some(profile_response::Outcome::Installed(pb::ProfileInstalled {
            identifier: "kbf.fda".to_owned(),
        })),
    });
    assert_eq!(
        client.install_profile(&serial("C02X"), &digest).await,
        Ok("kbf.fda".to_owned())
    );
    assert_eq!(
        h.gate.script().seen,
        vec![format!("profile C02X {}", "ab".repeat(32))]
    );

    h.gate.script().profile = Some(pb::ProfileResponse {
        outcome: Some(profile_response::Outcome::Refused(refusal(
            RefusalReason::NotAllowlisted as i32,
            "not listed",
        ))),
    });
    assert_eq!(
        client.install_profile(&serial("C02X"), &digest).await,
        Err(GateError::Refused {
            reason: RefusalReason::NotAllowlisted,
            detail: "not listed".to_owned()
        })
    );
    h.gate.script().profile = Some(pb::ProfileResponse {
        outcome: Some(profile_response::Outcome::Refused(refusal(
            99,
            "new reason",
        ))),
    });
    assert_eq!(
        client.install_profile(&serial("C02X"), &digest).await,
        Err(GateError::Refused {
            reason: RefusalReason::Unspecified,
            detail: "new reason".to_owned()
        })
    );
    h.gate.script().profile = Some(pb::ProfileResponse { outcome: None });
    assert_eq!(
        client.install_profile(&serial("C02X"), &digest).await,
        Err(GateError::Malformed(
            "the profile answer has no outcome".to_owned()
        ))
    );
}

/// Catches: a gate failure, on any verb, passed on as an answer.
#[tokio::test]
async fn a_failed_call_is_unavailable() {
    let h = harness().await;
    let client = GateClient::new(&h.endpoint).unwrap();
    let digest = Sha256Hex::new("ab".repeat(32)).unwrap();
    let results = [
        client.status(&[]).await.map(|_| ()),
        client.enforce(&order()).await.map(|_| ()),
        client.withdraw(&serial("C02X")).await.map(|_| ()),
        client
            .install_profile(&serial("C02X"), &digest)
            .await
            .map(|_| ()),
    ];
    for r in results {
        match r {
            Err(GateError::Unavailable(m)) => assert!(m.contains("no answer scripted"), "{m}"),
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(h.gate.script().seen.len(), 4);
}

/// Catches: a client that speaks plain text, or that trusts a gate whose
/// certificate chains to another CA or names another host.
#[tokio::test]
async fn the_client_reaches_only_a_trusted_gate_over_tls() {
    let h = harness().await;
    h.gate.script().status = Some(pb::StatusResponse::default());

    let plain = GateEndpoint {
        url: format!("http://{}", h.addr),
        ..h.endpoint.clone()
    };
    match GateClient::new(&plain) {
        Err(GateError::Unavailable(m)) => assert!(m.contains("not https://"), "{m}"),
        other => panic!("{other:?}"),
    }
    let bad_url = GateEndpoint {
        url: "https://bad host".to_owned(),
        ..h.endpoint.clone()
    };
    assert!(matches!(
        GateClient::new(&bad_url),
        Err(GateError::Unavailable(_))
    ));

    let no_key = GateEndpoint {
        key_pem: b"not a key".to_vec(),
        ..h.endpoint.clone()
    };
    assert!(matches!(
        GateClient::new(&no_key),
        Err(GateError::Unavailable(_))
    ));

    let other_ca = Ca::new("another CA");
    let untrusted = GateEndpoint {
        ca_pem: other_ca.pem().into_bytes(),
        ..h.endpoint.clone()
    };
    let client = GateClient::new(&untrusted).unwrap();
    assert!(matches!(
        client.status(&[]).await,
        Err(GateError::Unavailable(_))
    ));

    let wrong_name = GateEndpoint {
        domain: "gate.example".to_owned(),
        ..h.endpoint.clone()
    };
    let client = GateClient::new(&wrong_name).unwrap();
    assert!(matches!(
        client.status(&[]).await,
        Err(GateError::Unavailable(_))
    ));

    // A client certificate from another CA is refused by the gate.
    let (cert, key) = other_ca.leaf("kbf-server", ExtendedKeyUsagePurpose::ClientAuth);
    let stranger = GateEndpoint {
        cert_pem: cert.into_bytes(),
        key_pem: key.into_bytes(),
        ..h.endpoint.clone()
    };
    let client = GateClient::new(&stranger).unwrap();
    assert!(matches!(
        client.status(&[]).await,
        Err(GateError::Unavailable(_))
    ));
    // The trusted pairing works, and only it reached the fake.
    let client = GateClient::new(&h.endpoint).unwrap();
    assert!(client.status(&[]).await.is_ok());
    assert_eq!(h.gate.script().seen.len(), 1);
    // The endpoint's Debug shows no key material.
    let shown = format!("{:?}", h.endpoint);
    assert!(
        shown.contains("localhost") && !shown.contains("PRIVATE KEY"),
        "{shown}"
    );
}
