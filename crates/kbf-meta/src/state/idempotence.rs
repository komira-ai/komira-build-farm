//! The idempotence rule for forwarded commands (docs/design/ha.md, section 4.6).
//!
//! A forward retried across a leader change lands as a second log entry. The rule:
//! applying a forwardable command twice, as two entries stamped with the same farm time
//! and with nothing applied between them, leaves the state of one apply on every field
//! but two, each of which only rises:
//! - the loss-mark generation an `ObjectUnreachable` stamps, which becomes the later
//!   entry's index;
//! - `next_epoch`, which an `AllocEpoch` raises once more.
//!
//! With a `Tick` between the two, the only further difference is the touch times of the
//! entries the command names, now the later time.
//!
//! The test lives in the crate so it can compare every private field: a field added to
//! [`MetaState`] is compared too, through `PartialEq`, without a change here.
//!
//! Catches:
//! - a forwardable command whose second apply changes state the first did not: a
//!   `PutBlob` of a held blob that moves the entry, a `PutAction` that is refused or
//!   adds a second record, a second `ObjectUnreachable` that changes the reason, an
//!   `AllocEpoch` that allocates more than one epoch;
//! - a mark whose generation a retry does not restamp (the mutant that stamps only when
//!   the reason rises), or one that stamps a generation on another object;
//! - a second apply after a `Tick` that touches an entry the command does not name, or
//!   touches a named one at a time other than the applying entry's.

use std::collections::BTreeSet;
use std::time::Duration;

use kbf_types::{Digest, DigestFunction, FarmTime};

use super::{Applied, Command, MetaState};
use crate::model::{
    ActionRecord, Closure, Epoch, Generation, Location, ObjectId, Retention, Role, StoreId, Touch,
    UnreachableReason,
};

const DAY: Duration = Duration::from_secs(24 * 60 * 60);

/// xorshift64*, so a failing case is reproducible from its seed.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d) % n
    }

    fn one_in(&mut self, n: u64) -> bool {
        self.below(n) == 0
    }
}

fn digest(rng: &mut Rng) -> Digest {
    let n = u8::try_from(rng.below(10)).expect("small");
    Digest::new(DigestFunction::Sha256, [n; 32], u64::from(n))
}

/// An object of epoch 1, 2 or (rarely) 3. The generated states allocate two epochs, so
/// epoch 3 is refused until an `AllocEpoch` in the history allocates it.
fn object(rng: &mut Rng) -> ObjectId {
    let epoch = if rng.one_in(10) { 3 } else { 1 + rng.below(2) };
    ObjectId::new(Epoch::new(epoch), rng.below(4))
}

fn location(rng: &mut Rng) -> Location {
    Location {
        store: StoreId::CONFIGURED,
        object: object(rng),
        offset: rng.below(4) * 100,
    }
}

fn reason(rng: &mut Rng) -> UnreachableReason {
    if rng.one_in(3) {
        UnreachableReason::Corrupt
    } else {
        UnreachableReason::Missing
    }
}

/// A forwardable command over the generated digests and objects. `ObjectReachable`
/// names the object's current generation half the time, as a prober that read it; the
/// rest of the time, any generation.
fn forwardable(rng: &mut Rng, state: &MetaState) -> Command {
    match rng.below(7) {
        0 => Command::AllocEpoch,
        1 => Command::PutBlob {
            digest: digest(rng),
            location: location(rng),
        },
        2 => Command::PutBlobs(
            (0..rng.below(5))
                .map(|_| (digest(rng), location(rng)))
                .collect(),
        ),
        3 => Command::PutAction {
            role: if rng.one_in(8) {
                Role::Client
            } else {
                Role::Daemon
            },
            action: digest(rng),
            record: ActionRecord {
                result: digest(rng),
                closure: (0..rng.below(3)).map(|_| digest(rng)).collect::<Closure>(),
            },
        },
        4 => Command::Touch(Touch {
            blobs: (0..rng.below(4)).map(|_| digest(rng)).collect(),
            actions: (0..rng.below(3)).map(|_| digest(rng)).collect(),
        }),
        5 => Command::ObjectUnreachable {
            object: object(rng),
            reason: reason(rng),
        },
        _ => {
            let object = object(rng);
            let generation = match state.loss_mark(object) {
                Some(mark) if rng.one_in(2) => mark.generation,
                _ => Generation::new(rng.below(40)),
            };
            Command::ObjectReachable { object, generation }
        }
    }
}

/// A generated state and the index of its last entry: two epochs allocated, then a
/// history of every command, `Tick` and `Collect` included.
fn generated(rng: &mut Rng) -> (MetaState, u64) {
    let mut state = MetaState::new(Retention::default());
    let mut index = 0;
    for _ in 0..2 {
        index += 1;
        state.execute(index, Command::AllocEpoch);
    }
    for _ in 0..rng.below(30) {
        index += 1;
        let command = match rng.below(10) {
            0 => Command::Tick(
                state
                    .now()
                    .saturating_add(DAY * (1 + u32::try_from(rng.below(3)).expect("small"))),
            ),
            1 if rng.one_in(3) => Command::Collect,
            _ => forwardable(rng, &state),
        };
        state.execute(index, command);
    }
    (state, index)
}

/// The object whose mark `command` stamped when it applied as `applied`, if any.
fn stamped(command: &Command, applied: &Applied) -> Option<ObjectId> {
    match (command, applied) {
        (Command::ObjectUnreachable { object, .. }, Applied::Marked(Ok(()))) => Some(*object),
        _ => None,
    }
}

