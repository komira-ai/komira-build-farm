//! The closure check before every action-cache hit.

mod common;

use common::{
    DAY, HOUR, cache_action, collect, commit_touch, mark_missing, mark_reachable, meta, tick,
};
use kbf_meta::{ActionAnswer, Miss, Touch};

/// Catches: a lookup that fails on a complete entry, or serves something other than
/// the stored `ActionResult` digest.
#[test]
fn complete_closure_is_a_hit() {
    let mut m = meta();
    let c = cache_action(&mut m);
    match m.action(&c.action) {
        ActionAnswer::Hit { result, .. } => assert_eq!(result, c.result),
        other => panic!("expected a hit, got {other:?}"),
    }
    assert_eq!(m.action(&c.result), ActionAnswer::Miss(Miss::NoEntry));
}

/// Catches: a hit served while one blob the entry needs is gone (protection 1 of the
/// storage section). Each of the result, an output file, an output tree and a tree's
/// child is collected in turn while the rest stay fresh; each must turn the hit into a
/// miss naming that blob. Skipping the closure check serves all four as hits.
#[test]
fn any_missing_blob_turns_the_hit_into_a_miss() {
    for victim_index in 0..4 {
        let mut m = meta();
        let c = cache_action(&mut m);
        let victim = c.all()[victim_index];
        // Touch every blob but the victim at day 5, then collect past the victim's
        // retention (written at 0: 7 days plus the 1-day quantum).
        tick(&mut m, DAY * 5);
        let fresh = Touch {
            blobs: c.all().into_iter().filter(|d| *d != victim).collect(),
            actions: [c.action].into(),
        };
        assert!(commit_touch(&mut m, fresh).is_empty());
        tick(&mut m, DAY * 9);
        let collected = collect(&mut m);
        assert_eq!(
            collected.blobs.iter().map(|(d, _)| *d).collect::<Vec<_>>(),
            vec![victim]
        );
        assert_eq!(
            m.action(&c.action),
            ActionAnswer::Miss(Miss::Absent(victim)),
            "victim {victim_index}"
        );
    }
}

/// Catches: a hit served while a needed blob's object cannot be read (the store said
/// 404, or is down). The table in the storage section: UNAVAILABLE for the read, a miss
/// for the action cache. Also catches a miss that sticks after the object is back.
#[test]
fn unreachable_closure_blob_is_a_miss_until_reachable() {
    let mut m = meta();
    let c = cache_action(&mut m);
    // The child lives in segment 4 (see `cache_action`).
    mark_missing(&mut m, 4);
    assert_eq!(
        m.action(&c.action),
        ActionAnswer::Miss(Miss::Unreachable(c.child))
    );
    mark_reachable(&mut m, 4);
    assert!(matches!(m.action(&c.action), ActionAnswer::Hit { .. }));
}

/// Catches: a hit that does not touch stale blobs (they could be collected while the
/// client still relies on them), or one that touches on every read (a log commit per
/// hit, which the touch quantum exists to avoid).
#[test]
fn a_hit_touches_only_what_is_stale() {
    let mut m = meta();
    let c = cache_action(&mut m);
    tick(&mut m, HOUR * 23);
    match m.action(&c.action) {
        ActionAnswer::Hit { touch, .. } => assert!(touch.is_empty(), "{touch:?}"),
        other => panic!("expected a hit, got {other:?}"),
    }
    tick(&mut m, DAY);
    match m.action(&c.action) {
        ActionAnswer::Hit { touch, .. } => {
            assert_eq!(touch.blobs, c.all().into_iter().collect());
            assert_eq!(touch.actions, [c.action].into());
        }
        other => panic!("expected a hit, got {other:?}"),
    }
}

/// Catches: an entry served after 30 days without a hit, or one hidden although hits
/// kept touching it.
#[test]
fn an_entry_without_hits_expires() {
    let mut m = meta();
    let c = cache_action(&mut m);
    let keep_blobs = |m: &mut kbf_meta::MetaState| {
        let t = m.touch_for(c.all().iter());
        commit_touch(m, t);
    };
    // Hit at day 20 and commit its touch; blobs kept fresh throughout.
    for day in [5, 10, 15, 20] {
        tick(&mut m, DAY * day);
        keep_blobs(&mut m);
    }
    let ActionAnswer::Hit { touch, .. } = m.action(&c.action) else {
        panic!("expected a hit at day 20");
    };
    assert!(commit_touch(&mut m, touch).is_empty());
    for day in [25, 30, 35, 40, 45, 50] {
        tick(&mut m, DAY * day);
        keep_blobs(&mut m);
    }
    // Last hit at day 20: held through 20 + 30 + 1 = day 51.
    assert!(matches!(m.action(&c.action), ActionAnswer::Hit { .. }));
    tick(&mut m, DAY * 51 + HOUR);
    keep_blobs(&mut m);
    assert_eq!(m.action(&c.action), ActionAnswer::Miss(Miss::Expired));
    assert_eq!(collect(&mut m).actions, vec![c.action]);
    assert_eq!(m.action(&c.action), ActionAnswer::Miss(Miss::NoEntry));
}
