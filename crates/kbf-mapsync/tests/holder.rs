//! The holder keeps the newest map and never goes back; `reach` waits for a map that
//! new and says it is wanted. Paused clocks only: every wait here ends because another
//! task acted, or the paused clock has nothing left to wait for.

use std::time::Duration;

use kbf_map::{Map, Member, MemberId, MemberState, Rule};
use kbf_mapsync::{FakeMapSource, MapHolder, MapSource};
use tokio::time::timeout;

fn map(generation: u64) -> Map {
    let member = Member {
        id: MemberId(1),
        addr: "127.0.0.1:1".to_owned(),
        zone: String::new(),
        weight: 1,
        state: MemberState::Active,
    };
    Map::new(generation, vec![member], Rule::Rendezvous, 1, 1, Vec::new()).expect("a valid map")
}

/// Catches a holder that goes backwards: a late or repeated push of an older or the same
/// generation must change nothing (not the map, not the generation, not the map it
/// replaced), and a newer one is taken. Mutants: `>=` made `>` (the same generation
/// replaces the map and the previous one), the comparison removed (the last offer wins).
#[test]
fn a_holder_never_goes_backwards() {
    let holder = MapHolder::default();
    assert_eq!(holder.generation(), 0);
    assert!(holder.current().is_none());
    assert!(holder.offer(map(5)));
    for refused in [5, 4, 1] {
        assert!(!holder.offer(map(refused)), "generation {refused}");
        assert_eq!(holder.generation(), 5);
        assert_eq!(holder.current().expect("held").generation(), 5);
        assert!(holder.previous().is_none(), "nothing was replaced");
    }
    assert!(holder.offer(map(9)));
    assert_eq!(holder.generation(), 9);
    assert_eq!(holder.previous().expect("replaced").generation(), 5);
}

/// Catches a holder that announces a map it refused, or does not announce one it took:
/// the receiver is notified once per map taken.
#[tokio::test(start_paused = true)]
async fn the_receiver_is_notified_for_each_map_taken_only() {
    let holder = MapHolder::new();
    let mut changes = holder.changes();
    holder.offer(map(3));
    changes.changed().await.expect("notified");
    assert_eq!(*changes.borrow_and_update(), 3);
    holder.offer(map(2));
    assert!(!changes.has_changed().expect("open"), "a refused map");
    holder.offer(map(4));
    assert!(changes.has_changed().expect("open"));
    assert_eq!(*changes.borrow_and_update(), 4);
}

/// Catches a `reach` that returns a map older than asked, one that returns without
/// waiting, and one that wakes for the first push whatever its generation.
#[tokio::test(start_paused = true)]
async fn reach_waits_for_the_generation_asked() {
    let holder = std::sync::Arc::new(MapHolder::new());
    holder.offer(map(2));
    let waiting = tokio::spawn({
        let holder = std::sync::Arc::clone(&holder);
        async move { holder.reach(5).await }
    });
    tokio::task::yield_now().await;
    holder.offer(map(3));
    tokio::task::yield_now().await;
    assert!(!waiting.is_finished(), "woken for generation 3");
    holder.offer(map(6));
    assert_eq!(waiting.await.expect("task").generation(), 6);
    // Already there: answered at once.
    assert_eq!(holder.reach(4).await.generation(), 6);
}

/// Catches a `reach` that does not say what it wants (the session would never ask the
/// frontend, and a push that was lost would be waited for for ever), and one that lowers
/// the want.
#[tokio::test(start_paused = true)]
async fn reach_records_the_generation_wanted() {
    let holder = MapHolder::new();
    let mut asks = holder.asks();
    assert_eq!(*asks.borrow(), 0);
    let _ = timeout(Duration::from_secs(1), holder.reach(7)).await;
    assert_eq!(*asks.borrow_and_update(), 7);
    let _ = timeout(Duration::from_secs(1), holder.reach(4)).await;
    assert_eq!(
        *asks.borrow(),
        7,
        "a lower want does not replace a higher one"
    );
    assert!(!asks.has_changed().expect("open"));
    // 0 asks for the first map.
    let fresh = MapHolder::new();
    fresh.offer(map(1));
    assert_eq!(fresh.reach(0).await.generation(), 1);
}

/// Catches a fake that does not announce a change, or announces a generation other than
/// the map's it now serves.
#[tokio::test(start_paused = true)]
async fn the_fake_source_serves_the_map_it_announced() {
    let source = FakeMapSource::new(map(1));
    let mut changes = source.changes();
    assert_eq!(*changes.borrow(), 1);
    assert_eq!(source.current().expect("map").generation(), 1);
    source.set(map(2));
    changes.changed().await.expect("notified");
    assert_eq!(*changes.borrow_and_update(), 2);
    assert_eq!(source.current().expect("map").generation(), 2);
}

/// Catches a `follow` that misses the map already held (a client built after the first
/// push would tag its calls with generation 0), one that applies a map that was refused
/// or applies generation 0 for a holder with no map, and one that does not stop with the
/// holder (a task for every client for ever).
#[tokio::test(start_paused = true)]
async fn follow_applies_each_generation_taken() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    let holder = MapHolder::new();
    let applied = Arc::new(AtomicU64::new(u64::MAX));
    let task = holder.follow({
        let applied = Arc::clone(&applied);
        move |generation| applied.store(generation, Ordering::SeqCst)
    });
    assert_eq!(applied.load(Ordering::SeqCst), u64::MAX, "nothing held yet");
    holder.offer(map(4));
    tokio::task::yield_now().await;
    assert_eq!(applied.load(Ordering::SeqCst), 4);
    holder.offer(map(2));
    tokio::task::yield_now().await;
    assert_eq!(applied.load(Ordering::SeqCst), 4, "a refused map");
    holder.offer(map(8));
    tokio::task::yield_now().await;
    assert_eq!(applied.load(Ordering::SeqCst), 8);
    // A follower of a holder that holds a map has it at once, before any change.
    let late = Arc::new(AtomicU64::new(0));
    let second = holder.follow({
        let late = Arc::clone(&late);
        move |generation| late.store(generation, Ordering::SeqCst)
    });
    assert_eq!(late.load(Ordering::SeqCst), 8);
    drop(holder);
    task.await.expect("the task ends with the holder");
    second.await.expect("the task ends with the holder");
}
