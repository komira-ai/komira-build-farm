//! The gate: which digests a node flushes under the map it holds. Each test's comment
//! names the defect it catches.

use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use kbf_map::{Map, Member, MemberId, MemberState, Rule};
use kbf_types::Digest;

use super::{FlushGate, OneFlusher};
use crate::replicate::MapSource;

fn digests() -> Vec<Digest> {
    (0..64u8).map(|i| kbf_segments::sha256(&[i])).collect()
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

fn map(generation: u64, members: &[(u64, MemberState)]) -> Arc<Map> {
    let members = members.iter().map(|&(id, s)| member(id, s)).collect();
    Arc::new(Map::new(generation, members, Rule::Rendezvous, 2, 1, Vec::new()).expect("map"))
}

/// A node's source: the map it holds, its id (if registered), and what a fetch brings.
struct Node {
    me: Option<MemberId>,
    held: Mutex<Option<Arc<Map>>>,
    /// The map a refetch delivers, if any.
    fetch: Mutex<Option<Arc<Map>>>,
}

impl Node {
    fn new(me: Option<u64>, held: Option<Arc<Map>>) -> Arc<Self> {
        Arc::new(Self {
            me: me.map(MemberId),
            held: Mutex::new(held),
            fetch: Mutex::new(None),
        })
    }
}

impl MapSource for Node {
    fn current(&self) -> Option<Arc<Map>> {
        self.held.lock().expect("lock").clone()
    }

    fn me(&self) -> Option<MemberId> {
        self.me
    }

    fn refetch(&self, _at_least: u64) -> BoxFuture<'_, Option<Arc<Map>>> {
        let fetched = self.fetch.lock().expect("lock").clone();
        if let Some(map) = &fetched {
            *self.held.lock().expect("lock") = Some(Arc::clone(map));
        }
        Box::pin(async move { fetched })
    }
}

const BOTH: [(u64, MemberState); 2] = [(1, MemberState::Active), (2, MemberState::Active)];

/// How many of the digests each of the two nodes flushes under `map`.
fn split(map: &Arc<Map>) -> (usize, usize) {
    let one = OneFlusher::new(Node::new(Some(1), Some(Arc::clone(map))));
    let two = OneFlusher::new(Node::new(Some(2), Some(Arc::clone(map))));
    let ds = digests();
    (
        ds.iter().filter(|d| one.flushes(d)).count(),
        ds.iter().filter(|d| two.flushes(d)).count(),
    )
}

/// Catches: both owners flushing (the double PUT), or neither: every digest has exactly
/// one flusher among the two members, and both members flush some.
#[test]
fn every_digest_has_exactly_one_flusher() {
    let map = map(5, &BOTH);
    let (a, b) = split(&map);
    assert_eq!(a + b, digests().len());
    assert!(a > 0 && b > 0, "{a} {b}");
}

/// Catches: a node that flushes without a map, or without knowing its id (a late flush
/// is cheap, a wrong one is a second PUT).
#[test]
fn a_node_without_a_map_or_an_id_flushes_nothing() {
    let ds = digests();
    let no_map = OneFlusher::new(Node::new(Some(1), None));
    let no_id = OneFlusher::new(Node::new(None, Some(map(5, &BOTH))));
    assert!(ds.iter().all(|d| !no_map.flushes(d) && !no_id.flushes(d)));
}

/// Catches: a replica that takes rank 1 while the primary is only `Down` (inside the
/// dwell): the primary keeps its slot, so the replica still holds. And that, once the
/// member has left the map, the replica flushes everything the primary never did.
#[test]
fn the_replica_takes_over_only_when_the_primary_has_left_the_map() {
    let down = map(6, &[(1, MemberState::Down), (2, MemberState::Active)]);
    let (a, b) = split(&down);
    assert_eq!(
        a + b,
        digests().len(),
        "a down primary still owns its slice"
    );
    let ds = digests();
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
    let two = OneFlusher::new(Node::new(Some(2), Some(alone)));
    assert!(ds.iter().all(|d| two.flushes(d)));
}

/// Catches: a node behind the map flushing as rank 1 after a caller showed it a newer
/// generation exists (a second flusher beside the real primary). It holds until its map
/// is at least as new, then judges by that one.
#[tokio::test]
async fn a_node_told_it_is_behind_flushes_nothing_until_it_has_the_new_map() {
    let old = map(5, &BOTH);
    let ds = digests();
    let source = Node::new(Some(1), Some(Arc::clone(&old)));
    let gate = OneFlusher::new(Arc::clone(&source) as Arc<dyn MapSource>);
    let mine: Vec<&Digest> = ds.iter().filter(|d| gate.flushes(d)).collect();
    assert!(!mine.is_empty());
    // A caller routed by generation 6; the node cannot get it.
    assert!(gate.refetch(6).await.is_none());
    assert!(mine.iter().all(|d| !gate.flushes(d)));
    // The map arrives: the same node is judged by it, member 1 now only Draining.
    let new = map(6, &[(1, MemberState::Draining), (2, MemberState::Active)]);
    *source.fetch.lock().expect("lock") = Some(Arc::clone(&new));
    assert_eq!(gate.refetch(6).await.expect("fetched").generation(), 6);
    assert!(mine.iter().all(|d| !gate.flushes(d)), "no longer rank 1");
    assert_eq!(gate.current().expect("held").generation(), 6);
    assert_eq!(gate.me(), Some(MemberId(1)));
}

/// The gate is `Debug` (the cache that holds it is), and says how new a map it was told of.
#[test]
fn the_gate_formats_with_the_newest_generation_it_was_told_of() {
    let gate = OneFlusher::new(Node::new(Some(1), None));
    assert!(format!("{gate:?}").contains("newest: 0"), "{gate:?}");
}
