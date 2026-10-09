//! Who may write the action cache, and when.

mod common;

use common::{
    cache_action, digest, in_segment, mark_missing, mark_reachable, meta, put, put_action,
};
use kbf_meta::{
    ActionAnswer, ActionRecord, ActionWriteError, Closure, Command, Miss, Role,
};

/// Catches: a client `UpdateActionResult` accepted. Only daemons, which ran the action,
/// write the action cache; a client write would let one client serve any result to
/// everyone. The refused write must leave no entry behind, and must not replace a
/// daemon's entry either.
#[test]
fn client_writes_are_refused() {
    let mut m = meta();
    let (action, result) = (digest(1), digest(2));
    put(&mut m, result, in_segment(1, 0));
    let record = ActionRecord {
        result,
        closure: Closure::new(),
    };
    assert_eq!(
        put_action(&mut m, Role::Client, action, record.clone()),
        Err(ActionWriteError::NotDaemon)
    );
    assert_eq!(m.action_count(), 0);
    assert_eq!(m.action(&action), ActionAnswer::Miss(Miss::NoEntry));

    let c = cache_action(&mut m);
    let before = m.clone();
    assert_eq!(
        put_action(&mut m, Role::Client, c.action, record),
        Err(ActionWriteError::NotDaemon)
    );
    assert_eq!(m, before);
    assert_eq!(
        put_action(
            &mut m,
            Role::Daemon,
            action,
            ActionRecord {
                result,
                closure: Closure::new(),
            }
        ),
        Ok(())
    );
    assert!(matches!(m.action(&action), ActionAnswer::Hit { .. }));
}

/// Catches: an entry written before its outputs are stored, which would be served as a
/// hit the moment the outputs land and as a broken hit if they never do. The refusal
/// names the first missing blob and changes nothing.
#[test]
fn writes_need_every_blob_held_and_reachable() {
    let mut m = meta();
    let (action, result, output) = (digest(1), digest(2), digest(3));
    put(&mut m, result, in_segment(1, 0));
    let record = ActionRecord {
        result,
        closure: Closure::from_iter([output]),
    };
    assert_eq!(
        put_action(&mut m, Role::Daemon, action, record.clone()),
        Err(ActionWriteError::Absent(output))
    );
    put(&mut m, output, in_segment(2, 0));
    mark_missing(&mut m, 2);
    assert_eq!(
        put_action(&mut m, Role::Daemon, action, record.clone()),
        Err(ActionWriteError::Unreachable(output))
    );
    assert_eq!(m.action_count(), 0);
    mark_reachable(&mut m, 2);
    assert_eq!(put_action(&mut m, Role::Daemon, action, record), Ok(()));
    assert_eq!(m.action_count(), 1);
}
