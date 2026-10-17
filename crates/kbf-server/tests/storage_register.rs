//! `kbf storage --frontend`: a storage node registers with a real frontend in this
//! process (`bind_server_with_state` over the fake state store and its manual clock),
//! heartbeats, and is expired by the frontend's own timer when it stops.

#[path = "support/hooked.rs"]
mod hooked;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use hooked::Hooked;
use kbf_front::Cache;
use kbf_map::{MemberId, MemberState};
use kbf_proto::kbf::map::v1 as pb;
use kbf_proto::kbf::map::v1::membership_client::MembershipClient;
use kbf_proto::worker::segments_client::SegmentsClient;
use kbf_server::NodeMap;
use kbf_server::join::{Identity as NodeIdentity, Joiner};
use kbf_server::members::{Members, MembershipConfig};
use kbf_server::storage_health::Reported;
use kbf_server::storage_node::{StorageFlags, command, parse, start_storage};
use kbf_server::{
    EpochPolicy, Listeners, StateStore, TlsSource, WorkerTls, bind_server_with_state,
};
use kbf_state::{Store, Versioned};
use kbf_storage::Storage as _;
use kbf_storage::repair::{NoLedger, Options, RepairRecord, Repairs};
use kbf_storage::service::{CatchUp, OpenWrites, ROLE_URI_PREFIX, RemoteStorage};
use kbf_store::FaultFs;
use kbf_types::{Digest, DigestFunction};
use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, DnType, ExtendedKeyUsagePurpose, IsCa,
    KeyPair, KeyUsagePurpose, SanType,
};
use tonic::Code;
use tonic::transport::{Certificate, ClientTlsConfig, Endpoint, Identity};

const MS: Duration = Duration::from_millis(1);

fn dir(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("storage-register")
        .join(name);
    // A state store left by an earlier run would be a cluster that has already formed.
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("directory");
    dir
}

fn flags(dir: &Path, more: &[String]) -> Result<StorageFlags, clap::Error> {
    let mut argv = vec!["storage".to_owned()];
    if !more.iter().any(|m| m.starts_with("--storage-listen")) {
        argv.push("--storage-listen=127.0.0.1:0".to_owned());
    }
    argv.extend([
        format!("--cold-store=file://{}", dir.join("cold").display()),
        format!("--meta-dir={}", dir.join("meta").display()),
    ]);
    argv.extend_from_slice(more);
    parse(&command().try_get_matches_from(argv)?)
}

fn never() -> std::io::Result<std::future::Pending<()>> {
    Ok(std::future::pending())
}

/// A TTL of 60 ms (beats every 20 ms) on a clock that moves only when a test moves it.
fn config() -> MembershipConfig {
    MembershipConfig {
        ttl: 60 * MS,
        dwell: Duration::from_secs(1),
        replication: 2,
    }
}

struct Frontend {
    worker: SocketAddr,
    store: Arc<Hooked>,
    members: Members,
}

fn listeners(worker_tls: Option<WorkerTls>) -> Listeners {
    let loopback: SocketAddr = "127.0.0.1:0".parse().expect("addr");
    Listeners {
        reapi: loopback,
        worker: loopback,
        reapi_tls: None,
        worker_tls,
        heartbeat_interval: Duration::from_secs(5),
        hello_wait: Duration::from_secs(10),
        tick: Duration::from_millis(50),
        unservable_wait: kbf_sched::UNSERVABLE_WAIT,
        memory_ladder: kbf_server::MemoryLadder::default(),
        finished_retention: kbf_sched::FINISHED_RETENTION,
        shutdown_timeout: Duration::from_secs(10),
        store_probe_timeout: kbf_server::health::STORE_PROBE_TIMEOUT,
        invocation_memory: kbf_front::DEFAULT_INVOCATION_MEMORY,
        segments: kbf_front::SegmentsConfig::DEFAULT,
        streams: kbf_server::StreamTiming::default(),
        stream_limits: kbf_server::StreamLimits::DEFAULT,
    }
}

/// A frontend with a state store: the membership service is on its worker listener and
/// its timer expires members.
fn frontend(worker_tls: Option<WorkerTls>) -> Frontend {
    let store = Hooked::new();
    let state = StateStore {
        store: Arc::clone(&store) as Arc<dyn Store>,
        epoch: EpochPolicy::Keep,
        members: config(),
        history: None,
    };
    let bound = bind_server_with_state(
        Arc::new(Cache::memory()),
        listeners(worker_tls),
        None,
        kbf_auth::Policy::allow_all(),
        Some(state),
        std::future::pending::<()>(),
    )
    .expect("binds");
    let worker = bound.worker;
    tokio::spawn(bound.serving);
    Frontend {
        worker,
        members: Members::new(Arc::clone(&store.fake) as Arc<dyn Store>, config()),
        store,
    }
}

async fn until_async<T, Fut: std::future::Future<Output = Option<T>>>(
    what: &str,
    mut probe: impl FnMut() -> Fut,
) -> T {
    for _ in 0..1000 {
        if let Some(found) = probe().await {
            return found;
        }
        tokio::time::sleep(5 * MS).await;
    }
    panic!("timed out waiting for {what}");
}

async fn until<T>(what: &str, mut probe: impl FnMut() -> Option<T>) -> T {
    for _ in 0..1000 {
        if let Some(found) = probe() {
            return found;
        }
        tokio::time::sleep(5 * MS).await;
    }
    panic!("timed out waiting for {what}");
}

fn row(f: &Frontend, id: u64) -> Option<Versioned> {
    f.store.fake.get(&format!("member/{id}")).expect("read")
}

fn member(f: &Frontend, id: u64) -> Option<kbf_map::Member> {
    let map = f.members.current_map().expect("read")?;
    map.member(MemberId(id)).cloned()
}

fn state(f: &Frontend, id: u64) -> Option<MemberState> {
    member(f, id).map(|m| m.state)
}

/// Catches: a node that registers but does not keep its row alive, a frontend that never
/// expires a node that stops, and a registration that tells the frontend something other
/// than what the node is. The node registers on start (its bound address, Active); the
/// store's clock then moves 45 ms (TTL 60 ms) after each beat it sees, six times, which is
/// four TTLs and which the row survives only because the node beats; when the node stops,
/// 61 ms are enough for the frontend's own timer to mark it Down.
#[tokio::test]
async fn a_storage_node_registers_and_heartbeats_across_several_ttls() {
    let f = frontend(None);
    let url = format!("--frontend=http://{}", f.worker);
    let node = start_storage(&flags(&dir("beats"), &[url]).expect("flags"), never)
        .await
        .expect("starts");
    let addr = node.addr;
    let serving = tokio::spawn(node.serve());

    let mut seen = until("the node to register", || row(&f, 1)).await.version;
    let me = until("the node in the map", || member(&f, 1)).await;
    assert_eq!(
        (me.addr.clone(), me.state),
        (addr.to_string(), MemberState::Active)
    );
    for round in 1..=6 {
        f.store.clock.advance(45 * MS);
        seen = until("a heartbeat", || {
            row(&f, 1).filter(|r| r.version != seen).map(|r| r.version)
        })
        .await;
        assert_eq!(state(&f, 1), Some(MemberState::Active), "round {round}");
    }

    serving.abort();
    f.store.clock.advance(61 * MS);
    until("the frontend to mark the silent node Down", || {
        (state(&f, 1) == Some(MemberState::Down)).then_some(())
    })
    .await;
}

