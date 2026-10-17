//! `kbf.map.v1.Membership` on the worker listener of a real `kbf frontend` under mutual
//! TLS with a state store: the path the binary takes (`start_frontend`), not the
//! `bind_server_with_state` of `worker_roles.rs`, which has its own wiring. Whatever
//! admission the service is built with there, this is where the frontend builds it, so
//! these tests fail if the frontend hands the service no certificate rules.

use kbf_proto::kbf::map::v1 as pb;
use kbf_proto::kbf::map::v1::membership_client::MembershipClient;
use tonic::Code;
use tonic::transport::{Certificate, ClientTlsConfig, Endpoint, Identity};

use super::*;

/// The flags of a frontend whose worker listener serves mutual TLS (a `localhost` server
/// certificate, clients chaining to `pki`'s CA) over a SQLite state store at `state`, and
/// these more.
fn tls_flags(pki: &Pki, state: &Path, more: &[String]) -> FrontendFlags {
    let mut params = rcgen::CertificateParams::new(vec!["localhost".to_owned()]).expect("params");
    params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
    let key = rcgen::KeyPair::generate().expect("key");
    let cert = params.signed_by(&key, &pki.ca).expect("sign");
    let mut argv = vec![
        "--storage=http://127.0.0.1:1".to_owned(),
        format!("--state-store=sqlite://{}", state.display()),
        format!("--worker-tls-cert={}", pki.write("worker.pem", cert.pem())),
        format!(
            "--worker-tls-key={}",
            pki.write("worker.key", key.serialize_pem())
        ),
        format!("--worker-client-ca={}", pki.ca_file()),
    ];
    argv.extend_from_slice(more);
    flags(&argv.iter().map(String::as_str).collect::<Vec<_>>()).expect("flags")
}

/// A storage node's connection to the worker listener at `worker`: the certificate it
/// presents names the role `storage` and carries exactly the DNS `names`.
async fn storage_node(pki: &Pki, worker: SocketAddr, names: &[&str]) -> MembershipClient<Channel> {
    let mut params = rcgen::CertificateParams::new(
        names
            .iter()
            .map(|name| (*name).to_owned())
            .collect::<Vec<_>>(),
    )
    .expect("params");
    params.subject_alt_names.push(rcgen::SanType::URI(
        format!("{}storage", kbf_storage::service::ROLE_URI_PREFIX)
            .try_into()
            .expect("an IA5 name"),
    ));
    params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ClientAuth];
    let key = rcgen::KeyPair::generate().expect("key");
    let cert = params.signed_by(&key, &pki.ca).expect("sign");
    let tls = ClientTlsConfig::new()
        .ca_certificate(Certificate::from_pem(pki.ca.pem()))
        .domain_name("localhost")
        .identity(Identity::from_pem(cert.pem(), key.serialize_pem()));
    let channel = Endpoint::from_shared(format!("https://{worker}"))
        .expect("endpoint")
        .tls_config(tls)
        .expect("TLS")
        .connect()
        .await
        .expect("connect");
    MembershipClient::new(channel)
}

/// A registration of the storage node `instance`, dialled at `addr`.
fn register(instance: &str, addr: &str) -> pb::RegisterRequest {
    pb::RegisterRequest {
        role: pb::Role::Storage.into(),
        instance_id: instance.to_owned(),
        addr: addr.to_owned(),
        weight: 1,
        protocol: kbf_server::members::PROTOCOL,
        ..pb::RegisterRequest::default()
    }
}

fn beat(joined: &pb::RegisterResponse, boot: u64) -> pb::HeartbeatRequest {
    pb::HeartbeatRequest {
        member_id: joined.member_id,
        boot,
        ..pb::HeartbeatRequest::default()
    }
}

