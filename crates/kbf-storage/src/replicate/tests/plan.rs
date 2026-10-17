//! Where a write goes under a map, without a network: the placement table.

use std::sync::Arc;
use std::time::Duration;

use kbf_map::{MemberId, MemberState};
use kbf_types::Digest;

use super::farm::Farm;
use crate::StorageError;
use crate::replicate::{ChainConfig, MapSource, Plan, View};
use crate::service::tests::fake::Held;

fn view(farm: &Farm, map: &Arc<kbf_map::Map>, node: usize) -> View {
    View {
        map: Arc::clone(map),
        me: farm.nodes[node].id,
    }
}

fn placed(farm: &Farm, map: &kbf_map::Map) -> (Digest, usize, usize) {
    let bytes = farm.blob(map, 0, 1, 64, 1);
    let digest = kbf_segments::sha256(&bytes);
    (digest, 0, 1)
}

/// Catches a node that takes a write it is not meant to: the primary plans a copy to the
/// replica (named with the address the map gives it); the replica, a third node, and a
/// non-owner receiving a client write refuse it; a copy is taken by an owner that is not
/// the sender and refused by a non-owner or by the sender itself.
#[tokio::test]
async fn each_node_has_one_part_in_a_write() {
    let farm = Farm::start(3, ChainConfig::default()).await;
    let map = farm.map(1, 2, &[]);
    let (digest, primary, replica) = placed(&farm, &map);
    let chain = &farm.nodes[primary].chain;
    let plan = chain
        .plan(&view(&farm, &map, primary), &digest, None)
        .expect("plan");
    let Plan::Primary { replica: member } = plan else {
        panic!("{plan:?}");
    };
    assert_eq!(member.id, farm.nodes[replica].id);
    assert_eq!(member.addr, farm.nodes[replica].switch.addr.to_string());
    let refused = |node: usize, leg: Option<MemberId>| {
        farm.nodes[node]
            .chain
            .plan(&view(&farm, &map, node), &digest, leg)
    };
    // The third node owns nothing of the blob; the second owner takes a client write as
    // the primary, with the first owner as the copy.
    assert!(
        matches!(refused(2, None), Err(StorageError::Invalid(m)) if m.contains("not the primary"))
    );
    let Plan::Primary { replica: copy_to } = refused(replica, None).expect("the second owner")
    else {
        panic!("the second owner does not take a client write");
    };
    assert_eq!(copy_to.id, farm.nodes[primary].id);
    assert_eq!(
        refused(replica, Some(farm.nodes[primary].id)).expect("a copy"),
        Plan::Local
    );
    assert!(matches!(
        refused(2, Some(farm.nodes[primary].id)),
        Err(StorageError::Invalid(_))
    ));
    assert!(matches!(
        refused(replica, Some(farm.nodes[replica].id)),
        Err(StorageError::Invalid(_))
    ));
}

/// Catches a replication factor the chain cannot serve taken anyway: R = 1 is the primary
/// alone (no leg), R = 3 is refused, and a map with fewer write owners than its factor
/// (two of three draining) is UNAVAILABLE, not a single copy.
#[tokio::test]
async fn the_replication_factor_decides_whether_there_is_a_replica() {
    let farm = Farm::start(3, ChainConfig::default()).await;
    let one = farm.map(1, 1, &[]);
    let (digest, primary, _) = placed(&farm, &farm.map(1, 2, &[]));
    let owner = farm.primary_of(&one, &digest);
    let alone = farm.nodes[owner]
        .chain
        .plan(&view(&farm, &one, owner), &digest, None);
    assert_eq!(alone.expect("plan"), Plan::Local);
    let three = farm.map(1, 3, &[]);
    let answer = farm.nodes[primary]
        .chain
        .plan(&view(&farm, &three, primary), &digest, None);
    assert!(matches!(answer, Err(StorageError::Invalid(m)) if m.contains("replication factor")));
    let thin = farm.map(
        1,
        2,
        &[(0, MemberState::Draining), (1, MemberState::Draining)],
    );
    let answer = farm.nodes[2]
        .chain
        .plan(&view(&farm, &thin, 2), &digest, None);
    let Err(StorageError::Unavailable(why)) = answer else {
        panic!("{answer:?}");
    };
    assert!(why.to_string().contains("1 write owners"), "{why}");
}

/// Catches a chain node with no map serving a write (it is one of several, so it fails
/// closed: UNAVAILABLE for any generation), and the view of a node with a map being taken
/// for a stale one.
#[tokio::test]
async fn a_node_with_no_map_has_no_view() {
    let farm = Farm::start(2, ChainConfig::default()).await;
    let none = crate::replicate::Chain::new(
        Held::none() as Arc<dyn MapSource>,
        crate::replicate::Peers::plain(),
        Arc::clone(&farm.nodes[0].repairs),
        ChainConfig::default(),
    );
    let wait = Duration::from_millis(10);
    for generation in [0, 1] {
        assert!(matches!(
            none.view(generation, wait).await,
            Err(StorageError::Unavailable(_))
        ));
    }
    farm.nodes[0].chain.view(1, wait).await.expect("current");
}