/// Catches: a node that treats a lapsed row as fatal or never notices. After its row
/// expires (the manual clock moves past the TTL right after a beat, and the frontend
/// marks it Down) its next heartbeat is NOT_FOUND; it registers again, keeps its member
/// id, gets boot 2 and is Active.
#[tokio::test]
async fn a_node_whose_row_lapsed_registers_again() {
    let f = frontend(None);
    let url = format!("--frontend=http://{}", f.worker);
    let node = start_storage(&flags(&dir("lapse"), &[url]).expect("flags"), never)
        .await
        .expect("starts");
    let _serving = tokio::spawn(node.serve());
    let first = until("the node to register", || row(&f, 1)).await.version;
    until("a beat", || row(&f, 1).filter(|r| r.version != first)).await;
    f.store.clock.advance(61 * MS);
    // Boot 2 is the registration after the lapse: a beat of boot 2 is accepted, and the
    // member has the id it had.
    until("the node to register again", || {
        f.members
            .heartbeat(
                MemberId(1),
                2,
                kbf_server::members::Progress::CaughtUp,
                Reported::default(),
            )
            .ok()
    })
    .await;
    // The row of boot 2 is written before the refresh that publishes it, and a beat of
    // boot 2 is accepted in between: the map is Down until that refresh lands. Wait for
    // the map to say Active; nothing moves it back, the row is live.
    until("the map to hold the node Active", || {
        (state(&f, 1) == Some(MemberState::Active)).then_some(())
    })
    .await;
    assert_eq!(state(&f, 1), Some(MemberState::Active));
    assert_eq!(f.store.fake.scan_prefix("member/").expect("scan").len(), 1);
}

/// Member 1's address in the map. Waits for the map: the refresh that publishes it can
/// land after the member's row, on a slow runner.
async fn node_addr_text(f: &Frontend) -> String {
    let map = until("the map", || f.members.current_map().expect("read")).await;
    map.member(MemberId(1)).expect("member 1").addr.clone()
}

/// Catches: a node that gives up, or loses its place, when the frontend fails: the first
/// three registrations fail (the store errors; UNAVAILABLE), the node waits and tries
/// again until it is in; two heartbeats then fail, and the node keeps its registration
/// (boot 1, no second registration) and beats again afterwards.
#[tokio::test]
async fn a_node_retries_through_failures_and_keeps_its_boot() {
    let f = frontend(None);
    let channel = Endpoint::from_shared(format!("http://{}", f.worker))
        .expect("endpoint")
        .connect_lazy();
    let who = NodeIdentity {
        instance_id: "node-x".to_owned(),
        addr: "192.0.2.5:8982".to_owned(),
        zone: String::new(),
        weight: 1,
    };
    f.store
        .fail_gets
        .store(3, std::sync::atomic::Ordering::SeqCst);
    let joiner = tokio::spawn(Joiner::new(channel, who, 5 * MS).run());
    let first = until("the node to register", || row(&f, 1)).await.version;
    f.store
        .fail_gets
        .store(2, std::sync::atomic::Ordering::SeqCst);
    f.store.clock.advance(10 * MS);
    until("a heartbeat after the failures", || {
        row(&f, 1).filter(|r| r.version != first)
    })
    .await;
    assert_eq!(state(&f, 1), Some(MemberState::Active));
    assert_eq!(
        f.store.fake.scan_prefix("member/").expect("scan").len(),
        1,
        "one member, registered once"
    );
    joiner.abort();
}

/// Catches: a rebuilding node that enters the map as anything but CatchingUp, a report of
/// "still rebuilding" that promotes it, and a report of "done" that does not. A joiner
/// whose [`CatchUp`] says it is rebuilding registers through the frontend's membership
/// service and is CatchingUp; a heartbeat while it still rebuilds leaves it so and bumps
/// no generation; after [`CatchUp::finish`] the next heartbeat makes it Active in a new
/// generation, which is the one the heartbeat answers. Mutants: `catching_up` not sent
/// (Active); the promotion on every beat (Active while rebuilding); no promotion.
#[tokio::test]
async fn a_rebuilding_node_registers_catching_up_and_is_promoted_on_its_report() {
    let f = frontend(None);
    let channel = Endpoint::from_shared(format!("http://{}", f.worker))
        .expect("endpoint")
        .connect_lazy();
    let who = NodeIdentity {
        instance_id: "node-r".to_owned(),
        addr: "192.0.2.6:8982".to_owned(),
        zone: String::new(),
        weight: 1,
    };
    let catch_up = CatchUp::rebuilding();
    let mut joiner = Joiner::new(channel, who, 5 * MS).with_catch_up(catch_up.clone());
    let joined = joiner.register().await.expect("registers");
    assert_eq!(state(&f, joined.member_id), Some(MemberState::CatchingUp));

    let still = joiner.beat(&joined).await.expect("beats");
    assert_eq!(
        still, joined.generation,
        "a beat while rebuilding changed the map"
    );
    assert_eq!(state(&f, joined.member_id), Some(MemberState::CatchingUp));

    catch_up.finish();
    let promoted = joiner.beat(&joined).await.expect("beats");
    assert_eq!(state(&f, joined.member_id), Some(MemberState::Active));
    let map = f.members.current_map().expect("read").expect("a map");
    assert_eq!(promoted, map.generation());
    assert_eq!(
        promoted,
        joined.generation + 1,
        "one generation for the promotion"
    );
}

/// Catches: flags that are ignored or accepted when they cannot work: the advertised
/// address, zone and weight are what the frontend records; the node refuses a flag that
/// needs `--frontend` without it, a non-http URL, `https` without the certificate flags,
/// and a wildcard listen address with no `--advertise-addr` (it would register an address
/// nobody can dial); and a node without `--frontend` registers nowhere.
#[tokio::test]
async fn the_registration_flags_are_carried_or_refused() {
    let f = frontend(None);
    let url = format!("--frontend=http://{}", f.worker);
    let more = [
        url.clone(),
        "--advertise-addr=192.0.2.7:8982".to_owned(),
        "--storage-zone=eu-1".to_owned(),
        "--storage-weight=3".to_owned(),
    ];
    let node = start_storage(&flags(&dir("flags"), &more).expect("flags"), never)
        .await
        .expect("starts");
    let _serving = tokio::spawn(node.serve());
    until("the node to register", || row(&f, 1)).await;
    let me = until("the node in the map", || member(&f, 1)).await;
    assert_eq!(
        (me.addr.as_str(), me.zone.as_str(), me.weight),
        ("192.0.2.7:8982", "eu-1", 3)
    );

    let d = dir("flag-refusals");
    for (argv, why) in [
        (
            vec!["--advertise-addr=192.0.2.4:5".to_owned()],
            "needs --frontend",
        ),
        (vec!["--storage-zone=z".to_owned()], "needs --frontend"),
        (vec!["--frontend=ftp://x".to_owned()], "http:// or https://"),
        (
            vec!["--frontend=https://x".to_owned()],
            "--storage-tls-cert",
        ),
        (
            vec![
                "--frontend=http://x".to_owned(),
                "--storage-listen=0.0.0.0:0".to_owned(),
            ],
            "--advertise-addr is required",
        ),
    ] {
        let refused = flags(&d, &argv).expect_err(why).to_string();
        assert!(
            refused.contains(why) || refused.contains("required arguments"),
            "{argv:?}: {refused}"
        );
    }
    let alone = flags(&d, &[]).expect("flags");
    let none = alone.joiner(
        "127.0.0.1:1".parse().expect("addr"),
        Arc::new(NodeMap::new(Arc::new(kbf_mapsync::MapHolder::new()))),
    );
    assert!(
        none.expect("no error").is_none(),
        "no --frontend, no registration"
    );
    let help = command().render_help().to_string();
    for flag in [
        "--frontend",
        "--advertise-addr",
        "--storage-zone",
        "--storage-weight",
    ] {
        assert!(help.contains(flag), "{flag}");
    }
}

struct Pki {
    ca: CertifiedIssuer<'static, KeyPair>,
}