/// Catches a frontend that builds its membership service without the certificate rules
/// (the service built as for plain text, whatever the worker listener's TLS): then a
/// storage certificate registers any instance id and address, and beats any member.
/// Over the worker listener of a started frontend, node A's certificate (`node-a`) is
/// refused PERMISSION_DENIED when it registers `node-b` as the instance id, and when it
/// registers `node-a` but advertises `node-b`; both name the value refused. Neither
/// writes a member, so the registration it is then admitted for, its own, is member 1.
/// Node B's certificate (`node-b`) is member 2, and its beat for A's member, with A's
/// boot and with another, is PERMISSION_DENIED; A's own beat is admitted.
///
/// Mutant seen red: the frontend's membership service built open (`Peers::Unauthenticated`
/// handed to it whatever the listener's TLS): A's first registration is admitted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_frontend_binds_a_storage_certificate_to_the_names_it_carries() {
    let pki = Pki::new("membership-names");
    let state = scratch_dir("membership-names").join("state.db");
    let front = Front::start(&tls_flags(&pki, &state, &[])).await;
    let mut a = storage_node(&pki, front.worker, &["node-a"]).await;
    let mut b = storage_node(&pki, front.worker, &["node-b"]).await;

    for (instance, addr, offending) in [
        ("node-b:8982", "node-a:8982", "node-b:8982"),
        ("node-a:8982", "node-b:8982", "node-b:8982"),
    ] {
        let status = a
            .register(register(instance, addr))
            .await
            .expect_err(offending);
        assert_eq!(status.code(), Code::PermissionDenied, "{instance} {addr}");
        assert!(status.message().contains(offending), "{}", status.message());
    }
    let first = a
        .register(register("node-a:8982", "node-a:8982"))
        .await
        .expect("A registers what its certificate names")
        .into_inner();
    assert_eq!(first.member_id, 1, "no refusal wrote a member");
    let second = b
        .register(register("node-b:8982", "node-b:8982"))
        .await
        .expect("B registers what its certificate names")
        .into_inner();
    assert_eq!(second.member_id, 2);

    for boot in [first.boot, first.boot + 7] {
        let status = b
            .heartbeat(beat(&first, boot))
            .await
            .expect_err("B beats A's member");
        assert_eq!(status.code(), Code::PermissionDenied, "boot {boot}");
    }
    a.heartbeat(beat(&first, first.boot))
        .await
        .expect("A beats its own member");
    b.heartbeat(beat(&second, second.boot))
        .await
        .expect("B beats its own member");
}

/// Catches a frontend that reads `--worker-deny-list` for its daemons and not for the
/// storage nodes calling the membership service on the same listener. The list starts
/// empty, and A (`node-a`) and B (`node-b`) register and beat. The list is then replaced
/// by one that names the node `node-a`: A's certificate is refused `Register` and
/// `Heartbeat` PERMISSION_DENIED, saying `denied: node node-a`, and B goes on.
///
/// Mutant seen red: the frontend's membership service built with no deny list whatever
/// the listener has one: A is admitted after the list names it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_frontend_applies_its_deny_list_to_storage_certificates() {
    let pki = Pki::new("membership-deny");
    let dir = scratch_dir("membership-deny");
    let list = dir.join("deny.list");
    let write_list = |text: &str| {
        // Replaced by a rename, as the list's module docs ask, so the next check sees it.
        let staged = list.with_extension("new");
        std::fs::write(&staged, text).expect("write");
        std::fs::rename(&staged, &list).expect("replace");
    };
    write_list("# nobody yet\n");
    let more = [format!("--worker-deny-list={}", list.display())];
    let front = Front::start(&tls_flags(&pki, &dir.join("state.db"), &more)).await;
    let mut a = storage_node(&pki, front.worker, &["node-a"]).await;
    let mut b = storage_node(&pki, front.worker, &["node-b"]).await;
    let joined_a = a
        .register(register("node-a:8982", "node-a:8982"))
        .await
        .expect("A registers while nothing is listed")
        .into_inner();
    let joined_b = b
        .register(register("node-b:8982", "node-b:8982"))
        .await
        .expect("B registers while nothing is listed")
        .into_inner();
    a.heartbeat(beat(&joined_a, joined_a.boot))
        .await
        .expect("A beats while nothing is listed");

    write_list("node node-a\n");
    let registered = a
        .register(register("node-a:8982", "node-a:8982"))
        .await
        .expect_err("A is listed");
    let beaten = a
        .heartbeat(beat(&joined_a, joined_a.boot))
        .await
        .expect_err("A is listed");
    for status in [registered, beaten] {
        assert_eq!(status.code(), Code::PermissionDenied);
        assert_eq!(status.message(), "denied: node node-a");
    }
    b.heartbeat(beat(&joined_b, joined_b.boot))
        .await
        .expect("B is not listed");
}
