//! `kbf.map.v1.Membership` on the wire (`kbf_server::members_rpc`): what a storage node
//! is told, how every failure of the store or the call maps to a gRPC code, who may call,
//! and the frontend's timer that expires members.

#[path = "support/hooked.rs"]
mod hooked;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use hooked::Hooked;
use kbf_map::MemberState;
use kbf_proto::kbf::map::v1 as pb;
use kbf_proto::kbf::map::v1::membership_client::MembershipClient;
use kbf_server::members::{Members, MembershipConfig, PROTOCOL};
use kbf_server::members_rpc::{MembershipService, sweep};
use kbf_state::{Cond, Store, Txn};
use kbf_storage::service::{Admission, ROLE_URI_PREFIX};
use rcgen::{CertificateParams, ExtendedKeyUsagePurpose, KeyPair, SanType};
use tonic::Code;
use tonic::transport::server::TcpIncoming;
use tonic::transport::{Channel, Server};

const MS: Duration = Duration::from_millis(1);

fn config() -> MembershipConfig {
    MembershipConfig {
        ttl: Duration::from_millis(60),
        dwell: Duration::from_secs(1),
        replication: 2,
    }
}

/// A frontend's membership service on a loopback port, over `store`.
async fn serve(store: &Arc<Hooked>) -> (Arc<Members>, MembershipClient<Channel>) {
    let members = Arc::new(Members::new(Arc::clone(store) as Arc<dyn Store>, config()));
    let incoming = TcpIncoming::bind("127.0.0.1:0".parse().expect("addr")).expect("bind");
    let addr: SocketAddr = incoming.local_addr().expect("addr");
    let service = MembershipService::new(Arc::clone(&members), Admission::Open).server();
    tokio::spawn(
        Server::builder()
            .add_service(service)
            .serve_with_incoming(incoming),
    );
    let channel = Channel::from_shared(format!("http://{addr}"))
        .expect("endpoint")
        .connect()
        .await
        .expect("connect");
    (members, MembershipClient::new(channel))
}

fn request(i: u32) -> pb::RegisterRequest {
    pb::RegisterRequest {
        role: pb::Role::Storage.into(),
        instance_id: format!("node-{i}"),
        addr: format!("10.0.0.{i}:8982"),
        weight: 1,
        zone: "a".to_owned(),
        protocol: PROTOCOL,
        catching_up: false,
    }
}

/// Catches: a registration answer that lacks what the node needs (the store's id and
/// boot, the interval to beat at, the map with the node in it as Active), an interval
/// not below the TTL, and a heartbeat that does not report the map's generation.
#[tokio::test]
async fn register_answers_with_identity_interval_and_map() {
    let store = Hooked::new();
    let (members, mut client) = serve(&store).await;
    let got = client
        .register(request(1))
        .await
        .expect("registers")
        .into_inner();
    assert_eq!((got.member_id, got.boot), (1, 1));
    assert_eq!(got.heartbeat_interval_ms, 20, "a third of the 60 ms TTL");
    let map = kbf_map::Map::try_from(got.map.expect("a map")).expect("a valid map");
    let me = map.member(kbf_map::MemberId(1)).expect("in the map");
    assert_eq!(
        (me.state, me.zone.as_str(), me.weight),
        (MemberState::Active, "a", 1)
    );
    assert_eq!(members.current_map().expect("read"), Some(map.clone()));

    let beat = client
        .heartbeat(pb::HeartbeatRequest {
            member_id: 1,
            boot: 1,
            catching_up: false,
            have_generation: 0,
        })
        .await
        .expect("beats")
        .into_inner();
    assert_eq!(beat.map_generation, map.generation());
    assert_eq!(
        beat.map,
        Some(pb::Map::from(&map)),
        "a node with no map is sent it"
    );
    let current = client
        .heartbeat(pb::HeartbeatRequest {
            member_id: 1,
            boot: 1,
            catching_up: false,
            have_generation: map.generation(),
        })
        .await
        .expect("beats")
        .into_inner();
    assert_eq!(current.map_generation, map.generation());
    assert_eq!(current.map, None, "a node that is current is sent no map");
}

/// Catches: a failure that reaches a node as the wrong code. A node retries UNAVAILABLE,
/// registers again on NOT_FOUND and must not retry INVALID_ARGUMENT or FAILED_PRECONDITION
/// as if they were outages: a non-storage role, a blank field, an old protocol, an old
/// boot, a lapsed member, a store that fails, writers that keep losing, and a stored map
/// that does not decode each have their own code.
#[tokio::test]
async fn every_failure_has_its_own_code() {
    let store = Hooked::new();
    let (_, mut client) = serve(&store).await;
    let register = |mut client: MembershipClient<Channel>, edit: fn(&mut pb::RegisterRequest)| async move {
        let mut r = request(1);
        edit(&mut r);
        client.register(r).await.expect_err("refused").code()
    };
    assert_eq!(
        register(client.clone(), |r| r.role = 0).await,
        Code::InvalidArgument
    );
    assert_eq!(
        register(client.clone(), |r| r.weight = 0).await,
        Code::InvalidArgument
    );
    assert_eq!(
        register(client.clone(), |r| r.protocol = 0).await,
        Code::FailedPrecondition
    );

    client.register(request(1)).await.expect("registers");
    client.register(request(1)).await.expect("registers again");
    let beat = |id, boot| pb::HeartbeatRequest {
        member_id: id,
        boot,
        catching_up: false,
        have_generation: 0,
    };
    let old = client.heartbeat(beat(1, 1)).await.expect_err("old boot");
    assert_eq!(old.code(), Code::FailedPrecondition);
    let unknown = client.heartbeat(beat(9, 1)).await.expect_err("unknown");
    assert_eq!(unknown.code(), Code::NotFound);

    store
        .fail_gets
        .store(1, std::sync::atomic::Ordering::SeqCst);
    let down = client.register(request(1)).await.expect_err("store down");
    assert_eq!(down.code(), Code::Unavailable);
    store.conflict_on(Some("boot/"));
    let busy = client.register(request(1)).await.expect_err("contended");
    assert_eq!(busy.code(), Code::Aborted);
    store.conflict_on(None);

    store
        .fake
        .txn(Txn::new().put("map", vec![0xff], Cond::Any))
        .expect("store")
        .expect("commit");
    let broken = client.register(request(1)).await.expect_err("corrupt");
    assert_eq!(broken.code(), Code::Internal);
}

