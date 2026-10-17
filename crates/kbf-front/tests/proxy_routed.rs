//! The proxy's raw ByteStream pass-through when the frontend routes by a map
//! (`Owners::routed`): which node a Write, a status query and a Read go to, and the map
//! generation the node is told. The nodes are the fakes of `proxy/harness.rs`.

#[path = "proxy/harness.rs"]
mod harness;

use std::sync::atomic::Ordering::SeqCst;
use std::sync::{Arc, Mutex, PoisonError};

use futures::future::BoxFuture;
use futures::stream;
use harness::{
    CHUNK, Fake, blob, eventually, front_over, node, pattern, routed_owners, routed_owners_over,
    upload,
};
use kbf_auth::Authorizers;
use kbf_map::{Map, Member, MemberId, MemberState, Rule};
use kbf_proto::google::bytestream::byte_stream_client::ByteStreamClient;
use kbf_proto::google::bytestream::{QueryWriteStatusRequest, ReadRequest, WriteRequest};
use kbf_route::{MapSource, MapView, RefetchError};
use kbf_types::{Digest, DigestFunction};
use tonic::Request;
use tonic::transport::Channel;

/// The map generation the fixed map is.
const GENERATION: u64 = 7;

fn map(states: [MemberState; 3]) -> Arc<Map> {
    let members = states
        .iter()
        .zip(1..)
        .map(|(state, id)| Member {
            id: MemberId(id),
            addr: format!("127.0.0.1:{id}"),
            zone: String::new(),
            weight: 1,
            state: *state,
        })
        .collect();
    Arc::new(Map::new(GENERATION, members, Rule::Rendezvous, 2, 1, Vec::new()).expect("a map"))
}

/// Three fake nodes, the map over them, and a frontend routed by it.
struct Trio {
    fakes: Vec<Arc<Fake>>,
    map: Arc<Map>,
    front: harness::Front,
}

async fn trio(states: [MemberState; 3]) -> Trio {
    let fakes: Vec<_> = (0..3).map(|_| Fake::new(false)).collect();
    let mut addrs = Vec::new();
    for fake in &fakes {
        addrs.push(node(fake).await);
    }
    let map = map(states);
    let owners = routed_owners(&map, &addrs);
    let front = front_over(owners, Authorizers::allow_all(), None).await;
    Trio { fakes, map, front }
}

/// The digest a ByteStream name of `kind` and `size` names.
fn digest_of(kind: char, size: u64) -> Digest {
    Digest::parse(
        DigestFunction::Sha256,
        &format!("{}/{size}", harness::hash(kind)),
    )
    .expect("a digest")
}

/// A blob (its kind and size, which a name carries) that `fits`.
fn blob_where(fits: impl Fn(&Digest) -> bool) -> (char, u64) {
    for kind in ['0', '6', '7', '8', '9', 'a', 'b', 'c', 'd', 'e', 'f'] {
        for size in 1..200 {
            if fits(&digest_of(kind, size)) {
                return (kind, size);
            }
        }
    }
    panic!("no blob fits");
}

fn client(front: &harness::Front) -> ByteStreamClient<Channel> {
    ByteStreamClient::new(front.channel.clone())
}

fn one_message(name: String) -> impl futures::Stream<Item = WriteRequest> {
    stream::iter([WriteRequest {
        resource_name: name,
        write_offset: 0,
        finish_write: true,
        data: vec![pattern(0); CHUNK / 16],
    }])
}

/// An upload of one message small enough to be one HTTP/2 data frame: all of it is the
/// part of the request the proxy keeps to send again.
fn small_message(name: String) -> impl futures::Stream<Item = WriteRequest> {
    stream::iter([WriteRequest {
        resource_name: name,
        write_offset: 0,
        finish_write: true,
        data: vec![pattern(0); 1000],
    }])
}

/// Which of the nodes were called, by member number.
fn called(t: &Trio) -> Vec<usize> {
    t.fakes
        .iter()
        .zip(1..)
        .filter(|(f, _)| f.calls.load(SeqCst) > 0)
        .map(|(_, n)| n)
        .collect()
}