/// The blob and action entries `command` names, and whether its second apply (as
/// `applied`) must have touched each one that is held.
fn named(command: &Command, applied: &Applied) -> (BTreeSet<Digest>, BTreeSet<Digest>, bool) {
    match (command, applied) {
        (Command::PutBlob { digest, .. }, Applied::Blob(r)) => {
            ([*digest].into(), BTreeSet::new(), r.is_ok())
        }
        (Command::PutBlobs(blobs), Applied::Blobs(r)) => (
            blobs.iter().map(|(d, _)| *d).collect(),
            BTreeSet::new(),
            r.is_ok(),
        ),
        (Command::PutAction { action, .. }, Applied::Action(r)) => {
            (BTreeSet::new(), [*action].into(), r.is_ok())
        }
        (Command::Touch(touch), Applied::Touched { .. }) => {
            (touch.blobs.clone(), touch.actions.clone(), true)
        }
        _ => (BTreeSet::new(), BTreeSet::new(), false),
    }
}

/// What the cases saw, so a generator that stops exercising a path fails the test.
#[derive(Default)]
struct Seen {
    restamped: u32,
    restamped_same_reason: u32,
    epochs: u32,
    touched_later: u32,
}

/// The second apply of a command: what it was, at which index, how it applied, and the
/// farm time a `Tick` between the two moved to (`None`: no farm time passed).
struct Second<'a> {
    command: &'a Command,
    j: u64,
    applied: Applied,
    later: Option<FarmTime>,
}

/// Checks `twice` (`once` and then the command again, as `second`) against `once`.
fn check(once: &MetaState, twice: &MetaState, second: &Second<'_>, seen: &mut Seen, case: &str) {
    let Second {
        command,
        j,
        ref applied,
        later,
    } = *second;
    let mut norm = twice.clone();

    let extra = u64::from(matches!(command, Command::AllocEpoch));
    assert_eq!(
        twice.next_epoch,
        once.next_epoch + extra,
        "{case}: next_epoch"
    );
    seen.epochs += u32::try_from(extra).expect("0 or 1");
    norm.next_epoch = once.next_epoch;

    if let Some(object) = stamped(command, applied) {
        let (Some(before), Some(after)) =
            (once.loss_mark(object), norm.unreachable.get_mut(&object))
        else {
            panic!("{case}: the mark on {object} is gone after a second ObjectUnreachable");
        };
        assert_eq!(
            after.generation,
            Generation::new(j),
            "{case}: generation of {object}"
        );
        seen.restamped += 1;
        if let Command::ObjectUnreachable { reason, .. } = command
            && *reason == before.reason
        {
            seen.restamped_same_reason += 1;
        }
        after.generation = before.generation;
    }

    let (blobs, actions, must_touch) = named(command, applied);
    if let Some(later) = later {
        for (digest, entry) in &mut norm.blobs {
            let Some(old) = once.blobs.get(digest) else {
                continue;
            };
            if blobs.contains(digest) && must_touch {
                assert_eq!(entry.last_touch, later, "{case}: blob {digest} touch time");
                seen.touched_later += 1;
                entry.last_touch = old.last_touch;
            }
        }
        for (action, entry) in &mut norm.actions {
            let Some(old) = once.actions.get(action) else {
                continue;
            };
            if actions.contains(action) && must_touch {
                assert_eq!(entry.last_hit, later, "{case}: action {action} hit time");
                seen.touched_later += 1;
                entry.last_hit = old.last_hit;
            }
        }
    }

    assert_eq!(
        &norm, once,
        "{case}: {command:?} applied twice as {applied:?}"
    );
}

/// The rule of section 4.6, over 2 000 generated states and one generated forwardable
/// command each. Case 1: the command at index i and again at j > i, nothing between.
/// Case 2: the same, with a `Tick` to a later farm time between the two.
#[test]
fn a_forwarded_command_applied_twice_equals_once_but_for_generations_and_next_epoch() {
    let mut seen = Seen::default();
    for seed in 1..=2000u64 {
        let mut rng = Rng::new(seed);
        let (base, last) = generated(&mut rng);
        let command = forwardable(&mut rng, &base);

        let i = last + 1;
        let mut once = base.clone();
        once.execute(i, command.clone());

        let j = i + 1 + rng.below(5);
        let mut twice = once.clone();
        let applied = twice.execute(j, command.clone());
        let second = Second {
            command: &command,
            j,
            applied,
            later: None,
        };
        check(
            &once,
            &twice,
            &second,
            &mut seen,
            &format!("seed {seed}, no tick"),
        );

        let k = i + 1;
        let later = once
            .now()
            .saturating_add(Duration::from_millis(1 + rng.below(1000)));
        once.execute(k, Command::Tick(later));
        let j = k + 1 + rng.below(5);
        let mut twice = once.clone();
        let applied = twice.execute(j, command.clone());
        let second = Second {
            command: &command,
            j,
            applied,
            later: Some(later),
        };
        check(
            &once,
            &twice,
            &second,
            &mut seen,
            &format!("seed {seed}, tick between"),
        );
    }
    // Every exception and the touch-time rule must have been exercised, the same-reason
    // retry (which a stamp-only-on-rise mutant leaves at i) included.
    assert!(
        seen.restamped > 200
            && seen.restamped_same_reason > 100
            && seen.epochs > 200
            && seen.touched_later > 200,
        "restamped {}, same reason {}, epochs {}, touched later {}",
        seen.restamped,
        seen.restamped_same_reason,
        seen.epochs,
        seen.touched_later
    );
}