impl Pki {
    fn new() -> Self {
        let mut ca = CertificateParams::new(Vec::<String>::new()).expect("CA params");
        ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca.distinguished_name
            .push(DnType::CommonName, "kbf test CA");
        ca.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::DigitalSignature,
        ];
        Self {
            ca: CertifiedIssuer::self_signed(ca, KeyPair::generate().expect("key")).expect("CA"),
        }
    }

    /// A certificate for `localhost` naming `role`, good for serving and for calling.
    fn cert(&self, role: &str) -> (String, String) {
        let mut params = CertificateParams::new(vec!["localhost".to_owned()]).expect("params");
        params.subject_alt_names.push(SanType::URI(
            format!("{ROLE_URI_PREFIX}{role}").try_into().expect("IA5"),
        ));
        params.extended_key_usages = vec![
            ExtendedKeyUsagePurpose::ServerAuth,
            ExtendedKeyUsagePurpose::ClientAuth,
        ];
        let key = KeyPair::generate().expect("key");
        let cert = params.signed_by(&key, &self.ca).expect("sign");
        (cert.pem(), key.serialize_pem())
    }
}

/// Catches: registration that works only in plain text, or that admits any certificate.
/// Over mutual TLS a node whose certificate names the role `storage` registers (it
/// presents its own certificate to the frontend and trusts the CA it was given); a
/// certificate that names the role `daemon` is refused PERMISSION_DENIED.
#[tokio::test]
async fn over_mutual_tls_only_the_storage_role_registers() {
    let pki = Pki::new();
    let (frontend_cert, frontend_key) = pki.cert("frontend");
    let worker_tls = WorkerTls {
        server: TlsSource::fixed(
            "worker",
            frontend_cert.as_bytes(),
            frontend_key.as_bytes(),
            Some(pki.ca.pem().as_bytes()),
        )
        .expect("the worker listener's TLS"),
        deny_list: None,
    };
    let f = frontend(Some(worker_tls));
    let d = dir("tls");
    let write = |name: &str, text: String| {
        let path = d.join(name);
        std::fs::write(&path, text).expect("write");
        path.display().to_string()
    };
    let (cert, key) = pki.cert("storage");
    let more = [
        format!("--frontend=https://localhost:{}", f.worker.port()),
        format!("--storage-tls-cert={}", write("node.pem", cert)),
        format!("--storage-tls-key={}", write("node.key", key)),
        format!("--storage-client-ca={}", write("ca.pem", pki.ca.pem())),
    ];
    let node = start_storage(&flags(&d, &more).expect("flags"), never)
        .await
        .expect("starts");
    let addr = node.addr;
    let _serving = tokio::spawn(node.serve());
    until("the node to register over TLS", || row(&f, 1)).await;
    assert_eq!(node_addr_text(&f).await, addr.to_string());

    let (cert, key) = pki.cert("daemon");
    let tls = ClientTlsConfig::new()
        .ca_certificate(Certificate::from_pem(pki.ca.pem()))
        .identity(Identity::from_pem(cert, key));
    let channel = Endpoint::from_shared(format!("https://localhost:{}", f.worker.port()))
        .expect("endpoint")
        .tls_config(tls)
        .expect("TLS")
        .connect_lazy();
    let who = NodeIdentity {
        instance_id: "daemon-box".to_owned(),
        addr: "192.0.2.9:1".to_owned(),
        zone: String::new(),
        weight: 1,
    };
    let denied = Joiner::new(channel, who, 5 * MS)
        .register()
        .await
        .expect_err("denied");
    assert_eq!(denied.code(), Code::PermissionDenied);
}

/// Catches: a retry that does not back off (a down frontend would be hit at the base
/// rate), one that grows without bound, and one that overflows after many failures: the
/// wait doubles from the base to 30 s and stays there, however many failures.
#[test]
fn the_wait_after_failures_doubles_to_a_cap() {
    use kbf_server::join::{RETRY_BASE, RETRY_MAX, backoff};
    let secs = |failures| backoff(RETRY_BASE, failures).as_secs();
    assert_eq!([secs(0), secs(1), secs(2), secs(4)], [1, 2, 4, 16]);
    assert_eq!(backoff(RETRY_BASE, 5), RETRY_MAX);
    assert_eq!(backoff(RETRY_BASE, 40), RETRY_MAX);
    assert_eq!(backoff(RETRY_BASE, u32::MAX), RETRY_MAX);
    assert_eq!(backoff(5 * MS, 1), 10 * MS);
}

/// Catches: a `kbf frontend` with `--state-store` that does not serve the membership
/// service (or rejects the member flags), so a `kbf storage --frontend` node retries
/// forever against it and never enters the storage map. A real `kbf frontend`
/// (`start_frontend`, SQLite state store, `--member-ttl-secs` and `--member-dwell-secs`)
/// and a real node register; a second registrant re-registers until the map the
/// frontend answers with (published by the refresh that follows a registration) holds the
/// node at its bound address, `Active` (the first member of a fresh cluster is promoted on
/// arrival). The wait is bounded only to fail a hang.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_storage_node_registers_with_a_frontend_and_is_in_its_map() {
    let d = dir("frontend-mode");
    let db = d.join("state").join("state.db");
    let uri = format!("sqlite://{}", db.display());
    let argv = [
        "frontend".to_owned(),
        "--listen=127.0.0.1:0".to_owned(),
        "--worker-listen=127.0.0.1:0".to_owned(),
        "--storage=http://127.0.0.1:1".to_owned(),
        format!("--state-store={uri}"),
        "--member-ttl-secs=600".to_owned(),
        "--member-dwell-secs=600".to_owned(),
    ];
    let front = kbf_server::frontend::parse(
        &kbf_server::frontend::command()
            .try_get_matches_from(argv)
            .expect("the frontend takes the member flags"),
    )
    .expect("parses");
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let handle = kbf_server::frontend::start_frontend(&front, || {
        Ok(async move {
            let _ = stopped.await;
        })
    })
    .expect("starts");
    let worker = handle.worker;
    let serving = tokio::spawn(handle.serve());

    let url = format!("--frontend=http://{worker}");
    let node = start_storage(&flags(&d, &[url]).expect("flags"), never)
        .await
        .expect("starts");
    let addr = node.addr;
    let node_serving = tokio::spawn(node.serve());

    let channel = Endpoint::from_shared(format!("http://{worker}"))
        .expect("endpoint")
        .connect_lazy();
    // Re-registering the same instance answers with the frontend's current map; the
    // node's row is in it once the refresh that publishes the registration has landed.
    let probe = MembershipClient::new(channel);
    let request = pb::RegisterRequest {
        role: pb::Role::Storage.into(),
        instance_id: "probe".to_owned(),
        addr: "192.0.2.9:1".to_owned(),
        weight: 1,
        zone: String::new(),
        protocol: kbf_server::members::PROTOCOL,
        catching_up: false,
    };
    let found = until_async("the node in the frontend's map", || {
        let (mut probe, request) = (probe.clone(), request.clone());
        async move {
            let map = probe.register(request).await.ok()?.into_inner().map?;
            map.members.into_iter().find(|m| m.addr == addr.to_string())
        }
    })
    .await;
    assert_eq!(found.state, pb::MemberState::Active as i32);
    node_serving.abort();
    drop(stop);
    serving.await.expect("joins").expect("stops");
}

fn node_x() -> NodeIdentity {
    NodeIdentity {
        instance_id: "node-x".to_owned(),
        addr: "192.0.2.5:8982".to_owned(),
        zone: String::new(),
        weight: 1,
    }
}

fn joiner_of(f: &Frontend) -> Joiner {
    let channel = Endpoint::from_shared(format!("http://{}", f.worker))
        .expect("endpoint")
        .connect_lazy();
    Joiner::new(channel, node_x(), 5 * MS)
}

fn owed(i: u8) -> RepairRecord {
    RepairRecord {
        digest: Digest::new(DigestFunction::Sha256, [i; 32], 10),
        replica: MemberId(2),
        generation: 1,
    }
}

fn open_journal(fs: &FaultFs) -> Arc<Repairs> {
    Arc::new(Repairs::open(fs.clone(), Options::default(), Arc::new(NoLedger)).expect("journal"))
}

/// The frontend's number once member 1 has been promoted to Active and has reported:
/// polled until `want` (the node beats on its own schedule).
async fn until_number(f: &Frontend, want: Option<u64>) {
    until("the frontend's number", || {
        let health = f.members.storage_health().expect("health");
        (health.under_replicated_blobs == want && health.silent.is_empty()).then_some(())
    })
    .await;
}