/// Catches an upload sent to the owner a read asks first. Member 1 is draining: it holds
/// the blob (so a read asks it first) and takes no new writes, so the map's writer is
/// another node, and the upload and a query of its status go there, never to the draining
/// one.
#[tokio::test(flavor = "multi_thread")]
async fn an_upload_and_its_status_go_to_the_writer_not_the_first_reader() {
    let t = trio([
        MemberState::Draining,
        MemberState::Active,
        MemberState::Active,
    ])
    .await;
    let (kind, size) = blob_where(|d| {
        t.map.read_order(d)[0] == MemberId(1) && t.map.write_owners(d)[0] != MemberId(1)
    });
    let writer = t.map.write_owners(&digest_of(kind, size))[0].0 as usize;

    client(&t.front)
        .write(Request::new(one_message(upload("main", kind, size))))
        .await
        .expect("written");
    assert_eq!(called(&t), [writer]);

    let status = client(&t.front)
        .query_write_status(QueryWriteStatusRequest {
            resource_name: upload("main", kind, size),
        })
        .await
        .expect("status");
    assert!(status.into_inner().complete);
    assert_eq!(called(&t), [writer]);
    assert_eq!(t.fakes[writer - 1].calls.load(SeqCst), 2);
}

/// Catches a read sent to the `--storage` node (here nobody), or to the owners in another
/// order than the map's: the node a read asks first lacks the blob and the next one has it.
#[tokio::test(flavor = "multi_thread")]
async fn a_read_asks_the_owners_in_the_maps_order() {
    let map = map([MemberState::Active; 3]);
    let (kind, size) = blob_where(|d| map.read_order(d).len() >= 2);
    let order = map.read_order(&digest_of(kind, size));
    let (first, second) = (order[0].0 as usize, order[1].0 as usize);
    // The first owner says it has no such blob.
    let mut fakes: Vec<_> = (0..3).map(|_| Fake::new(false)).collect();
    fakes[first - 1] = Fake::forcing('1');
    let mut addrs = Vec::new();
    for fake in &fakes {
        addrs.push(node(fake).await);
    }
    let owners = routed_owners(&map, &addrs);
    let front = front_over(owners, Authorizers::allow_all(), None).await;

    let mut stream = client(&front)
        .read(ReadRequest {
            resource_name: blob("main", kind, size),
            read_offset: 0,
            read_limit: 0,
        })
        .await
        .expect("read")
        .into_inner();
    while stream.message().await.expect("a message").is_some() {}
    assert_eq!(fakes[first - 1].calls.load(SeqCst), 1);
    assert_eq!(fakes[second - 1].calls.load(SeqCst), 1);
}

/// Catches a node left unable to tell the frontend's map is old, and a client that sets
/// the generation itself: a call the frontend routed carries the generation of its map, in
/// place of whatever the client sent; a call it did not route (no map) carries none.
#[tokio::test(flavor = "multi_thread")]
async fn a_call_carries_the_frontends_generation_not_the_clients() {
    let t = trio([MemberState::Active; 3]).await;
    let (kind, size) = blob_where(|_| true);
    let mut request = Request::new(QueryWriteStatusRequest {
        resource_name: upload("main", kind, size),
    });
    request
        .metadata_mut()
        .insert("kbf-map-generation", "99".parse().expect("metadata"));
    client(&t.front)
        .query_write_status(request)
        .await
        .expect("status");
    let seen: Vec<_> = t
        .fakes
        .iter()
        .flat_map(|f| f.seen.lock().expect("lock").clone())
        .collect();
    assert_eq!(seen.len(), 1);
    assert_eq!(
        seen[0]
            .metadata
            .get("kbf-map-generation")
            .map(|v| v.to_str().expect("text")),
        Some("7")
    );

    // A frontend with a fixed list of nodes tells them nothing of a map, and passes
    // nothing a client claimed.
    let fake = Fake::new(false);
    let plain = harness::front(
        vec![harness::owner("only", node(&fake).await)],
        Authorizers::allow_all(),
        None,
    )
    .await;
    let mut request = Request::new(QueryWriteStatusRequest {
        resource_name: upload("main", kind, size),
    });
    request
        .metadata_mut()
        .insert("kbf-map-generation", "99".parse().expect("metadata"));
    client(&plain)
        .query_write_status(request)
        .await
        .expect("status");
    let seen = fake.seen.lock().expect("lock").clone();
    assert!(seen[0].metadata.get("kbf-map-generation").is_none());
}

