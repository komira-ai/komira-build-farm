//! Touch-on-read, garbage collection, farm time and replay.

mod common;

use std::collections::BTreeMap;

use common::{
    ANY_INDEX, DAY, HOUR, at, collect, commit_touch, digest, in_segment, meta, put, put_action,
    tick,
};
use kbf_meta::{
    ActionAnswer, ActionRecord, Applied, BlobAnswer, Closure, Command, MetaState, Role, Touch,
};
use kbf_types::{Digest, FarmTime, StateMachine};

/// Catches: collection by write time instead of last-touch time. Two blobs are written
/// together; one is read (and touched) at day 5. At day 9 only the untouched one may
/// go; the touched one stays until its own retention (day 5 + 7 + 1) runs out.
#[test]
fn collection_follows_the_last_touch_not_the_write() {
    let mut m = meta();
    let (read, unread) = (digest(1), digest(2));
    put(&mut m, read, in_segment(1, 1));
    put(&mut m, unread, in_segment(1, 2));

    tick(&mut m, DAY * 5);
    let touch = m.touch_for([&read]);
    assert_eq!(touch.blobs, [read].into());
    assert!(commit_touch(&mut m, touch).is_empty());

    tick(&mut m, DAY * 9);
    assert_eq!(collect(&mut m).blobs, vec![(unread, in_segment(1, 2))]);
    assert!(matches!(m.blob(&read), BlobAnswer::Present(_)));

    tick(&mut m, DAY * 13);
    assert!(collect(&mut m).blobs.is_empty(), "held through day 13");
    tick(&mut m, DAY * 13 + HOUR);
    assert_eq!(collect(&mut m).blobs, vec![(read, in_segment(1, 1))]);
    assert_eq!(m.blob(&read), BlobAnswer::Absent);
}

/// Catches: a reader answering from a read whose touch lost a race with a collection.
/// The touch must report the entry lost so the reader asks again (and now hears
/// absent), instead of serving a blob that is already gone.
#[test]
fn a_touch_after_collection_reports_the_loss() {
    let mut m = meta();
    let d = digest(1);
    put(&mut m, d, in_segment(1, 0));
    tick(&mut m, DAY * 9);
    let touch = m.touch_for([&d]);
    assert_eq!(touch.blobs, [d].into());
    collect(&mut m);
    let action = digest(9);
    let lost = commit_touch(
        &mut m,
        Touch {
            blobs: touch.blobs,
            actions: [action].into(),
        },
    );
    assert_eq!(lost.blobs, [d].into());
    assert_eq!(lost.actions, [action].into());
    assert_eq!(m.blob(&d), BlobAnswer::Absent);
}

/// Catches: farm time that moves backwards on a late or reordered tick, which would
/// make entries look younger or older than they are.
#[test]
fn farm_time_never_moves_back() {
    let mut m = meta();
    assert_eq!(
        m.execute(ANY_INDEX, Command::Tick(at(DAY * 5))),
        Applied::Ticked(at(DAY * 5))
    );
    assert_eq!(
        m.execute(ANY_INDEX, Command::Tick(at(DAY))),
        Applied::Ticked(at(DAY * 5))
    );
    assert_eq!(m.now(), at(DAY * 5));
}

/// A small deterministic generator for the scripted runs below (xorshift64).
struct Script(u64);

impl Script {
    fn next(&mut self, below: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % below
    }
}