/// Catches: a node that does not tell the frontend what its journal holds, or tells it
/// something else. Through the real joiner and the real membership service: 2 copies owed
/// is 2 at the frontend, a third is 3 at the next beat, and a node that restarts (its
/// journal reopened, the process registering again) reports the debts it had, not 0 for
/// "none since I started". Mutants: the count of debts since the journal was opened; the
/// debts never sent; sent as unknown.
#[tokio::test]
async fn a_node_with_a_journal_reports_the_copies_it_owes_across_a_restart() {
    let f = frontend(None);
    let fs = FaultFs::new();
    let repairs = open_journal(&fs);
    for i in 1..=2 {
        let _pending = repairs.owe(owed(i)).await.expect("owed");
    }
    let first = tokio::spawn(joiner_of(&f).with_repairs(Arc::clone(&repairs)).run());
    until("the node to register", || row(&f, 1)).await;
    f.members
        .set_state(MemberId(1), MemberState::Active)
        .expect("promoted");
    until_number(&f, Some(2)).await;

    let _pending = repairs.owe(owed(3)).await.expect("owed");
    until_number(&f, Some(3)).await;

    first.abort();
    drop(repairs);
    let reopened = open_journal(&fs.crash(0));
    let second = tokio::spawn(joiner_of(&f).with_repairs(reopened).run());
    // Boot 2 is the registration after the restart (its index is whole, so it is Active
    // at once). A beat of boot 2 is accepted only after it, and says nothing, so the
    // stale report of boot 1 is gone before the number is read.
    until("the node to register again", || {
        f.members
            .heartbeat(
                MemberId(1),
                2,
                kbf_server::members::Progress::CaughtUp,
                Reported::default(),
            )
            .ok()
    })
    .await;
    until_number(&f, Some(3)).await;
    second.abort();
}

/// Catches: a node with no journal saying it owes nothing. Without one it cannot say, and
/// the frontend must read "unknown", not zero. Mutant: the joiner reports `Known(0)` when
/// it has no journal.
#[tokio::test]
async fn a_node_without_a_journal_says_it_cannot_say() {
    let f = frontend(None);
    let joiner = tokio::spawn(joiner_of(&f).run());
    until("the node to register", || row(&f, 1)).await;
    f.members
        .set_state(MemberId(1), MemberState::Active)
        .expect("promoted");
    until("the node to say it cannot say", || {
        let health = f.members.storage_health().expect("health");
        (health.unreported == [MemberId(1)]).then_some(health)
    })
    .await;
    assert_eq!(
        f.members
            .storage_health()
            .expect("health")
            .under_replicated_blobs,
        None
    );
    joiner.abort();
}

/// The frontend's view of member 1, polled until `ready` holds for it.
async fn until_node(
    f: &Frontend,
    what: &str,
    ready: impl Fn(&kbf_server::storage_health::NodeHealth) -> bool,
) -> kbf_server::storage_health::NodeHealth {
    until(what, || {
        let health = f.members.storage_health().expect("health");
        health
            .nodes
            .iter()
            .copied()
            .find(|n| n.id == MemberId(1) && ready(n))
    })
    .await
}

/// Catches: a node that does not tell the frontend how many writes it has open, tells it a
/// count that is not the handle's, or says nothing about the map it held when it counted.
/// Through the real joiner and membership service: 2 open is 2 at the frontend, one ending
/// is 1, none is 0 (known, not unknown), and the generation is the one the frontend's map
/// has. A node whose joiner has no handle cannot say, and the frontend reads "unknown", not
/// zero. Mutant: the joiner reports `Known(0)` without a handle.
#[tokio::test]
async fn a_node_reports_the_writes_it_has_open_and_the_map_it_holds() {
    let f = frontend(None);
    let open = OpenWrites::new();
    let (first, second) = (open.begin(), open.begin());
    let joiner = tokio::spawn(joiner_of(&f).with_open_writes(open.clone()).run());
    until("the node to register", || row(&f, 1)).await;
    f.members
        .set_state(MemberId(1), MemberState::Active)
        .expect("promoted");
    let node = until_node(&f, "two open writes", |n| n.open_writes == Some(2)).await;
    let map = f.members.current_map().expect("read").expect("a map");
    assert_eq!(node.generation, Some(map.generation()));
    assert_ne!(map.generation(), 0, "the node was sent a map");

    drop(first);
    until_node(&f, "one open write", |n| n.open_writes == Some(1)).await;
    drop(second);
    until_node(&f, "none open", |n| n.open_writes == Some(0)).await;
    joiner.abort();

    let silent = tokio::spawn(joiner_of(&f).run());
    until("the node to register again", || {
        f.members
            .heartbeat(
                MemberId(1),
                2,
                kbf_server::members::Progress::CaughtUp,
                Reported::default(),
            )
            .ok()
    })
    .await;
    let node = until_node(&f, "a report without a count", |n| {
        n.repair_debts.is_none() && n.generation.is_some_and(|g| g != 0)
    })
    .await;
    assert_eq!(node.open_writes, None, "no handle: cannot say");
    silent.abort();
}

/// A node started with `--frontend` that the frontend cannot yet admit (its store fails),
/// listening, and a client of its storage service.
async fn waiting_node(
    f: &Frontend,
    name: &str,
) -> (kbf_server::storage_node::StorageHandle, RemoteStorage) {
    let url = format!("--frontend=http://{}", f.worker);
    let node = start_storage(&flags(&dir(name), &[url]).expect("flags"), never)
        .await
        .expect("starts");
    let channel = Endpoint::from_shared(format!("http://{}", node.addr))
        .expect("endpoint")
        .connect_lazy();
    (node, RemoteStorage::new(channel, 0))
}

/// Catches: a node in a multi-member farm that serves before it knows the farm. While the
/// frontend cannot admit it (its store fails, so there is no map) every kind of call is
/// refused UNAVAILABLE "has not been given a map", not answered as an empty cache (a
/// FindMissing that says every blob is missing, a write taken that the map places
/// elsewhere); once the registration lands, and the caller routes by the map the node
/// holds, the same calls are served. The refusal is the storage service's own check, the
/// only one. Mutant: a service given no map source (every call is served at once).
#[tokio::test]
async fn a_node_with_a_frontend_serves_nothing_until_it_holds_a_map() {
    let f = frontend(None);
    f.store
        .fail_gets
        .store(1_000_000, std::sync::atomic::Ordering::SeqCst);
    let (node, client) = waiting_node(&f, "gate").await;
    let maps = node.maps();
    let mut segments = SegmentsClient::new(
        Endpoint::from_shared(format!("http://{}", node.addr))
            .expect("endpoint")
            .connect_lazy(),
    );
    let _serving = tokio::spawn(node.serve());
    // The node's directory outlives the run: a blob no earlier run stored.
    let blob = format!(
        "a blob of run {} at {:?}",
        std::process::id(),
        std::time::SystemTime::now()
    )
    .into_bytes();
    let digest = kbf_segments::sha256(&blob);

    // Each call waits for the node to fetch a map (its service's bound) before it is
    // refused, so they wait together: the blob calls, the action cache (read and written)
    // and the segment uploads.
    let refused_status = |status: tonic::Status| {
        (
            status.code(),
            status.message().contains("has not been given a map"),
        )
    };
    let refused = |error: kbf_storage::StorageError| refused_status(tonic::Status::from(error));
    let want = (Code::Unavailable, true);
    let digests = [digest];
    let empty = kbf_proto::reapi::ActionResult::default();
    let (found, read, put, got, prepared, begun) = tokio::join!(
        client.find_missing(&digests),
        client.read_blob(&digest),
        client.put(bytes::Bytes::from(blob.clone())),
        client.action_result(&digest),
        client.prepare_action_result(&empty),
        segments.begin(kbf_proto::worker::BeginRequest::default()),
    );
    assert_eq!(refused(found.expect_err("refused")), want);
    assert_eq!(refused(read.expect_err("refused")), want);
    assert_eq!(refused(put.expect_err("refused")), want);
    assert_eq!(refused(got.expect_err("refused")), want);
    assert_eq!(refused(prepared.expect_err("refused")), want);
    assert_eq!(refused_status(begun.expect_err("refused")), want);
    assert_eq!(maps.generation(), 0, "the node has no map");

    f.store
        .fail_gets
        .store(0, std::sync::atomic::Ordering::SeqCst);
    until("the node to hold a map", || {
        (maps.generation() > 0).then_some(())
    })
    .await;
    client.set_map_generation(maps.generation());
    assert_eq!(
        client.find_missing(&[digest]).await.expect("served"),
        [digest]
    );
    client
        .put(bytes::Bytes::from(blob.clone()))
        .await
        .expect("a write is taken");
    assert!(
        client
            .find_missing(&[digest])
            .await
            .expect("served")
            .is_empty()
    );
}

