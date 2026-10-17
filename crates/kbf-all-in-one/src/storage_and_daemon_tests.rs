//! The one command line of `kbf storage-and-daemon`.

#[cfg(target_os = "linux")]
use clap::Parser as _;
use clap::error::ErrorKind;

use super::*;

/// The flags every command line here gives: the daemon's required ones.
const BASE: &[&str] = &[
    "storage-and-daemon",
    "--server=https://127.0.0.1:8981",
    "--ca-cert=ca.pem",
    "--cert=node.pem",
    "--key=node.key",
    "--node-id=node-1",
    "--driver=container",
];

fn flags(extra: &[&str]) -> Result<Flags, clap::Error> {
    let argv = BASE.iter().chain(extra);
    let matches = command().try_get_matches_from(argv)?;
    parse(&matches)
}

/// Catches a storage flag that never reaches the storage half (its listener, its
/// certificates, its stores), a daemon flag that never reaches the daemon half, and the
/// halves' shared thread flags held twice (clap's own check of the command panics on a
/// name held twice) or handed to the storage half, which sizes itself as `kbf storage`
/// does.
#[test]
fn each_flag_reaches_its_half_once() {
    command().debug_assert();
    let flags = flags(&[
        "--storage-listen=127.0.0.1:9982",
        "--storage-tls-cert=storage.pem",
        "--storage-tls-key=storage.key",
        "--storage-client-ca=ca.pem",
        "--cold-store=file:///var/kbf/cold",
        "--hot-store=file:///var/kbf/hot",
        "--cpu-threads=3",
        "--data-dir=/var/kbf",
    ])
    .expect("flags");
    assert_eq!(flags.storage.listen.to_string(), "127.0.0.1:9982");
    let tls = flags.storage.tls.expect("the three TLS flags");
    assert_eq!(
        tls.map(|p| p.display().to_string()),
        ["storage.pem", "storage.key", "ca.pem"]
    );
    assert_eq!(
        flags.storage.server.threads,
        kbf_threads::PoolArgs::default()
    );
    let daemon = format!("{:?}", flags.daemon);
    assert!(daemon.contains("cpu_threads: Some(3)"), "{daemon}");
    assert!(daemon.contains("\"/var/kbf\""), "{daemon}");
    let storage = format!("{:?}", flags.storage.server);
    assert!(storage.contains("\"/var/kbf/hot\""), "{storage}");
    assert!(storage.contains("\"/var/kbf/cold\""), "{storage}");
}

/// Catches the daemon half holding where the cold store is (its URI, which names the
/// bucket and the endpoint its credential opens) or any store flag: the cold store is
/// the storage half's alone, and the daemon reaches blobs only through the in-process
/// storage. The cold URI given must be in the storage half's flags and in nothing the
/// daemon half holds, and the daemon's command line (`kbf-daemon`'s) takes no store flag.
#[test]
fn the_daemon_half_holds_no_cold_store() {
    let cold = "s3://private-bucket/kbf/?endpoint=http://127.0.0.1:1";
    let flags = flags(&[&format!("--cold-store={cold}")]).expect("flags");
    let storage = format!("{:?}", flags.storage.server);
    assert!(storage.contains("private-bucket"), "{storage}");
    let daemon = format!("{:?}", flags.daemon);
    assert!(!daemon.contains("private-bucket"), "{daemon}");
    for arg in kbf_node::Cli::command().get_arguments() {
        let id = arg.get_id().as_str();
        assert!(
            !["cold_store", "hot_store", "meta_dir"].contains(&id),
            "the daemon's own flags include a store flag: {id}"
        );
    }
}

/// Catches a driver other than the container driver taken, `--cas` taken (a remote data
/// plane beside the storage it would reach over the network), and a flag of
/// `kbf-server` that `kbf storage` refuses (the REAPI listener, the state store) taken
/// here: each refused, named.
#[test]
fn other_drivers_cas_and_server_flags_are_refused() {
    for (extra, says) in [
        (&["--driver=native"][..], "--driver native"),
        (&["--driver=fake"][..], "--driver fake"),
        (&["--cas=https://127.0.0.1:8981"][..], "--cas"),
        (&["--listen=127.0.0.1:8980"][..], "refuses --listen"),
        (
            &["--state-store=sqlite:///var/kbf/s"][..],
            "refuses --state-store",
        ),
    ] {
        let refused = flags(extra).expect_err("refused");
        assert_eq!(refused.kind(), ErrorKind::ArgumentConflict, "{extra:?}");
        assert!(refused.to_string().contains(says), "{says}: {refused}");
    }
}