/// Catches a member the frontend cannot dial taking the call down with it, or a blob given
/// to a node the map did not name: a read passes over the owner it has no connection to and
/// is served by the next, and an upload whose writer it cannot dial goes to the blob's
/// other write owner (as the router's write passes over a member it cannot dial), never to
/// a member that does not own the blob.
#[tokio::test(flavor = "multi_thread")]
async fn a_member_that_cannot_be_dialled_is_passed_over() {
    let map = map([MemberState::Active; 3]);
    // Members 1 and 2 are nodes; member 3 has no connection.
    let fakes = [Fake::new(false), Fake::new(false)];
    let addrs = [node(&fakes[0]).await, node(&fakes[1]).await];
    let front = front_over(routed_owners(&map, &addrs), Authorizers::allow_all(), None).await;

    let (kind, size) = blob_where(|d| map.read_order(d)[0] == MemberId(3));
    let mut stream = client(&front)
        .read(ReadRequest {
            resource_name: blob("main", kind, size),
            read_offset: 0,
            read_limit: 0,
        })
        .await
        .expect("served by the next owner")
        .into_inner();
    while stream.message().await.expect("a message").is_some() {}
    assert_eq!(fakes.iter().map(|f| f.calls.load(SeqCst)).sum::<usize>(), 1);

    let before: Vec<_> = fakes.iter().map(|f| f.calls.load(SeqCst)).collect();
    let (kind, size) = blob_where(|d| map.write_owners(d)[0] == MemberId(3));
    let next = map.write_owners(&digest_of(kind, size))[1].0 as usize;
    client(&front)
        .write(Request::new(one_message(upload("main", kind, size))))
        .await
        .expect("the writer cannot be dialled: the other owner takes the upload");
    let after: Vec<_> = fakes.iter().map(|f| f.calls.load(SeqCst)).collect();
    let took: Vec<_> = after.iter().zip(&before).map(|(a, b)| a - b).collect();
    let mut want = vec![0, 0];
    want[next - 1] = 1;
    assert_eq!(took, want, "the upload went to the blob's other owner only");
}

/// A trio whose fakes are `fakes` and where member `dead` (1 to 3) is a closed port: a
/// node that is dead but that the map still names active.
async fn trio_with(
    fakes: Vec<Arc<Fake>>,
    dead: Option<usize>,
) -> (Vec<Arc<Fake>>, Arc<Map>, harness::Front) {
    let mut addrs = Vec::new();
    for (i, fake) in fakes.iter().enumerate() {
        addrs.push(if dead == Some(i + 1) {
            ([127, 0, 0, 1], 1).into()
        } else {
            node(fake).await
        });
    }
    let map = map([MemberState::Active; 3]);
    let front = front_over(routed_owners(&map, &addrs), Authorizers::allow_all(), None).await;
    (fakes, map, front)
}

fn calls(fakes: &[Arc<Fake>]) -> Vec<usize> {
    fakes.iter().map(|f| f.calls.load(SeqCst)).collect()
}

/// Catches an upload that fails for the whole member lease (15 s) when its writer is dead
/// and the map has not marked it down: the writer is a closed port, active in the map, and
/// the upload is answered by the blob's other write owner (and by nobody else), with the
/// bytes intact (the fake checks them) and the frontend's map generation. Mutant: the
/// retry loop in `RawByteStream::write` removed (`continue` dropped).
#[tokio::test(flavor = "multi_thread")]
async fn an_upload_goes_to_the_next_owner_when_the_writer_cannot_be_reached() {
    let probe = map([MemberState::Active; 3]);
    let (kind, size) = blob_where(|d| probe.write_owners(d)[0] == MemberId(2));
    let owners = probe.write_owners(&digest_of(kind, size));
    let (writer, next) = (owners[0].0 as usize, owners[1].0 as usize);
    let fakes: Vec<_> = (0..3).map(|_| Fake::new(false)).collect();
    let (fakes, _, front) = trio_with(fakes, Some(writer)).await;

    client(&front)
        .write(Request::new(one_message(upload("main", kind, size))))
        .await
        .expect("the next owner takes the upload");
    let mut want = vec![0, 0, 0];
    want[next - 1] = 1;
    assert_eq!(calls(&fakes), want);
    let seen = fakes[next - 1].seen.lock().expect("lock").clone();
    assert_eq!(seen[0].name, upload("main", kind, size));
    assert_eq!(
        seen[0]
            .metadata
            .get("kbf-map-generation")
            .map(|v| v.to_str().expect("text")),
        Some("7")
    );
}