/// Catches: a node that keeps the map it registered with and never learns a bump (a second
/// member joining), or whose heartbeat says it holds a map it does not: the frontend
/// sends the map only to a node that is behind, so a node that did not take it is stuck
/// for ever. The node holds generation 1 from its registration; a second registrant
/// makes generation 2, and the node's own next heartbeat brings it. Mutant: a joiner that
/// ignores the map in a heartbeat's answer.
#[tokio::test]
async fn a_heartbeat_brings_a_node_the_map_it_missed() {
    let f = frontend(None);
    let url = format!("--frontend=http://{}", f.worker);
    let node = start_storage(&flags(&dir("beat-map"), &[url]).expect("flags"), never)
        .await
        .expect("starts");
    let maps = node.maps();
    let _serving = tokio::spawn(node.serve());
    until("the node to hold its first map", || {
        maps.current().filter(|m| m.members().len() == 1)
    })
    .await;
    assert_eq!(maps.generation(), 1);

    f.members
        .register(&kbf_server::members::Registrant {
            instance_id: "second".to_owned(),
            addr: "192.0.2.8:8982".to_owned(),
            zone: String::new(),
            weight: 1,
            protocol: kbf_server::members::PROTOCOL,
            progress: kbf_server::members::Progress::CaughtUp,
        })
        .expect("a second member");
    let learned = until("a heartbeat to bring the new map", || {
        maps.current().filter(|m| m.members().len() == 2)
    })
    .await;
    assert_eq!(learned.generation(), 2);
}

/// A membership service that answers every call with the map it holds (a garbled one, or a valid one).
struct Garbled(pb::Map);

#[tonic::async_trait]
impl kbf_proto::kbf::map::v1::membership_server::Membership for Garbled {
    async fn register(
        &self,
        _: tonic::Request<pb::RegisterRequest>,
    ) -> Result<tonic::Response<pb::RegisterResponse>, tonic::Status> {
        Ok(tonic::Response::new(pb::RegisterResponse {
            member_id: 1,
            boot: 1,
            heartbeat_interval_ms: 1,
            map: Some(self.0.clone()),
        }))
    }

    async fn heartbeat(
        &self,
        _: tonic::Request<pb::HeartbeatRequest>,
    ) -> Result<tonic::Response<pb::HeartbeatResponse>, tonic::Status> {
        Ok(tonic::Response::new(pb::HeartbeatResponse {
            map_generation: 9,
            map: Some(self.0.clone()),
        }))
    }
}

/// Catches: a node that dies, or takes a map of generation 0 as one, when a frontend sends
/// one it cannot read: it is ignored (the node stays without a map, and so, gated, without
/// service) whether it came with the registration or with a heartbeat.
#[tokio::test]
async fn a_map_the_node_cannot_read_is_ignored() {
    let maps = joined_by(pb::Map::default()).await;
    assert!(maps.current().is_none());
    assert_eq!(maps.generation(), 0);
}

/// Catches: a node that goes back to an older map, or takes the same one again as news:
/// the frontend's answer to the registration and to the heartbeat that follows is the same
/// map of generation 4; the node holds it once, and nothing it held before is "replaced".
#[tokio::test]
async fn a_map_that_is_not_newer_changes_nothing() {
    let member = kbf_map::Member {
        id: MemberId(1),
        addr: "192.0.2.5:1".to_owned(),
        zone: String::new(),
        weight: 1,
        state: MemberState::Active,
    };
    let map = kbf_map::Map::new(4, vec![member], kbf_map::Rule::Rendezvous, 1, 1, Vec::new())
        .expect("a map");
    let maps = joined_by(pb::Map::from(&map)).await;
    assert_eq!(maps.generation(), 4);
    assert!(
        maps.previous().is_none(),
        "the second copy replaced nothing"
    );
}

/// A node that registers and beats once with a frontend that answers with `map` both
/// times; the maps it holds after.
async fn joined_by(map: pb::Map) -> Arc<kbf_mapsync::MapHolder> {
    joined_node(map).await.maps()
}

/// The node of [`joined_by`]: its registration was answered with member id 1.
async fn joined_node(map: pb::Map) -> Arc<NodeMap> {
    let incoming =
        tonic::transport::server::TcpIncoming::bind("127.0.0.1:0".parse().unwrap()).expect("bind");
    let addr = incoming.local_addr().expect("address");
    tokio::spawn(
        tonic::transport::Server::builder()
            .add_service(
                kbf_proto::kbf::map::v1::membership_server::MembershipServer::new(Garbled(map)),
            )
            .serve_with_incoming(incoming),
    );
    let channel = Endpoint::from_shared(format!("http://{addr}"))
        .expect("endpoint")
        .connect_lazy();
    let who = NodeIdentity {
        instance_id: "n".to_owned(),
        addr: "192.0.2.5:1".to_owned(),
        zone: String::new(),
        weight: 1,
    };
    let node = Arc::new(NodeMap::new(Arc::new(kbf_mapsync::MapHolder::new())));
    let mut joiner = Joiner::new(channel, who, MS).with_node(Arc::clone(&node));
    let joined = joiner.register().await.expect("registers");
    assert_eq!(joiner.beat(&joined).await.expect("beats"), 9);
    node
}

/// Catches a storage service and a node that read two different maps, or a node that does
/// not know which member it is: `NodeMap` is the one `MapSource` the service reads, over the
/// holder the joiner fills. Before the registration it has no map and no member id; after,
/// it says the map it was sent and the id the registration gave; a caller ahead of it
/// (`refetch`) is waited for and recorded as wanted, and is answered when the holder takes
/// that map. Mutants: `registered` never called (no member id: a node cannot take part in a
/// chain); `current` reading a holder of its own; `refetch` not recording the generation.
#[tokio::test]
async fn the_node_map_is_the_holders_and_knows_the_member_the_registration_named() {
    use futures::FutureExt as _;
    use kbf_storage::replicate::MapSource as _;

    let fresh = NodeMap::new(Arc::new(kbf_mapsync::MapHolder::new()));
    assert!(fresh.current().is_none() && fresh.me().is_none());

    let member = kbf_map::Member {
        id: MemberId(1),
        addr: "192.0.2.5:1".to_owned(),
        zone: String::new(),
        weight: 1,
        state: MemberState::Active,
    };
    let map = |generation| {
        kbf_map::Map::new(
            generation,
            vec![member.clone()],
            kbf_map::Rule::Rendezvous,
            1,
            1,
            Vec::new(),
        )
        .expect("a map")
    };
    let node = joined_node(pb::Map::from(&map(4))).await;
    assert_eq!(node.me(), Some(MemberId(1)));
    let held = node.current().expect("the map it was sent");
    assert_eq!(held.generation(), 4);
    assert!(Arc::ptr_eq(&held, &node.maps().current().expect("held")));

    // A caller ahead of the node: the wait is pending, the generation is wanted, and the
    // holder taking that map ends the wait.
    let mut wanted = node.maps().asks();
    let mut refetch = node.refetch(7);
    assert!(refetch.as_mut().now_or_never().is_none(), "no map of 7 yet");
    assert_eq!(*wanted.borrow_and_update(), 7);
    assert!(node.maps().offer(map(7)));
    let fetched = refetch.await.expect("the map of 7");
    assert_eq!(fetched.generation(), 7);
}

