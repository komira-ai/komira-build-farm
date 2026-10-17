//! One flusher (`docs/design/architecture-v2.md`, section 8) at the write tier: of the
//! two owners of a digest, only the primary PUTs its staged object. Two caches stand for
//! the two storage nodes of an R = 2 map, each with its own tier, log and store, each
//! holding a copy of the same blobs, each gated by `kbf_storage::flush::OneFlusher` over
//! the map it holds. A test's comment names the defect it catches.

mod common;
#[allow(dead_code)]
#[path = "write_tier/support.rs"]
mod support;

use std::sync::Arc;

use kbf_map::{Map, Member, MemberId, MemberState, Rule};
use kbf_meta::StagedState;
use kbf_storage::flush::OneFlusher;
use kbf_storage::replicate::MapSource;
use support::{Farm, SEGMENT, TestBlob, later};

const GIB: u64 = 1 << 30;

/// A node's view of the farm: its id and the map the test last gave it.
struct Node {
    me: MemberId,
    map: std::sync::Mutex<Option<Arc<Map>>>,
}

impl Node {
    fn new(me: u64, map: &Arc<Map>) -> Arc<Self> {
        Arc::new(Self {
            me: MemberId(me),
            map: std::sync::Mutex::new(Some(Arc::clone(map))),
        })
    }

    fn set(&self, map: &Arc<Map>) {
        *self.map.lock().expect("lock") = Some(Arc::clone(map));
    }
}

impl MapSource for Node {
    fn current(&self) -> Option<Arc<Map>> {
        self.map.lock().expect("lock").clone()
    }

    fn me(&self) -> Option<MemberId> {
        Some(self.me)
    }
}

fn member(id: u64, state: MemberState) -> Member {
    Member {
        id: MemberId(id),
        addr: format!("127.0.0.1:{id}"),
        zone: String::new(),
        weight: 1,
        state,
    }
}

fn two(generation: u64, state_of_one: MemberState) -> Arc<Map> {
    let members = vec![member(1, state_of_one), member(2, MemberState::Active)];
    Arc::new(Map::new(generation, members, Rule::Rendezvous, 2, 1, Vec::new()).expect("map"))
}

/// The two nodes of an R = 2 farm, and `n` blobs of one object each that member 1 is the
/// primary of, held by both.
struct Pair {
    primary: Farm,
    replica: Farm,
    primary_cache: support::TierCache,
    replica_cache: support::TierCache,
    replica_node: Arc<Node>,
    primary_node: Arc<Node>,
    primary_gate: Arc<OneFlusher>,
    blobs: Vec<TestBlob>,
}

impl Pair {
    async fn new(n: usize, map: &Arc<Map>) -> Self {
        let len = usize::try_from(SEGMENT).expect("size") + 1;
        let mut blobs = Vec::new();
        let mut seed = 1;
        while blobs.len() < n {
            let blob = TestBlob::new(seed, len);
            seed += 1;
            if map.write_owners(&blob.digest)[0] == MemberId(1) {
                blobs.push(blob);
            }
        }
        let (primary, replica) = (Farm::new(Default::default()), Farm::new(Default::default()));
        let primary_node = Node::new(1, map);
        let replica_node = Node::new(2, map);
        let primary_gate = OneFlusher::new(Arc::clone(&primary_node) as _);
        let primary_cache = primary
            .start(GIB)
            .await
            .with_flush_gate(Arc::clone(&primary_gate) as _);
        let replica_cache = replica
            .start(GIB)
            .await
            .with_flush_gate(OneFlusher::new(Arc::clone(&replica_node) as _));
        for cache in [&primary_cache, &replica_cache] {
            for b in &blobs {
                cache.store_blobs(vec![b.verified()]).await.expect("stored");
            }
        }
        Self {
            primary,
            replica,
            primary_cache,
            replica_cache,
            replica_node,
            primary_node,
            primary_gate,
            blobs,
        }
    }
}

/// Catches: the replica flushing its copy while the primary is live (a second PUT of
/// every blob into another object: double PUT bytes and cold-store cost). The replica
/// must PUT nothing, keep every object staged with its block, and still serve the blobs;
/// the primary PUTs each blob once.
#[tokio::test]
async fn a_replica_does_not_put_while_the_primary_is_live() {
    let map = two(5, MemberState::Active);
    let pair = Pair::new(3, &map).await;
    let round = pair.replica_cache.flush_tier(later()).await.expect("round");
    assert_eq!((round.flushed, round.retrying), (vec![], vec![]));
    assert_eq!(pair.replica.store.put_keys(), []);
    for b in &pair.blobs {
        let object = pair.replica.log.object_of(&b.digest);
        assert_eq!(
            pair.replica.log.staged_state(object),
            Some(StagedState::Sealed)
        );
        assert!(pair.replica.has_block(object) && !pair.replica.has_copy(object));
        let read = pair.replica_cache.read_blob(&b.digest).await.expect("read");
        assert_eq!(read, b.bytes);
    }
    let round = pair.primary_cache.flush_tier(later()).await.expect("round");
    assert_eq!(round.flushed.len(), 3);
    assert_eq!(pair.primary.store.put_keys().len(), 3);
    assert_eq!(pair.replica.store.put_keys(), []);
}

