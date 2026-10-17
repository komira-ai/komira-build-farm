//! The members' map as the frontend's routing map (`kbf_server::front_maps::FrontMaps`):
//! what `current` says before and after members register, and that a `refetch` reads the
//! state store, so a bump another frontend made is found when a storage node says its map
//! is newer.

#[path = "support/hooked.rs"]
mod hooked;

use std::sync::Arc;
use std::sync::atomic::Ordering::SeqCst;
use std::time::Duration;

use hooked::Hooked;
use kbf_route::MapSource as _;
use kbf_server::front_maps::FrontMaps;
use kbf_server::members::{Members, MembershipConfig, PROTOCOL, Progress, Registrant};
use kbf_state::Store;

fn config() -> MembershipConfig {
    MembershipConfig {
        ttl: Duration::from_secs(30),
        dwell: Duration::from_secs(120),
        replication: 2,
    }
}

fn node(i: u32) -> Registrant {
    Registrant {
        instance_id: format!("node-{i}"),
        addr: format!("10.0.0.{i}:8982"),
        zone: String::new(),
        weight: 1,
        protocol: PROTOCOL,
        progress: Progress::CaughtUp,
    }
}

/// A frontend over the (hookable) store, and another that holds no state of its own.
fn two_frontends(s: &Arc<Hooked>) -> (Arc<Members>, Members) {
    (
        Arc::new(Members::new(Arc::clone(s) as Arc<dyn Store>, config())),
        Members::new(Arc::clone(&s.fake) as Arc<dyn Store>, config()),
    )
}

/// Catches a map source that invents a map before a member registers (the router would
/// route by nothing), and one that does not hand out the previous generation the members
/// keep: no view without a member, the newest map and the one before it with two.
#[test]
fn current_is_the_members_newest_map_and_the_one_before() {
    let s = Hooked::new();
    let (frontend, _) = two_frontends(&s);
    let maps = FrontMaps::new(Arc::clone(&frontend));
    assert!(maps.current().is_none());

    let a = frontend.register(&node(1)).expect("a");
    let view = maps.current().expect("a view");
    assert_eq!((view.map.generation(), view.previous), (1, None));

    let b = frontend.register(&node(2)).expect("b");
    let view = maps.current().expect("a view");
    assert_eq!(*view.map, b.map);
    assert_eq!(view.previous.as_deref(), Some(&a.map));
}

/// Catches a refetch that answers from memory (a bump another frontend committed would
/// never be found, and a node that is ahead of this frontend would fail every call), one
/// that invents a generation, and one that hides a store that cannot be read: it reads the
/// store, returns the view once it holds the generation asked for, and otherwise says
/// which was wanted.
#[tokio::test]
async fn a_refetch_reads_the_state_store() {
    let s = Hooked::new();
    let (frontend, elsewhere) = two_frontends(&s);
    let maps = FrontMaps::new(Arc::clone(&frontend));
    let a = frontend.register(&node(1)).expect("a");
    assert_eq!(maps.current().expect("a view").map.generation(), 1);

    // Another frontend bumps the map; this one has not seen it.
    let b = elsewhere
        .register(&node(2))
        .expect("b via another frontend");
    assert_eq!(maps.current().expect("a view").map.generation(), 1);
    let fetched = maps
        .refetch(b.map.generation())
        .await
        .expect("found in the store");
    assert_eq!(*fetched.map, b.map);
    assert_eq!(fetched.previous.as_deref(), Some(&a.map));
    assert_eq!(maps.current().expect("a view").map.generation(), 2);

    // A generation nobody has made.
    let error = maps.refetch(99).await.expect_err("no such map");
    assert_eq!(error.at_least, 99);
    assert!(error.reason.contains("no map that new"), "{error}");

    // A store that fails is the fetch failing, not an old map passed off as a new one.
    s.fail_gets.store(1, SeqCst);
    let error = maps.refetch(2).await.expect_err("the store fails");
    assert_eq!(error.at_least, 2);
    assert!(!error.reason.contains("no map that new"), "{error}");

    // A read that dies (a store call that panics) is a failed fetch, not a panic in the
    // router's call.
    s.after_get("", || panic!("the store's read died"));
    let error = maps.refetch(2).await.expect_err("the read dies");
    assert_eq!(error.at_least, 2);
    assert!(error.reason.contains("did not finish"), "{error}");
}