/// Catches a retry that only handles a refused connection: a writer that is reached and
/// answers UNAVAILABLE before it reads anything (a node going away) is gone around too,
/// while the upload is still only the part of the request the proxy keeps (a small one).
/// The writer was called once, the other owner once.
#[tokio::test(flavor = "multi_thread")]
async fn an_upload_a_writer_answers_unavailable_is_sent_to_the_next_owner() {
    let probe = map([MemberState::Active; 3]);
    let (kind, size) = blob_where(|d| probe.write_owners(d)[0] == MemberId(1));
    let owners = probe.write_owners(&digest_of(kind, size));
    let (writer, next) = (owners[0].0 as usize, owners[1].0 as usize);
    let mut fakes: Vec<_> = (0..3).map(|_| Fake::new(false)).collect();
    fakes[writer - 1] = Fake::refusing_writes(tonic::Code::Unavailable);
    let (fakes, _, front) = trio_with(fakes, None).await;

    client(&front)
        .write(Request::new(small_message(upload("main", kind, size))))
        .await
        .expect("the next owner takes the upload");
    let mut want = vec![0, 0, 0];
    want[writer - 1] = 1;
    want[next - 1] = 1;
    assert_eq!(calls(&fakes), want);
}

/// Catches an answer passed over for the next owner: a writer that is reached and refuses
/// the upload (INVALID_ARGUMENT, as a node refuses a write it is not the primary of, or one
/// whose blob is wrong) is the answer, and the other owner is not called. Mutant: every
/// code retried.
#[tokio::test(flavor = "multi_thread")]
async fn a_writer_that_refuses_an_upload_is_not_gone_around() {
    let probe = map([MemberState::Active; 3]);
    let (kind, size) = blob_where(|d| probe.write_owners(d)[0] == MemberId(1));
    let writer = probe.write_owners(&digest_of(kind, size))[0].0 as usize;
    let mut fakes: Vec<_> = (0..3).map(|_| Fake::new(false)).collect();
    fakes[writer - 1] = Fake::refusing_writes(tonic::Code::InvalidArgument);
    let (fakes, _, front) = trio_with(fakes, None).await;

    let refused = client(&front)
        .write(Request::new(small_message(upload("main", kind, size))))
        .await
        .expect_err("refused by the writer");
    assert_eq!(refused.code(), tonic::Code::InvalidArgument);
    let mut want = vec![0, 0, 0];
    want[writer - 1] = 1;
    assert_eq!(calls(&fakes), want);
}

/// Catches an upload failed with the last owner's failure, or one that never tries the
/// second: both owners dead gives the first writer's failure (it names the node), no third
/// node is called. Mutant: the last failure returned.
#[tokio::test(flavor = "multi_thread")]
async fn an_upload_no_owner_can_take_fails_with_the_first_writers_failure() {
    let probe = map([MemberState::Active; 3]);
    let (kind, size) = blob_where(|d| probe.write_owners(d)[0] == MemberId(1));
    let owners = probe.write_owners(&digest_of(kind, size));
    let (writer, next) = (owners[0].0 as usize, owners[1].0 as usize);
    let mut fakes: Vec<_> = (0..3).map(|_| Fake::new(false)).collect();
    fakes[writer - 1] = Fake::refusing_writes(tonic::Code::Unavailable);
    // The other owner is a closed port.
    let (fakes, _, front) = trio_with(fakes, Some(next)).await;

    let refused = client(&front)
        .write(Request::new(small_message(upload("main", kind, size))))
        .await
        .expect_err("no owner takes it");
    assert_eq!(refused.code(), tonic::Code::Unavailable);
    // The first writer answered with its own message; the dead one would name its node.
    assert_eq!(refused.message(), "refused");
    let mut want = vec![0, 0, 0];
    want[writer - 1] = 1;
    assert_eq!(calls(&fakes), want);
}

/// Catches an upload sent on after bytes of it went to the first owner: the writer reads
/// the first message of a three-message upload, then fails UNAVAILABLE. The proxy no
/// longer has the bytes it passed, so the call ends with that failure (the client starts it
/// again), and the other owner is never called with half a blob. Mutant: `Upload::again`
/// ignoring that a frame past the first ones was read.
#[tokio::test(flavor = "multi_thread")]
async fn an_upload_that_was_passed_on_is_not_sent_to_another_owner() {
    let probe = map([MemberState::Active; 3]);
    let (kind, size) = blob_where(|d| probe.write_owners(d)[0] == MemberId(1));
    let writer = probe.write_owners(&digest_of(kind, size))[0].0 as usize;
    let mut fakes: Vec<_> = (0..3).map(|_| Fake::new(false)).collect();
    fakes[writer - 1] = Fake::failing_writes_after(1);
    let (fakes, _, front) = trio_with(fakes, None).await;

    let message = |i: usize, last: bool| WriteRequest {
        resource_name: if i == 0 {
            upload("main", kind, size)
        } else {
            String::new()
        },
        write_offset: (i * CHUNK) as i64,
        finish_write: last,
        data: vec![pattern(i); CHUNK],
    };
    let failed = client(&front)
        .write(Request::new(stream::iter([
            message(0, false),
            message(1, false),
            message(2, true),
        ])))
        .await
        .expect_err("the writer failed mid-upload");
    assert_eq!(failed.code(), tonic::Code::Unavailable);
    let mut want = vec![0, 0, 0];
    want[writer - 1] = 1;
    assert_eq!(calls(&fakes), want);
}