/// Catches: a replica that never takes over. The primary dies before it flushes; its
/// dwell passes and the map drops it; under the new map the replica is rank 1, flushes
/// what the primary never did, and no blob is lost. Inside the dwell (the primary only
/// `Down`) it still holds.
#[tokio::test]
async fn the_replica_flushes_when_the_primary_leaves_the_map() {
    let map = two(5, MemberState::Active);
    let pair = Pair::new(2, &map).await;
    // The primary is down but keeps its slot for the dwell.
    pair.replica_node.set(&two(6, MemberState::Down));
    let round = pair.replica_cache.flush_tier(later()).await.expect("round");
    assert_eq!(round.flushed, []);
    assert_eq!(pair.replica.store.put_keys(), []);
    // Past the dwell the member has left the map.
    let alone = Arc::new(
        Map::new(
            7,
            vec![member(2, MemberState::Active)],
            Rule::Rendezvous,
            1,
            1,
            vec![],
        )
        .expect("map"),
    );
    pair.replica_node.set(&alone);
    let round = pair.replica_cache.flush_tier(later()).await.expect("round");
    assert_eq!(round.flushed.len(), 2);
    assert_eq!(pair.replica.store.put_keys().len(), 2);
    for b in &pair.blobs {
        let object = pair.replica.log.object_of(&b.digest);
        assert_eq!(pair.replica.log.staged_state(object), None);
        assert_eq!(
            pair.replica_cache.read_blob(&b.digest).await.expect("read"),
            b.bytes
        );
    }
}

/// Catches: a node that is behind the map flushing as rank 1 beside the real primary.
/// A caller routed by generation 6 shows the primary (which holds generation 5) that its
/// map is old and cannot be fetched; its round PUTs nothing, though by the map it holds
/// it is the primary of every blob. Not one blob has two flushers at any point.
#[tokio::test]
async fn a_node_behind_the_map_does_not_become_a_second_flusher() {
    let map = two(5, MemberState::Active);
    let pair = Pair::new(2, &map).await;
    assert!(pair.primary_gate.refetch(6).await.is_none());
    let round = pair.primary_cache.flush_tier(later()).await.expect("round");
    assert_eq!(round.flushed, []);
    assert_eq!(pair.primary.store.put_keys(), []);
    // Generation 6 reaches it and places the blobs on the same primary again: it
    // flushes them, once.
    pair.primary_node.set(&two(6, MemberState::Active));
    let round = pair.primary_cache.flush_tier(later()).await.expect("round");
    assert_eq!(round.flushed.len(), 2);
    assert_eq!(pair.primary.store.put_keys().len(), 2);
}

/// A gate that flushes only the digests it was given.
#[derive(Debug)]
struct Only(Vec<kbf_types::Digest>);

impl kbf_storage::flush::FlushGate for Only {
    fn flushes(&self, d: &kbf_types::Digest) -> bool {
        self.0.contains(d)
    }
}

/// Catches: a flush queue that takes its round's oldest sealed objects before it drops
/// the held ones: with more held objects than a round takes (four), the one object this
/// node flushes would never be reached.
#[tokio::test]
async fn held_objects_do_not_starve_the_one_this_node_flushes() {
    let farm = Farm::new(Default::default());
    let len = usize::try_from(SEGMENT).expect("size") + 1;
    let blobs: Vec<TestBlob> = (1..=5).map(|s| TestBlob::new(s, len)).collect();
    let cache = farm
        .start(GIB)
        .await
        .with_flush_gate(Arc::new(Only(vec![blobs[4].digest])));
    for b in &blobs {
        cache.store_blobs(vec![b.verified()]).await.expect("stored");
    }
    let last = farm.log.object_of(&blobs[4].digest);
    let round = cache.flush_tier(later()).await.expect("round");
    assert_eq!(round.flushed, [last]);
    assert_eq!(farm.store.put_keys().len(), 1);
}

/// One staged object holds blobs of both ranks: it is flushed whole when this node
/// flushes any of its digests (the others ride along, a duplicate PUT bounded by the
/// object), and held when it flushes none. Not flushing it would leave this node's own
/// digests unflushed for good.
#[tokio::test]
async fn an_object_with_one_digest_to_flush_is_flushed_whole() {
    let farm = Farm::new(Default::default());
    let blobs = support::blobs(1, 2, 4096);
    let cache = farm
        .start(GIB)
        .await
        .with_flush_gate(Arc::new(Only(vec![blobs[1].digest])));
    cache
        .store_blobs(support::verified(&blobs))
        .await
        .expect("stored");
    let object = farm.log.object_of(&blobs[0].digest);
    assert_eq!(object, farm.log.object_of(&blobs[1].digest));
    let round = cache.flush_tier(later()).await.expect("round");
    assert_eq!(round.flushed, [object]);
}
