//! A node that is catching up, served for real (the storage service over loopback, called
//! through [`RemoteStorage`]): its misses are UNAVAILABLE, so the router asks the next
//! owner instead of believing them.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;

use kbf_map::{Member, MemberId, MemberState};
use kbf_route::{Nodes, Router};
use kbf_storage::service::{Admission, CatchUp, Config, RemoteStorage, StorageService};
use kbf_storage::{Storage, StorageError};
use tonic::transport::server::TcpIncoming;
use tonic::transport::{Channel, Server};

use super::support::{Cell, Node, blob_written_to, map, member, view};

/// Nodes served over plain-text loopback, by member id.
struct Served(BTreeMap<MemberId, Channel>);

impl Nodes for Served {
    fn node(&self, member: &Member, generation: u64) -> Result<Arc<dyn Storage>, StorageError> {
        let channel = self.0.get(&member.id).cloned().expect("a served node");
        Ok(Arc::new(RemoteStorage::new(channel, generation)))
    }
}

/// `node` behind a storage service whose catch-up state is `catch_up`.
async fn serve(node: &Arc<Node>, catch_up: &CatchUp) -> Channel {
    let storage: Arc<dyn Storage> = node.clone();
    let config = Config {
        write_bytes_in_flight: 1 << 20,
    };
    let service =
        StorageService::new(storage, Admission::Open, config).with_catch_up(catch_up.clone());
    let incoming = TcpIncoming::bind(SocketAddr::from(([127, 0, 0, 1], 0))).expect("bind");
    let addr = incoming.local_addr().expect("address");
    tokio::spawn(
        Server::builder()
            .add_routes(service.routes())
            .serve_with_incoming(incoming),
    );
    Channel::from_shared(format!("http://{addr}"))
        .expect("endpoint")
        .connect()
        .await
        .expect("connect")
}

/// A router over `members` at replication `r`, where member 1 is a node that is catching
/// up and holds nothing yet, and member 2 (if listed) a caught-up node; and both nodes.
async fn farm(r: u8, members: Vec<Member>) -> (Router, Arc<Node>, Arc<Node>, Arc<kbf_map::Map>) {
    let (rebuilding, settled) = (Node::new(), Node::new());
    let mut channels = BTreeMap::new();
    channels.insert(
        MemberId(1),
        serve(&rebuilding, &CatchUp::rebuilding()).await,
    );
    channels.insert(MemberId(2), serve(&settled, &CatchUp::caught_up()).await);
    let map = map(1, r, members);
    let router = Router::new(Cell::with(view(&map)), Arc::new(Served(channels)));
    (router, rebuilding, settled, map)
}

/// Catches a router that believes a rebuilding node's miss, through the node's real
/// service. With R = 2 and a blob whose primary is the rebuilding node (the router's map
/// does not know yet that the node is catching up, so it is listed Active and asked
/// first), the read is served by the other owner and FindMissing does not call the blob
/// missing; the rebuilding node was asked both times. With the map saying CatchingUp,
/// the same holds. With R = 1 and the rebuilding node the only owner, the read is
/// UNAVAILABLE, not NOT_FOUND, and FindMissing is UNAVAILABLE, not a miss. Mutants (in
/// the service): the gate removed (FindMissing calls the blob missing; the R = 1 read is
/// NOT_FOUND); a gate that drops only the missed digests from FindMissing (the R = 1
/// FindMissing answers "held").
#[tokio::test]
async fn no_false_miss_through_the_router_during_catch_up() {
    let members = vec![
        member(1, MemberState::Active),
        member(2, MemberState::Active),
    ];
    let (router, rebuilding, settled, map) = farm(2, members).await;
    let (bytes, digest) = blob_written_to(&map, &[1, 2]);
    settled.hold(&bytes);

    let read = router
        .read_blob(&digest)
        .await
        .expect("served by the other owner");
    assert_eq!(&read[..], &bytes[..]);
    assert_eq!(router.find_missing(&[digest]).await.expect("an answer"), []);
    assert_eq!(
        rebuilding.read_count(),
        1,
        "the rebuilding node was asked first"
    );
    assert_eq!(rebuilding.find_log(), [vec![digest]]);

    let members = vec![
        member(1, MemberState::CatchingUp),
        member(2, MemberState::Active),
    ];
    let (router, _, settled, map) = farm(2, members).await;
    let (bytes, digest) = blob_written_to(&map, &[2, 1]);
    settled.hold(&bytes);
    let read = router.read_blob(&digest).await.expect("served");
    assert_eq!(&read[..], &bytes[..]);
    assert_eq!(router.find_missing(&[digest]).await.expect("an answer"), []);

    let (router, rebuilding, _, _) = farm(1, vec![member(1, MemberState::Active)]).await;
    let digest = super::support::sha(b"held in cold storage, not yet indexed");
    let read = router.read_blob(&digest).await;
    assert!(
        matches!(read, Err(StorageError::Unavailable(_))),
        "a sole rebuilding owner: {read:?}"
    );
    let found = router.find_missing(&[digest]).await;
    assert!(
        matches!(found, Err(StorageError::Unavailable(_))),
        "FindMissing of a sole rebuilding owner: {found:?}"
    );
    assert_eq!(rebuilding.find_log(), [vec![digest]]);
}