/// Catches a half started from flags other than the ones given the one cache (the
/// daemon half handed a copy made before the sharing, or the storage half): with the
/// hot store on the data directory's disk, both halves of the flags `serve` takes hold
/// the same cache.
#[cfg(target_os = "linux")]
#[test]
fn both_halves_start_with_the_one_cache() {
    let dir = crate::one_cache::under_target("prepared-storage-and-daemon");
    let at = |name: &str| dir.join(name).display().to_string();
    let hot = format!("--hot-store=file://{}", at("hot"));
    let data = format!("--data-dir={}", at("data"));
    let flags = prepared(&flags(&[&hot, &data]).expect("flags"));
    let local = flags.storage.server.local_cache.as_ref();
    let local = local.expect("the storage half's");
    assert!(crate::one_cache::daemon_holds(local, &flags.daemon));
}

/// Catches the daemon half routing by the map on a node that is the whole farm, or not
/// routing by it on a node that registers with a frontend: with `--frontend` the farm has
/// other members and a blob belongs to the one the map names (writing it to this node
/// whatever the map said is the defect this decides), and without one the node is a map of
/// itself and its blobs stay in-process.
#[cfg(target_os = "linux")]
#[test]
fn the_daemon_half_routes_by_the_map_only_with_a_frontend() {
    assert!(!routes_by_map(&flags(&[]).expect("flags")));
    let frontend = flags(&["--frontend=http://127.0.0.1:8981"]).expect("flags");
    assert!(routes_by_map(&frontend));
}

/// A membership service that admits every node as member 1 at boot 1 and sends no map: a
/// storage half started with `--frontend` registers before it opens its cache, so its
/// start waits for an answer.
#[cfg(target_os = "linux")]
struct Admits;

#[cfg(target_os = "linux")]
#[tonic::async_trait]
impl kbf_proto::kbf::map::v1::membership_server::Membership for Admits {
    async fn register(
        &self,
        _: tonic::Request<kbf_proto::kbf::map::v1::RegisterRequest>,
    ) -> Result<tonic::Response<kbf_proto::kbf::map::v1::RegisterResponse>, tonic::Status> {
        Ok(tonic::Response::new(
            kbf_proto::kbf::map::v1::RegisterResponse {
                member_id: 1,
                boot: 1,
                heartbeat_interval_ms: 60_000,
                map: None,
            },
        ))
    }

    async fn heartbeat(
        &self,
        _: tonic::Request<kbf_proto::kbf::map::v1::HeartbeatRequest>,
    ) -> Result<tonic::Response<kbf_proto::kbf::map::v1::HeartbeatResponse>, tonic::Status> {
        Ok(tonic::Response::new(
            kbf_proto::kbf::map::v1::HeartbeatResponse {
                map_generation: 0,
                map: None,
            },
        ))
    }
}

/// [`Admits`] on a port of its own, as the `--frontend` flag's value.
#[cfg(target_os = "linux")]
fn admitting_frontend(tokio: &tokio::runtime::Runtime) -> String {
    let _enter = tokio.enter();
    let incoming =
        tonic::transport::server::TcpIncoming::bind("127.0.0.1:0".parse().expect("address"))
            .expect("bind");
    let addr = incoming.local_addr().expect("address");
    tokio.spawn(
        tonic::transport::Server::builder()
            .add_service(kbf_proto::kbf::map::v1::membership_server::MembershipServer::new(Admits))
            .serve_with_incoming(incoming),
    );
    format!("--frontend=http://{addr}")
}