/// Catches a node over mutual TLS that is not a link of the chain, and a repair worker that
/// is never started: the node keeps a repair journal under its metadata directory, and a
/// debt put in it is settled by the worker (the map names no other owner of the blob, so
/// there is no copy to make, and the debt is closed without one). Mutants: the worker not
/// spawned (the debt stays for ever); the node judged plain text (no journal).
#[tokio::test]
async fn a_node_over_mutual_tls_keeps_a_repair_journal_and_a_worker_settles_it() {
    let pki = Pki::new();
    let (frontend_cert, frontend_key) = pki.cert("frontend");
    let worker_tls = WorkerTls {
        server: TlsSource::fixed(
            "worker",
            frontend_cert.as_bytes(),
            frontend_key.as_bytes(),
            Some(pki.ca.pem().as_bytes()),
        )
        .expect("the worker listener's TLS"),
        deny_list: None,
    };
    let f = frontend(Some(worker_tls));
    let d = dir("tls-repair");
    let write = |name: &str, text: String| {
        let path = d.join(name);
        std::fs::write(&path, text).expect("write");
        path.display().to_string()
    };
    let (cert, key) = pki.cert("storage");
    let more = [
        format!("--frontend=https://localhost:{}", f.worker.port()),
        format!("--storage-tls-cert={}", write("node.pem", cert)),
        format!("--storage-tls-key={}", write("node.key", key)),
        format!("--storage-client-ca={}", write("ca.pem", pki.ca.pem())),
    ];
    let mut flags = flags(&d, &more).expect("flags");
    flags.repair.interval = 5 * MS;
    let node = start_storage(&flags, never).await.expect("starts");
    let repairs = node
        .repairs()
        .expect("a node over mutual TLS keeps a journal");
    assert!(
        d.join("meta")
            .join("repair")
            .join("repair.journal")
            .is_file()
    );
    let maps = node.maps();
    let _serving = tokio::spawn(node.serve());

    let blob = kbf_segments::sha256(b"owed to no one else");
    let owed = kbf_storage::repair::RepairRecord {
        digest: blob,
        replica: MemberId(9),
        generation: 1,
    };
    drop(repairs.owe(owed).await.expect("owed"));
    until("the worker to settle the debt", || {
        repairs.open_records().is_empty().then_some(())
    })
    .await;
    assert!(maps.generation() > 0, "it needed the map to say so");
}

/// Catches a `kbf storage-and-daemon` whose in-process view is not the node's own service:
/// one built over a separate storage with no map and no chain would answer every write at
/// once, as one copy nobody owes. The real half (`storage_half::start`, mutual TLS, a
/// frontend) is called through `in_process()`. While the frontend cannot admit it the write
/// is refused UNAVAILABLE "has not been given a map"; once it holds a map in which it and a
/// partner that is not listening both own the blob, the same call is answered, the blob is
/// in this node's store, and the repair journal owes it to the partner. Mutant: `bind_node`
/// handing `in_process` a service made over another, unmapped, chainless storage.
#[tokio::test]
async fn a_write_through_the_halfs_in_process_view_is_judged_by_the_map_and_owes_the_partner() {
    let pki = Pki::new();
    let (frontend_cert, frontend_key) = pki.cert("frontend");
    let worker_tls = WorkerTls {
        server: TlsSource::fixed(
            "worker",
            frontend_cert.as_bytes(),
            frontend_key.as_bytes(),
            Some(pki.ca.pem().as_bytes()),
        )
        .expect("the worker listener's TLS"),
        deny_list: None,
    };
    let f = frontend(Some(worker_tls));
    f.store
        .fail_gets
        .store(1_000_000, std::sync::atomic::Ordering::SeqCst);
    let d = dir("half-in-process");
    let write = |name: &str, text: String| {
        let path = d.join(name);
        std::fs::write(&path, text).expect("write");
        path.display().to_string()
    };
    let (cert, key) = pki.cert("storage");
    let more = [
        format!("--frontend=https://localhost:{}", f.worker.port()),
        format!("--storage-tls-cert={}", write("node.pem", cert)),
        format!("--storage-tls-key={}", write("node.key", key)),
        format!("--storage-client-ca={}", write("ca.pem", pki.ca.pem())),
    ];
    let mut flags = flags(&d, &more).expect("flags");
    flags.repair.interval = 50 * MS;
    let half = kbf_server::storage_half::start(&flags, never)
        .await
        .expect("starts");
    let (in_process, storage, maps) = (half.in_process(), half.storage(), half.maps());
    let repairs = half
        .repairs()
        .expect("a node over mutual TLS keeps a journal");
    let here = half.addr;
    let _serving = tokio::spawn(half.serve());

    let refused = in_process
        .at(0)
        .put(bytes::Bytes::from_static(b"before any map"))
        .await
        .expect_err("no map yet");
    let refused = tonic::Status::from(refused);
    assert_eq!(refused.code(), Code::Unavailable);
    assert!(
        refused.message().contains("has not been given a map"),
        "{refused}"
    );

    f.store
        .fail_gets
        .store(0, std::sync::atomic::Ordering::SeqCst);
    until("the node to register", || row(&f, 1)).await;
    until("the node to hold a map", || {
        (maps.generation() > 0).then_some(())
    })
    .await;

    // The partner: a member whose address nothing listens on.
    let gone = {
        let socket = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        socket.local_addr().expect("address")
    };
    let member = |id, addr: SocketAddr| kbf_map::Member {
        id: MemberId(id),
        addr: addr.to_string(),
        zone: String::new(),
        weight: 1,
        state: MemberState::Active,
    };
    let map = kbf_map::Map::new(
        1_000,
        vec![member(1, here), member(2, gone)],
        kbf_map::Rule::Rendezvous,
        2,
        1,
        Vec::new(),
    )
    .expect("a map");
    // A blob this node is the primary of; the partner is its second owner.
    let blob = (0..)
        .map(|i| format!("owed to the partner {i} of run {}", std::process::id()).into_bytes())
        .find(|b| map.write_owners(&kbf_segments::sha256(b)).first() == Some(&MemberId(1)))
        .expect("a blob");
    let digest = kbf_segments::sha256(&blob);
    assert!(maps.offer(map));

    let answered = in_process
        .at(1_000)
        .put(bytes::Bytes::from(blob))
        .await
        .expect("the write is answered with one copy");
    assert_eq!(answered, digest);
    assert!(
        storage.read_blob(&digest).await.is_ok(),
        "one copy is on this node"
    );
    let owed = repairs.open_records();
    assert_eq!(owed.len(), 1, "{owed:?}");
    assert_eq!(
        (owed[0].digest, owed[0].replica, owed[0].generation),
        (digest, MemberId(2), 1_000)
    );
}