/// Catches: a store call that blocks the runtime (it runs on a blocking thread), and a
/// panic inside it taking the server down instead of failing the one call: the call
/// answers INTERNAL and the next call works.
#[tokio::test]
async fn a_panic_in_the_store_fails_one_call() {
    let store = Hooked::new();
    let (_, mut client) = serve(&store).await;
    store.after_get("instance/", || panic!("the store blew up"));
    let hurt = client.register(request(1)).await.expect_err("panicked");
    assert_eq!(hurt.code(), Code::Internal);
    assert!(
        hurt.message().contains("membership task"),
        "{}",
        hurt.message()
    );
    client
        .register(request(1))
        .await
        .expect("the next call works");
}

fn leaf(role: &str) -> Vec<u8> {
    let mut params = CertificateParams::new(Vec::<String>::new()).expect("params");
    params.subject_alt_names = vec![SanType::URI(
        format!("{ROLE_URI_PREFIX}{role}").try_into().expect("IA5"),
    )];
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
    let key = KeyPair::generate().expect("key");
    params.self_signed(&key).expect("cert").der().to_vec()
}

/// Catches: registration open to the wrong role. Under mutual TLS only a certificate that
/// names the role `storage` may register: a daemon's or a frontend's is PERMISSION_DENIED,
/// none is UNAUTHENTICATED, bytes that are no certificate are UNAUTHENTICATED; plain text
/// admits everyone, as the storage listener does.
#[test]
fn only_the_storage_role_may_register() {
    let members = Arc::new(Members::new(Hooked::new() as Arc<dyn Store>, config()));
    let certified = MembershipService::new(Arc::clone(&members), Admission::Certified);
    let open = MembershipService::new(members, Admission::Open);
    assert!(open.admit_leaf(None).is_ok());
    assert!(certified.admit_leaf(Some(&leaf("storage"))).is_ok());
    for role in ["daemon", "frontend"] {
        let e = certified.admit_leaf(Some(&leaf(role))).expect_err(role);
        assert_eq!(e.code(), Code::PermissionDenied, "{role}");
        assert!(e.message().contains(role), "{}", e.message());
    }
    assert_eq!(
        certified.admit_leaf(None).expect_err("none").code(),
        Code::Unauthenticated
    );
    let junk = certified
        .admit_leaf(Some(b"not a certificate"))
        .expect_err("junk");
    assert_eq!(junk.code(), Code::Unauthenticated);
}

/// Catches: a frontend that never expires anyone on its own. Nobody calls `refresh` here:
/// the timer does, so a member whose heartbeats stop (the manual clock moves past the TTL)
/// is Down in the map within a few beats; a refresh that fails once (the store errors) is
/// tried again at the next beat.
#[tokio::test]
async fn the_sweeper_expires_members_by_itself() {
    let store = Hooked::new();
    let (members, mut client) = serve(&store).await;
    client.register(request(1)).await.expect("registers");
    store
        .fail_gets
        .store(1, std::sync::atomic::Ordering::SeqCst);
    let sweeper = tokio::spawn(sweep(Arc::clone(&members)));
    while store.fail_gets.load(std::sync::atomic::Ordering::SeqCst) > 0 {
        tokio::time::sleep(MS).await;
    }
    store.clock.advance(61 * MS);
    for _ in 0..500 {
        let map = members.current_map().expect("read").expect("map");
        if map.members()[0].state == MemberState::Down {
            sweeper.abort();
            return;
        }
        tokio::time::sleep(10 * MS).await;
    }
    panic!("the sweeper never marked the silent member Down");
}

/// Catches: expired rows nobody frees (session rows, the results of finished operations
/// and every other time-to-live key sit in the file for good): the timer, with no caller,
/// removes a row whose time ran out, as an `Expire` row in the change feed, and a reap
/// that fails once is tried again at the next beat.
#[tokio::test]
async fn the_sweeper_frees_expired_rows() {
    let store = Hooked::new();
    let (members, _client) = serve(&store).await;
    let put = Txn::new().put_ttl("done/0001", "x", 30 * MS, Cond::Any);
    store.txn(put).expect("store").expect("put");
    store.clock.advance(31 * MS);
    store
        .fail_reaps
        .store(1, std::sync::atomic::Ordering::SeqCst);
    let sweeper = tokio::spawn(sweep(Arc::clone(&members)));
    for _ in 0..500 {
        let feed = store.changes(kbf_state::Revision::ZERO, 100).expect("feed");
        if feed
            .rows
            .iter()
            .any(|row| row.key == "done/0001" && row.event == kbf_state::Event::Expire)
        {
            sweeper.abort();
            assert_eq!(
                store.fail_reaps.load(std::sync::atomic::Ordering::SeqCst),
                0,
                "the failing reap was spent before the one that freed the row"
            );
            return;
        }
        tokio::time::sleep(10 * MS).await;
    }
    panic!("the sweeper never freed the expired row");
}