/// Catches a starter that cannot start the daemon half over the storage half it is
/// given, in either mode: the routed one (the node's own member, its storage and its map)
/// and the one-box in-process plane. Neither dials anything at start: the fake driver
/// moves no blobs, so no plane is made.
#[cfg(target_os = "linux")]
#[test]
fn a_starter_starts_the_daemon_half_over_the_storage_half_in_either_mode() {
    let dir = crate::one_cache::under_target("starter");
    let tokio = tokio::runtime::Runtime::new().expect("runtime");
    // The daemon reads its certificates at start.
    std::fs::create_dir_all(&dir).expect("scratch");
    let ca = rcgen::CertifiedIssuer::self_signed(
        rcgen::CertificateParams::new(Vec::<String>::new()).expect("CA params"),
        rcgen::KeyPair::generate().expect("key"),
    )
    .expect("CA");
    let key = rcgen::KeyPair::generate().expect("key");
    let cert = rcgen::CertificateParams::new(vec!["node-1".to_owned()])
        .expect("params")
        .signed_by(&key, &ca)
        .expect("sign");
    let at = |name: &str| dir.join(name);
    std::fs::write(at("ca.pem"), ca.pem()).expect("write");
    std::fs::write(at("node.pem"), cert.pem()).expect("write");
    std::fs::write(at("node.key"), key.serialize_pem()).expect("write");
    let daemon = kbf_node::Cli::try_parse_from([
        "kbf-daemon".to_owned(),
        "--server=https://127.0.0.1:8981".to_owned(),
        format!("--ca-cert={}", at("ca.pem").display()),
        format!("--cert={}", at("node.pem").display()),
        format!("--key={}", at("node.key").display()),
        "--node-id=node-1".to_owned(),
        "--driver=fake".to_owned(),
        format!("--data-dir={}", at("data").display()),
    ])
    .expect("the daemon's flags");
    let admitting = admitting_frontend(&tokio);
    for frontend in [None, Some(admitting.as_str())] {
        let at = |name: &str| dir.join(frontend.map_or("one", |_| "farm")).join(name);
        let cold = format!("--cold-store=file://{}", at("cold").display());
        let meta = format!("--meta-dir={}", at("meta").display());
        let mut extra = vec!["--storage-listen=127.0.0.1:0", &cold, &meta];
        extra.extend(frontend);
        let flags = flags(&extra).expect("flags");
        let half = tokio
            .block_on(kbf_server::storage_half::start(&flags.storage, || {
                Ok(std::future::pending::<()>())
            }))
            .expect("the storage half starts");
        let start = starter(&flags, &half, tokio.handle());
        let _node = start(&daemon).expect("the daemon half starts");
    }
}

/// Catches the daemon half taking the bound address for its own member's when the node
/// registers under `--advertise-addr`: the map carries the advertised address, so a plane
/// that looks for itself under the bound one never finds itself and dials its own node
/// over the network (or not at all). With the flag the member is the advertised address, a
/// routable-looking one that nothing is bound to; without it, the bound address.
///
/// Mutant: `local_member` registering `half.addr.to_string()` whatever the flag says.
#[cfg(target_os = "linux")]
#[test]
fn the_local_member_is_registered_under_the_advertised_address() {
    let dir = crate::one_cache::under_target("local-member");
    let tokio = tokio::runtime::Runtime::new().expect("runtime");
    let admitting = admitting_frontend(&tokio);
    for advertise in [Some("192.0.2.10:9982"), None] {
        let at = |name: &str| {
            dir.join(advertise.map_or("bound", |_| "advertised"))
                .join(name)
        };
        let cold = format!("--cold-store=file://{}", at("cold").display());
        let meta = format!("--meta-dir={}", at("meta").display());
        let mut extra = vec![
            "--storage-listen=127.0.0.1:0".to_owned(),
            cold,
            meta,
            admitting.clone(),
        ];
        extra.extend(advertise.map(|a| format!("--advertise-addr={a}")));
        let extra: Vec<&str> = extra.iter().map(String::as_str).collect();
        let flags = flags(&extra).expect("flags");
        let half = tokio
            .block_on(kbf_server::storage_half::start(&flags.storage, || {
                Ok(std::future::pending::<()>())
            }))
            .expect("the storage half starts");
        let member = local_member(&flags, &half);
        let want = advertise.map_or_else(|| half.addr.to_string(), str::to_owned);
        assert_eq!(member.addr(), want);
    }
}