/// Catches an unowned-copy marker that does not survive the real listener's TLS: the
/// frontend's closure copy reached a node's listener (a `TlsSource`, not a bare tonic
/// config) and was refused as a plain write because the caller's certificate was read from
/// the wrong place, so an action-cache entry was refused on a farm of three. A real half
/// holds a map in which another member owns the blob (r = 1): a plain write from a frontend
/// is refused "not the primary"; the marker from a `daemon` certificate is refused the same;
/// from the `frontend` certificate the blob is stored here as one copy.
#[tokio::test]
async fn the_frontends_unowned_copy_is_stored_through_the_real_listener() {
    let pki = Pki::new();
    let (frontend_cert, frontend_key) = pki.cert("frontend");
    let worker_tls = WorkerTls {
        server: TlsSource::fixed(
            "worker",
            frontend_cert.as_bytes(),
            frontend_key.as_bytes(),
            Some(pki.ca.pem().as_bytes()),
        )
        .expect("the worker listener's TLS"),
        deny_list: None,
    };
    let f = frontend(Some(worker_tls));
    let d = dir("half-unowned");
    let write = |name: &str, text: String| {
        let path = d.join(name);
        std::fs::write(&path, text).expect("write");
        path.display().to_string()
    };
    let (cert, key) = pki.cert("storage");
    let more = [
        format!("--frontend=https://localhost:{}", f.worker.port()),
        format!("--storage-tls-cert={}", write("node.pem", cert)),
        format!("--storage-tls-key={}", write("node.key", key)),
        format!("--storage-client-ca={}", write("ca.pem", pki.ca.pem())),
    ];
    let half = kbf_server::storage_half::start(&flags(&d, &more).expect("flags"), never)
        .await
        .expect("starts");
    let (storage, maps, here) = (half.storage(), half.maps(), half.addr);
    let _serving = tokio::spawn(half.serve());
    until("the node to register", || row(&f, 1)).await;
    until("the node to hold a map", || {
        (maps.generation() > 0).then_some(())
    })
    .await;

    let member = |id, port: u16| kbf_map::Member {
        id: MemberId(id),
        addr: format!("127.0.0.1:{port}"),
        zone: String::new(),
        weight: 1,
        state: MemberState::Active,
    };
    let map = kbf_map::Map::new(
        1_000,
        vec![member(1, here.port()), member(2, 1)],
        kbf_map::Rule::Rendezvous,
        1,
        1,
        Vec::new(),
    )
    .expect("a map");
    let blob = (0..)
        .map(|i| {
            format!(
                "owned by the other member {i} of run {}",
                std::process::id()
            )
            .into_bytes()
        })
        .find(|b| map.write_owners(&kbf_segments::sha256(b)) == [MemberId(2)])
        .expect("a blob");
    let digest = kbf_segments::sha256(&blob);
    assert!(maps.offer(map));
    let client = |role: &str| {
        let (cert, key) = pki.cert(role);
        let tls = ClientTlsConfig::new()
            .ca_certificate(Certificate::from_pem(pki.ca.pem()))
            .identity(Identity::from_pem(cert, key));
        let channel = Endpoint::from_shared(format!("https://localhost:{}", here.port()))
            .expect("endpoint")
            .tls_config(tls)
            .expect("TLS")
            .connect_lazy();
        RemoteStorage::new(channel, 1_000)
    };
    let not_primary = |answer: Result<kbf_types::Digest, kbf_storage::StorageError>| matches!(&answer, Err(kbf_storage::StorageError::Invalid(m)) if m.contains("not the primary"));

    let plain = client("frontend")
        .put(bytes::Bytes::from(blob.clone()))
        .await;
    assert!(not_primary(plain));
    let from_daemon = client("daemon")
        .put_unowned(bytes::Bytes::from(blob.clone()))
        .await;
    assert!(not_primary(from_daemon));
    assert!(storage.read_blob(&digest).await.is_err());

    let stored = client("frontend")
        .put_unowned(bytes::Bytes::from(blob))
        .await
        .expect("the frontend's copy is taken");
    assert_eq!(stored, digest);
    assert!(storage.read_blob(&digest).await.is_ok());
}

/// Catches a node with no frontend that keeps a repair journal: it is a one-member farm of
/// itself and owes no one a copy, so it has none and makes no directory for it. A node that
/// registers in plain text (it copies nothing) has none either.
#[tokio::test]
async fn a_node_that_copies_nothing_keeps_no_repair_journal() {
    let d = dir("no-journal");
    let alone = start_storage(&flags(&d, &[]).expect("flags"), never)
        .await
        .expect("starts");
    assert!(alone.repairs().is_none());
    assert!(!d.join("meta").join("repair").exists());

    let f = frontend(None);
    let (plain, _) = waiting_node(&f, "plain-no-journal").await;
    assert!(plain.repairs().is_none());
}

/// Catches: a started node whose service and registration do not share the writes-open
/// count, so the frontend reads "cannot say" or 0 whatever the node is taking in. A node
/// started with `--frontend` (plain text: it copies nothing, so it keeps no journal and its
/// debts are "cannot say") reports its open writes through the real service and the real
/// joiner: 0, then 1 while a `Write` is held open by a caller that has sent part of a blob,
/// then 0 again. Mutants: the service not given the handle; the joiner not given it.
#[tokio::test]
async fn a_started_node_reports_the_writes_it_has_open() {
    use kbf_proto::google::bytestream::WriteRequest;
    use kbf_proto::google::bytestream::byte_stream_client::ByteStreamClient;

    let f = frontend(None);
    let (node, client) = waiting_node(&f, "reports-writes").await;
    let (maps, here) = (node.maps(), node.addr);
    let _serving = tokio::spawn(node.serve());
    until("the node to hold a map", || {
        (maps.generation() > 0).then_some(())
    })
    .await;
    let idle = until_node(&f, "none open", |n| n.open_writes == Some(0)).await;
    assert_eq!(idle.repair_debts, None, "no journal: cannot say");
    drop(client);

    let blob = format!("held open by run {}", std::process::id()).into_bytes();
    let digest = kbf_segments::sha256(&blob);
    let name = format!(
        "uploads/u-1/blobs/{}/{}",
        digest.hash_hex(),
        digest.size_bytes
    );
    let (send, receive) = tokio::sync::mpsc::channel::<WriteRequest>(2);
    let mut write = ByteStreamClient::new(
        Endpoint::from_shared(format!("http://{here}"))
            .expect("endpoint")
            .connect_lazy(),
    );
    let mut request = tonic::Request::new(futures::stream::unfold(receive, |mut r| async move {
        r.recv().await.map(|m| (m, r))
    }));
    request.metadata_mut().insert(
        kbf_storage::service::MAP_GENERATION_HEADER,
        maps.generation().to_string().parse().expect("a header"),
    );
    let answer = tokio::spawn(async move { write.write(request).await });
    let half = blob.len() / 2;
    send.send(WriteRequest {
        resource_name: name,
        write_offset: 0,
        finish_write: false,
        data: blob[..half].to_vec(),
    })
    .await
    .expect("first message");
    until_node(&f, "one open", |n| n.open_writes == Some(1)).await;
    send.send(WriteRequest {
        resource_name: String::new(),
        write_offset: half as i64,
        finish_write: true,
        data: blob[half..].to_vec(),
    })
    .await
    .expect("last message");
    answer.await.expect("joins").expect("the write is taken");
    until_node(&f, "none open again", |n| n.open_writes == Some(0)).await;
}

/// Catches: a started node with a repair journal that does not give it to its registration,
/// so the frontend can never read the debts of a real farm: a mutual-TLS node (it keeps a
/// journal) is read as owing 0, known, and as having 0 writes open, not "cannot say".
/// Mutant: the joiner not given the journal.
#[tokio::test]
async fn a_started_node_with_a_journal_reports_what_it_owes() {
    let pki = Pki::new();
    let (frontend_cert, frontend_key) = pki.cert("frontend");
    let worker_tls = WorkerTls {
        server: TlsSource::fixed(
            "worker",
            frontend_cert.as_bytes(),
            frontend_key.as_bytes(),
            Some(pki.ca.pem().as_bytes()),
        )
        .expect("the worker listener's TLS"),
        deny_list: None,
    };
    let f = frontend(Some(worker_tls));
    let d = dir("reports-debts");
    let write = |name: &str, text: String| {
        let path = d.join(name);
        std::fs::write(&path, text).expect("write");
        path.display().to_string()
    };
    let (cert, key) = pki.cert("storage");
    let more = [
        format!("--frontend=https://localhost:{}", f.worker.port()),
        format!("--storage-tls-cert={}", write("node.pem", cert)),
        format!("--storage-tls-key={}", write("node.key", key)),
        format!("--storage-client-ca={}", write("ca.pem", pki.ca.pem())),
    ];
    let half = kbf_server::storage_half::start(&flags(&d, &more).expect("flags"), never)
        .await
        .expect("starts");
    let _serving = tokio::spawn(half.serve());
    until("the node to register", || row(&f, 1)).await;
    let node = until_node(&f, "debts and writes known", |n| {
        n.repair_debts.is_some() && n.open_writes.is_some()
    })
    .await;
    assert_eq!((node.repair_debts, node.open_writes), (Some(0), Some(0)));
}