/// Catches a frontend that loses its map between deciding to route and routing and then
/// guesses a node: the upload is UNAVAILABLE.
#[tokio::test(flavor = "multi_thread")]
async fn a_map_lost_mid_call_is_unavailable() {
    let map = map([MemberState::Active; 3]);
    let fake = Fake::new(false);
    let addr = node(&fake).await;
    let maps = Arc::new(harness::Flicker {
        map,
        looks: std::sync::atomic::AtomicUsize::new(0),
    });
    let owners = harness::routed_owners_over(maps, &[addr, addr, addr]);
    let front = front_over(owners, Authorizers::allow_all(), None).await;

    let refused = client(&front)
        .write(Request::new(one_message(upload("main", '0', 7))))
        .await
        .expect_err("no map");
    assert_eq!(refused.code(), tonic::Code::Unavailable);
    assert_eq!(fake.calls.load(SeqCst), 0);
}

/// A map the test moves, and never has anything newer to fetch.
struct Switch(Mutex<Arc<Map>>);

impl MapSource for Switch {
    fn current(&self) -> Option<MapView> {
        Some(MapView {
            map: Arc::clone(&self.0.lock().unwrap_or_else(PoisonError::into_inner)),
            previous: None,
        })
    }

    fn refetch(&self, at_least: u64) -> BoxFuture<'_, Result<MapView, RefetchError>> {
        Box::pin(async move {
            Err(RefetchError {
                at_least,
                reason: "nothing newer".to_owned(),
            })
        })
    }
}

/// Catches an upload that goes on to a node that is no longer the blob's writer: the
/// guard on the body asks the map, not a fixed list, who the writer is. Member 1 is the
/// writer when the upload begins and goes down after its first message, so the next
/// message ends the call UNAVAILABLE (the client starts it again) and the frontend sends
/// the rest of the blob to no other node.
#[tokio::test(flavor = "multi_thread")]
async fn an_upload_ends_unavailable_when_the_map_moves_its_writer() {
    let before = map([MemberState::Active; 3]);
    let after = map([MemberState::Down, MemberState::Active, MemberState::Active]);
    let (kind, size) = blob_where(|d| before.write_owners(d)[0] == MemberId(1));
    let switch = Arc::new(Switch(Mutex::new(Arc::clone(&before))));
    let fakes: Vec<_> = (0..3).map(|_| Fake::new(false)).collect();
    let mut addrs = Vec::new();
    for fake in &fakes {
        addrs.push(node(fake).await);
    }
    let owners = routed_owners_over(switch.clone(), &addrs);
    let front = front_over(owners, Authorizers::allow_all(), None).await;

    let (tx, rx) = tokio::sync::mpsc::channel::<WriteRequest>(1);
    let sent = stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|m| (m, rx)) });
    let mut c = client(&front);
    let call = tokio::spawn(async move { c.write(Request::new(sent)).await });
    let message = |i: usize, last: bool| WriteRequest {
        resource_name: if i == 0 {
            upload("main", kind, size)
        } else {
            String::new()
        },
        write_offset: (i * CHUNK) as i64,
        finish_write: last,
        data: vec![pattern(i); CHUNK],
    };
    tx.send(message(0, false)).await.expect("sent");
    tx.send(message(1, false)).await.expect("sent");
    assert!(eventually(|| fakes[0].lead.consumed.load(SeqCst) >= 2 * CHUNK as u64).await);

    *switch.0.lock().unwrap_or_else(PoisonError::into_inner) = after;
    tx.send(message(2, false)).await.expect("sent");
    // Were the upload to go on, it would end here, and the assertions below say so.
    let _ = tx.send(message(3, true)).await;
    let refused = call.await.expect("joined").expect_err("the writer changed");
    assert_eq!(refused.code(), tonic::Code::Unavailable);
    assert!(
        refused.message().contains("during the upload"),
        "{}",
        refused.message()
    );
    assert_eq!(
        fakes[1..]
            .iter()
            .map(|f| f.calls.load(SeqCst))
            .sum::<usize>(),
        0,
        "the frontend does not retry a write elsewhere"
    );
}