/// Catches a placement re-check that is too strict or too loose: under a newer map that
/// places the blob the same way the write stands; a different replica, or this node no
/// longer an owner, is STALE_MAP naming the newer generation; the same generation, or no
/// map, stands.
#[tokio::test]
async fn a_placement_is_confirmed_only_if_the_new_map_keeps_it() {
    let farm = Farm::start(3, ChainConfig::default()).await;
    let old = farm.map(1, 2, &[]);
    let (digest, primary, _) = placed(&farm, &old);
    let chain = &farm.nodes[primary].chain;
    let old_view = view(&farm, &old, primary);
    let plan = chain.plan(&old_view, &digest, None).expect("plan");
    // Nothing has changed.
    assert!(chain.confirm(&old_view, &digest, None, &plan).is_ok());
    // A newer generation, the same placement.
    farm.publish(&farm.map(2, 2, &[]));
    assert!(chain.confirm(&old_view, &digest, None, &plan).is_ok());
    // The replica leaves: a different second owner.
    farm.publish(&farm.map(3, 2, &[(1, MemberState::Draining)]));
    let answer = chain.confirm(&old_view, &digest, None, &plan);
    assert!(
        matches!(answer, Err(StorageError::StaleMap { current: 3 })),
        "{answer:?}"
    );
    // This node leaves: it is no owner.
    farm.publish(&farm.map(4, 2, &[(0, MemberState::Draining)]));
    let answer = chain.confirm(&old_view, &digest, None, &plan);
    assert!(
        matches!(answer, Err(StorageError::StaleMap { current: 4 })),
        "{answer:?}"
    );
}

/// Catches a down primary leaving its blobs unwritable, or the wrong node taking over: with
/// the primary down in the map, the next owner plans the write with the primary as the
/// copy owed (not called); while the primary is up the next owner plans it the same way,
/// with the primary as the copy to send (a frontend whose write to a dead primary the map
/// has not marked down yet goes to the next owner).
#[tokio::test]
async fn the_next_owner_takes_a_write_whose_primary_is_down() {
    let farm = Farm::start(3, ChainConfig::default()).await;
    let up = farm.map(1, 2, &[]);
    let (digest, primary, replica) = placed(&farm, &up);
    let down = farm.map(2, 2, &[(primary, MemberState::Down)]);
    let plan = farm.nodes[replica]
        .chain
        .plan(&view(&farm, &down, replica), &digest, None)
        .expect("plan");
    let Plan::Primary { replica: owed } = plan else {
        panic!("{plan:?}");
    };
    assert_eq!(
        (owed.id, owed.state),
        (farm.nodes[primary].id, MemberState::Down)
    );
    // While the map has the primary up the next owner still takes it: a client that
    // could not reach the primary has not been told by the map yet, and the primary is
    // then the copy that goes to it, owed if it cannot be reached.
    let plan = farm.nodes[replica]
        .chain
        .plan(&view(&farm, &up, replica), &digest, None)
        .expect("the second owner takes a client write");
    let Plan::Primary { replica: copy_to } = plan else {
        panic!("{plan:?}");
    };
    assert_eq!(
        (copy_to.id, copy_to.state),
        (farm.nodes[primary].id, MemberState::Active)
    );
}

/// A node that holds a map but has not been told which member it is.
struct Unregistered(Arc<kbf_map::Map>);

impl MapSource for Unregistered {
    fn current(&self) -> Option<Arc<kbf_map::Map>> {
        Some(Arc::clone(&self.0))
    }

    fn me(&self) -> Option<MemberId> {
        None
    }
}

/// Catches a node that does not know which member it is taking part in a chain: it cannot
/// tell whether it is the primary, so a write is UNAVAILABLE (naming why) rather than
/// served as a lone node, and it has no view to repair from either.
#[tokio::test]
async fn a_node_that_has_not_registered_cannot_take_part() {
    let farm = Farm::start(2, ChainConfig::default()).await;
    let source = Unregistered(farm.map(1, 2, &[]));
    let chain = crate::replicate::Chain::new(
        Arc::new(source),
        crate::replicate::Peers::plain(),
        Arc::clone(&farm.nodes[0].repairs),
        ChainConfig::default(),
    );
    let answer = chain.view(1, Duration::from_millis(10)).await;
    let Err(StorageError::Unavailable(why)) = answer else {
        panic!("{answer:?}");
    };
    assert!(
        why.to_string().contains("not been told its member id"),
        "{why}"
    );
    assert!(chain.current().is_none());
}