/// Catches: anything that lets a blob reported present or served be collected sooner
/// than `min_ttl` of farm time after the report: collecting by write time, a touch
/// that does not move the last touch, a skipped touch whose quantum is not covered by
/// the retention, or a hit that does not touch its closure. Runs seeded scripts of
/// writes, reads, action-cache hits, ticks and collections, and checks every removal
/// against every earlier report.
#[test]
fn nothing_reported_present_is_collected_within_min_ttl() {
    // How often the script exercised what it checks, so the check cannot pass vacuously.
    let (mut hits, mut checked) = (0, 0);
    for seed in 1..=8_u64 {
        let mut s = Script(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15));
        let mut m = meta();
        let min_ttl = m.retention().min_ttl;
        // Digest -> the earliest farm time it may be collected.
        let mut promised: BTreeMap<Digest, FarmTime> = BTreeMap::new();
        let promise = |promised: &mut BTreeMap<Digest, FarmTime>, m: &MetaState, d: Digest| {
            let until = m.now().saturating_add(min_ttl);
            let p = promised.entry(d).or_insert(until);
            *p = (*p).max(until);
        };
        let mut cached: Option<ActionRecord> = None;
        let mut now = 0_u64;
        for step in 0..4_000_u64 {
            let d = digest(u8::try_from(s.next(24)).unwrap());
            match s.next(6) {
                0 => {
                    now += s.next(u64::try_from(DAY.as_millis() * 3 / 2).unwrap());
                    m.execute(ANY_INDEX, Command::Tick(FarmTime::from_millis(now)));
                }
                1 => {
                    put(&mut m, d, in_segment(step, 0));
                }
                2 => {
                    // A read: answer, touch, and only then report present.
                    let touch = m.touch_for([&d]);
                    if let BlobAnswer::Present(_) = m.blob(&d) {
                        assert!(commit_touch(&mut m, touch).is_empty());
                        promise(&mut promised, &m, d);
                    }
                }
                3 => {
                    let (result, output) = (d, digest(u8::try_from(s.next(24)).unwrap()));
                    let record = ActionRecord {
                        result,
                        closure: Closure::from_iter([output]),
                    };
                    if put_action(&mut m, Role::Daemon, digest(100), record.clone()).is_ok() {
                        cached = Some(record);
                    }
                }
                4 => {
                    if let ActionAnswer::Hit { result, touch } = m.action(&digest(100)) {
                        assert!(commit_touch(&mut m, touch).is_empty());
                        hits += 1;
                        // A hit serves the result and every closure blob.
                        let record = cached.as_ref().expect("a hit needs a written entry");
                        assert_eq!(record.result, result);
                        promise(&mut promised, &m, result);
                        for c in record.closure.iter() {
                            promise(&mut promised, &m, *c);
                        }
                    }
                }
                _ => {
                    for (d, _) in collect(&mut m).blobs {
                        if let Some(until) = promised.get(&d) {
                            checked += 1;
                            assert!(
                                m.now() >= *until,
                                "seed {seed} step {step}: {d} collected at {:?}, promised until {until:?}",
                                m.now()
                            );
                        }
                    }
                }
            }
        }
    }
    assert!(
        hits > 100 && checked > 100,
        "hits {hits}, checked {checked}"
    );
}

/// Catches: hidden state outside the commands (a clock read, an iteration order that
/// varies), which would make two replicas of one log diverge. The same commands,
/// applied through `StateMachine::apply` and through `execute`, give equal states.
#[test]
fn replicas_applying_one_log_agree() {
    let log = || {
        let mut log = vec![Command::Tick(at(HOUR))];
        for n in 0..20 {
            log.push(Command::PutBlob {
                digest: digest(n),
                location: in_segment(u64::from(n % 3), n),
            });
        }
        log.push(Command::PutAction {
            role: Role::Daemon,
            action: digest(50),
            record: ActionRecord {
                result: digest(1),
                closure: Closure::from_iter([digest(2), digest(3)]),
            },
        });
        log.push(Command::Tick(at(DAY * 2)));
        log.push(Command::Touch(Touch {
            blobs: [digest(1), digest(2)].into(),
            actions: [digest(50)].into(),
        }));
        log.push(Command::Tick(at(DAY * 9)));
        log.push(Command::Collect);
        log
    };
    let mut a = meta();
    let mut b = meta();
    let mut effects = Vec::new();
    for c in log() {
        effects.extend(a.apply((ANY_INDEX, c)));
    }
    for c in log() {
        b.execute(ANY_INDEX, c);
    }
    assert!(effects.is_empty());
    assert_eq!(a, b);
    assert_eq!(a.blob_count(), 2);
    assert!(matches!(a.action(&digest(50)), ActionAnswer::Miss(_)));
}