/// Catches a node that calls its peers without the certificate it registers with, or that
/// starts with TLS files it cannot read: without the TLS flags there is no peer
/// configuration; with a file that is not there the error names it.
#[test]
fn the_peer_certificate_comes_from_the_tls_flags_and_must_be_readable() {
    let d = dir("peer-tls");
    let mut f = flags(&d, &[]).expect("flags");
    assert!(f.peer_tls().expect("no TLS flags, no error").is_none());
    let missing = d.join("not-there.pem");
    f.tls = Some([missing.clone(), missing.clone(), missing.clone()]);
    let error = f.peer_tls().expect_err("unreadable");
    assert!(error.to_string().contains("not-there.pem"), "{error}");
}

/// A membership service that says to beat every hour, registers a node with the map of
/// generation 1, and answers each heartbeat of a node behind with the map of generation 2.
struct Hourly {
    beats: std::sync::atomic::AtomicUsize,
}

/// [`Hourly`], shared with the test that counts its beats.
struct SharedHourly(Arc<Hourly>);

fn a_map(generation: u64) -> pb::Map {
    let member = kbf_map::Member {
        id: MemberId(1),
        addr: "192.0.2.5:1".to_owned(),
        zone: String::new(),
        weight: 1,
        state: MemberState::Active,
    };
    let map = kbf_map::Map::new(
        generation,
        vec![member],
        kbf_map::Rule::Rendezvous,
        1,
        1,
        Vec::new(),
    )
    .expect("a map");
    pb::Map::from(&map)
}

#[tonic::async_trait]
impl kbf_proto::kbf::map::v1::membership_server::Membership for SharedHourly {
    async fn register(
        &self,
        _: tonic::Request<pb::RegisterRequest>,
    ) -> Result<tonic::Response<pb::RegisterResponse>, tonic::Status> {
        Ok(tonic::Response::new(pb::RegisterResponse {
            member_id: 1,
            boot: 1,
            heartbeat_interval_ms: 3_600_000,
            map: Some(a_map(1)),
        }))
    }

    async fn heartbeat(
        &self,
        request: tonic::Request<pb::HeartbeatRequest>,
    ) -> Result<tonic::Response<pb::HeartbeatResponse>, tonic::Status> {
        self.0
            .beats
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let behind = request.into_inner().have_generation < 2;
        Ok(tonic::Response::new(pb::HeartbeatResponse {
            map_generation: 2,
            map: behind.then(|| a_map(2)),
        }))
    }
}

/// Catches a node that learns of a map only at its next heartbeat: a caller that routed by
/// generation 2 while the node holds 1 would be refused for the whole interval (here an
/// hour, so a node that waits it out never gets the map). The caller asks for the map
/// ([`kbf_mapsync::MapHolder::reach`], which is what the storage service does for a caller
/// ahead of it), the node beats at once, and the map arrives. Mutant: the joiner not
/// watching what is asked for (the wait ends only at the deadline that fails the test).
#[tokio::test]
async fn a_caller_ahead_of_the_node_makes_it_beat_at_once() {
    let service = Arc::new(Hourly {
        beats: std::sync::atomic::AtomicUsize::new(0),
    });
    let incoming =
        tonic::transport::server::TcpIncoming::bind("127.0.0.1:0".parse().unwrap()).expect("bind");
    let addr = incoming.local_addr().expect("address");
    tokio::spawn(
        tonic::transport::Server::builder()
            .add_service(
                kbf_proto::kbf::map::v1::membership_server::MembershipServer::new(SharedHourly(
                    Arc::clone(&service),
                )),
            )
            .serve_with_incoming(incoming),
    );
    let channel = Endpoint::from_shared(format!("http://{addr}"))
        .expect("endpoint")
        .connect_lazy();
    let who = NodeIdentity {
        instance_id: "n".to_owned(),
        addr: "192.0.2.5:1".to_owned(),
        zone: String::new(),
        weight: 1,
    };
    let maps = Arc::new(kbf_mapsync::MapHolder::new());
    let joiner = Joiner::new(channel, who, MS).with_node(Arc::new(NodeMap::new(Arc::clone(&maps))));
    let running = tokio::spawn(joiner.run());
    until("the node to hold the map of generation 1", || {
        (maps.generation() == 1).then_some(())
    })
    .await;
    assert_eq!(service.beats.load(std::sync::atomic::Ordering::SeqCst), 0);

    let reached = tokio::time::timeout(Duration::from_secs(60), maps.reach(2))
        .await
        .expect("the node beat when it was asked, not an hour later");
    assert_eq!(reached.generation(), 2);
    assert_eq!(service.beats.load(std::sync::atomic::Ordering::SeqCst), 1);
    running.abort();
}

/// A membership service whose first two heartbeats fail UNAVAILABLE, then answers; it
/// counts the registrations and the heartbeats it saw.
#[derive(Default)]
struct Flaky {
    registers: std::sync::atomic::AtomicUsize,
    beats: std::sync::atomic::AtomicUsize,
}

/// [`Flaky`], shared with the test that counts.
struct FlakyService(Arc<Flaky>);

impl std::ops::Deref for FlakyService {
    type Target = Flaky;
    fn deref(&self) -> &Flaky {
        &self.0
    }
}

#[tonic::async_trait]
impl kbf_proto::kbf::map::v1::membership_server::Membership for FlakyService {
    async fn register(
        &self,
        _: tonic::Request<pb::RegisterRequest>,
    ) -> Result<tonic::Response<pb::RegisterResponse>, tonic::Status> {
        self.registers
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(tonic::Response::new(pb::RegisterResponse {
            member_id: 1,
            boot: 1,
            heartbeat_interval_ms: 1,
            map: None,
        }))
    }

    async fn heartbeat(
        &self,
        _: tonic::Request<pb::HeartbeatRequest>,
    ) -> Result<tonic::Response<pb::HeartbeatResponse>, tonic::Status> {
        let seen = self.beats.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if seen < 2 {
            return Err(tonic::Status::unavailable("injected"));
        }
        Ok(tonic::Response::new(pb::HeartbeatResponse {
            map_generation: 1,
            map: None,
        }))
    }
}

/// Catches: a node that re-registers, or stops, when a heartbeat fails with anything but
/// NOT_FOUND. The frontend fails the first two heartbeats UNAVAILABLE; the node waits and
/// beats again (a third and fourth beat arrive) and registered exactly once. A mutant
/// that registers again on any failed beat makes the registrations more than one.
#[tokio::test]
async fn a_failed_heartbeat_is_retried_without_registering_again() {
    let flaky = Arc::new(Flaky::default());
    let incoming =
        tonic::transport::server::TcpIncoming::bind("127.0.0.1:0".parse().unwrap()).expect("bind");
    let addr = incoming.local_addr().expect("address");
    tokio::spawn(
        tonic::transport::Server::builder()
            .add_service(
                kbf_proto::kbf::map::v1::membership_server::MembershipServer::new(FlakyService(
                    Arc::clone(&flaky),
                )),
            )
            .serve_with_incoming(incoming),
    );
    let channel = Endpoint::from_shared(format!("http://{addr}"))
        .expect("endpoint")
        .connect_lazy();
    let who = NodeIdentity {
        instance_id: "n".to_owned(),
        addr: "192.0.2.5:1".to_owned(),
        zone: String::new(),
        weight: 1,
    };
    let joiner = tokio::spawn(Joiner::new(channel, who, MS).run());
    until("two failed beats and then two good ones", || {
        (flaky.beats.load(std::sync::atomic::Ordering::SeqCst) >= 4).then_some(())
    })
    .await;
    joiner.abort();
    assert_eq!(
        flaky.registers.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "registered once"
    );
}
