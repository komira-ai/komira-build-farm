//! A record read back from history (`Invocation::restored`): its counters are the ones
//! its rows show, it is no live record (no mint, no owner, no operation in flight), and
//! its hole is reported only when it has one. Each test names the defect it catches.

use std::collections::BTreeSet;

use kbf_invocation::{
    ActionRow, Counts, How, Invocation, InvocationId, Outcome, RecordKey, Restored, State, WallTime,
};
use kbf_types::{Digest, DigestFunction};

fn digest(n: u8) -> Digest {
    Digest::new(DigestFunction::Sha256, [n; 32], u64::from(n))
}

fn row(n: u8, how: How, outcome: Option<Outcome>) -> ActionRow {
    ActionRow {
        action: digest(n),
        instance: "main".to_owned(),
        target: None,
        mnemonic: None,
        configuration: None,
        action_id: None,
        how,
        operation: None,
        attempts: Vec::new(),
        outcome,
        listing: None,
        outputs_truncated: false,
        command: None,
        shape: None,
    }
}

fn parts(complete: bool, gap_from: Option<u64>) -> Restored {
    Restored {
        key: RecordKey::owner(InvocationId::new("build")),
        serial: 1 << 63,
        instances: BTreeSet::from(["main".to_owned()]),
        first_seen: WallTime::from_unix_micros(1_000),
        last_seen: WallTime::from_unix_micros(2_000),
        complete,
        gap_from: gap_from.map(WallTime::from_unix_micros),
        rows_capped: false,
    }
}

/// Catches: counters left at zero for a restored record (its summary would say it did
/// nothing), `execute` that is not the sum of the three ways an Execute is answered, a
/// hit counted as an Execute, a failure not counted (exit non-zero, timed out, refused)
/// or an OK exit-0 row counted as one, and a row of one `how` counted under another.
#[test]
fn the_counters_are_the_ones_the_rows_show() {
    let ok = Some(Outcome::Ok { exit_code: 0 });
    let rows = vec![
        row(1, How::Ran, ok.clone()),
        row(2, How::Ran, Some(Outcome::Ok { exit_code: 3 })),
        row(3, How::CacheHitExecute, ok.clone()),
        row(4, How::CacheHit, None),
        row(5, How::Joined, Some(Outcome::TimedOut)),
        row(6, How::Ran, Some(Outcome::Refused("no".to_owned()))),
        row(7, How::Ran, None),
    ];
    let inv = Invocation::restored(parts(true, None), rows);
    assert_eq!(
        *inv.counts(),
        Counts {
            execute: 6,
            cache_hit_execute: 1,
            joined: 1,
            ran: 4,
            failed: 3,
            ac_hit: 1,
            ..Counts::default()
        }
    );
    assert_eq!(inv.rows().len(), 7);
    assert_eq!(inv.row(&digest(2)).map(|r| r.how), Some(How::Ran));
}

/// Catches: a restored record taken for a live one (minted, owned, an operation in
/// flight that would keep it open for ever), a serial lost (a page token could not tell
/// it from another record's), a hole reported for a complete record, a hole lost for an
/// incomplete one, and rows not counted in the bytes (a cache bound that counts none).
#[test]
fn a_restored_record_is_no_live_record() {
    let rows = vec![row(1, How::Ran, None)];
    let inv = Invocation::restored(parts(false, Some(1_500)), rows.clone());
    assert!(inv.from_history());
    assert!(!inv.minted());
    assert!(!inv.complete());
    assert_eq!(inv.serial(), 1 << 63);
    assert_eq!(inv.gap_from(), Some(WallTime::from_unix_micros(1_500)));
    assert_eq!(inv.tool(), (None, None));
    assert!(inv.owner().0.is_empty());
    assert_eq!(inv.first_seen(), WallTime::from_unix_micros(1_000));
    assert_eq!(inv.last_seen(), WallTime::from_unix_micros(2_000));
    // Nothing in flight: an hour after its last record it is idle.
    let later = WallTime::from_unix_micros(2_000 + 3_600_000_000);
    assert_eq!(inv.state(later), State::Idle);
    let empty = Invocation::restored(parts(false, Some(1_500)), Vec::new());
    assert!(inv.bytes() > empty.bytes(), "the rows are counted");
    // Complete: no hole, even when a gap time is given.
    let whole = Invocation::restored(parts(true, Some(1_500)), rows);
    assert_eq!(whole.gap_from(), None);
    assert!(whole.complete());
}
