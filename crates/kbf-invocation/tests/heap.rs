//! The records' real memory against the bound of docs/design/invocations.md section
//! 3.5, measured by a counting allocator: what the allocator is asked for while the
//! records are built (not its own overhead per allocation). Each test names the defect
//! it catches.
//!
//! The allocator counts per thread, and each test builds its records on its own
//! thread, so tests running at once do not see each other's allocations.

mod support;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use kbf_invocation::{
    CallKind, Event, ExecuteAnswer, Finished, LeaseEnd, Metadata, Outcome, Output, ROW_CAP,
    Records, WallTime,
};
use kbf_sched::RequeueReason;
use kbf_types::{Failure, OperationId};
use support::*;

thread_local! {
    /// The bytes this thread has allocated and not freed.
    static LIVE: Cell<isize> = const { Cell::new(0) };
}

fn count(bytes: isize) {
    // Not reachable while the thread's locals are torn down; nothing measured then.
    let _ = LIVE.try_with(|live| live.set(live.get() + bytes));
}

fn signed(n: usize) -> isize {
    isize::try_from(n).expect("an allocation fits in isize")
}

/// The system allocator, counting what each thread is given.
struct Counting;

// SAFETY: every call is passed on to `System` unchanged; counting reads only the
// layouts and sizes.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count(signed(layout.size()));
        // SAFETY: the caller's contract for `alloc`, passed on.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        count(-signed(layout.size()));
        // SAFETY: the caller's contract for `dealloc`, passed on.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        count(signed(new_size) - signed(layout.size()));
        // SAFETY: the caller's contract for `realloc`, passed on.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static COUNTING: Counting = Counting;

fn live() -> isize {
    LIVE.with(Cell::get)
}

/// What the records' first nodes take besides what the bound counts: one node of
/// each index, made with the first record and the first operation (measured: under
/// 2.2 kB).
const FIRST_NODES: usize = 3_000;

/// Applies `events` to records bounded by `bound` and checks, after every event, that
/// the records' real memory is within a quarter over the bound and within a quarter
/// of what they count (beside [`FIRST_NODES`]), and that they count no more than a
/// quarter over what they hold.
fn within(bound: usize, events: impl IntoIterator<Item = Event>) {
    let base = live();
    let mut r = Records::with_bound(ROW_CAP, bound);
    for (n, event) in events.into_iter().enumerate() {
        r.apply(event);
        let real = usize::try_from(live() - base).expect("the records hold memory");
        let counted = r.bytes();
        let at = || format!("after event {n} within {bound}: {real} B real, {counted} counted");
        assert!(counted <= bound, "{}", at());
        assert!(
            real <= bound + bound / 4,
            "over a quarter past the bound {}",
            at()
        );
        assert!(
            real <= counted + counted / 4 + FIRST_NODES,
            "the estimate misses memory held {}",
            at()
        );
        assert!(
            counted <= real + real / 4,
            "the estimate counts memory not held {}",
            at()
        );
    }
}

/// A finish of `op` by lease `seq`, failed with a 5 kB reason and a 5 kB message,
/// whose result lists `outputs` outputs with 206-byte paths.
fn failed_wide(op: u64, seq: u64, outputs: usize, at: WallTime) -> Event {
    let mut l = listing(30, 1);
    l.outputs = (0..outputs)
        .map(|k| Output {
            path: format!("{}{k:06}", "p".repeat(200)),
            digest: d(3),
            executable: false,
            directory: false,
        })
        .collect();
    Event::Finished(Finished {
        operation: OperationId(op),
        lease: Some((
            lease(seq),
            LeaseEnd::Failed(Failure::Infra, "r".repeat(5000)),
        )),
        outcome: Outcome::Failed {
            code: 13,
            message: "m".repeat(5000),
        },
        result: Some(l),
        at,
    })
}

/// `records` open records of 8 Executes each, every operation shared by three
/// records (one queued, two twins joined), each operation requeued six times with a
/// 100-byte worker, then finished failed with 5 kB texts and 1,001 outputs. Made one
/// by one, so that each is freed as it is applied.
fn builds(records: u64) -> impl Iterator<Item = Event> {
    let calls = (0..records).flat_map(|i| {
        let inv = format!("rec-{i:03}");
        (0..8).map(move |a| {
            let op = (i / 3) * 100 + a;
            let action = u8::try_from(a).expect("a small action");
            if i % 3 == 0 {
                queued(&inv, t(10), action, op)
            } else {
                joined(&inv, t(10), action, op)
            }
        })
    });
    let leases = (0..records / 3).flat_map(|group| {
        (0..8).flat_map(move |a| {
            let op = group * 100 + a;
            // Each operation's 7 leases.
            let first = (group * 8 + a) * 7 + 1;
            let worker = "w".repeat(100);
            (first..first + 6)
                .flat_map(move |seq| {
                    [
                        granted(op, seq, &worker, t(11)),
                        started(op, seq, t(11)),
                        requeued(op, seq, RequeueReason::Silent, t(11)),
                    ]
                })
                .chain(std::iter::once_with(move || {
                    granted(op, first + 6, &"w".repeat(100), t(12))
                }))
                .chain(std::iter::once_with(move || {
                    failed_wide(op, first + 6, 1001, t(12))
                }))
        })
    });
    calls.chain(leases)
}

/// `n` records of one call each, with a tool and a correlated id, a second apart:
/// every record a header and nothing else.
fn headers(n: u64) -> impl Iterator<Item = Event> {
    (0..n).map(|i| {
        let mut c = call(
            &format!("0b5a7d3e-1f2c-4c3d-9e8f-{i:012}"),
            t(i),
            CallKind::Downloaded(1),
        );
        c.metadata = Metadata {
            tool: Some("bazel".to_owned()),
            tool_version: Some("7.4.1".to_owned()),
            correlated_invocations_id: Some(format!("build-{i}")),
            ..Metadata::default()
        };
        ev(c)
    })
}

/// `n` records of `per` cache hits each.
fn hits(n: u64, per: u8) -> impl Iterator<Item = Event> {
    (0..n).flat_map(move |i| {
        (0..per).map(move |a| {
            ev(call(
                &format!("inv-{i}"),
                t(i),
                CallKind::GetActionResult {
                    action: d(a),
                    hit: Some(listing(a, 0)),
                },
            ))
        })
    })
}

/// Catches an estimate that misses real memory (the bound then holds in counted
/// bytes while the heap grows past it: before this test, an outcome text cut for the
/// bound kept its 1 kB allocation, and capacities and B-tree nodes went uncounted,
/// so the heap reached 2.24 times the bound), and one padded past what is held (the
/// bound would turn records away with memory to spare). The mixes: 30 and 300 open
/// records with twins, requeues, 5 kB failure texts and 1,001-output listings; 3,000
/// header-only records, most of them not made (their dropped calls are remembered);
/// cache hits of 1, 8 and 200 rows a record.
#[test]
fn the_records_heap_stays_within_a_quarter_of_the_bound() {
    for (records, bound) in [
        (30, 200_000),
        (300, 200_000),
        (300, 2_000_000),
        (30, 20_000_000),
    ] {
        within(bound, builds(records));
    }
    for bound in [200_000, 2_000_000] {
        within(bound, headers(3000));
        for per in [1, 8, 200] {
            within(bound, hits(300, per));
        }
    }
}

/// The action digest numbered `n`, for builds of more than 256 actions.
fn action(n: u64) -> kbf_types::Digest {
    let mut hash = [0; 32];
    hash[..8].copy_from_slice(&n.to_le_bytes());
    kbf_types::Digest::new(kbf_types::DigestFunction::Sha256, hash, n)
}

/// `builds` open builds of `actions` actions each, every action its own operation
/// (one waiting call each), queued and, with `leases`, granted and started.
fn unique(builds: u64, actions: u64, leases: bool) -> impl Iterator<Item = Event> {
    (0..builds).flat_map(move |b| {
        let inv = format!("build-{b:03}");
        (0..actions).flat_map(move |a| {
            let op = b * 10_000 + a;
            let queued = ev(call(
                &inv,
                t(10),
                CallKind::Execute {
                    action: action(op),
                    answer: ExecuteAnswer::Queued {
                        name: name(&inv, op),
                        operation: OperationId(op),
                    },
                },
            ));
            let leased = leases.then(|| [granted(op, op, "node-a", t(11)), started(op, op, t(11))]);
            std::iter::once(queued).chain(leased.into_iter().flatten())
        })
    })
}

/// Catches an operation's list of waiting calls allocated past its length (plant:
/// no `reserve_exact` before the waiter's push in `Records::execute`; a list made by
/// `push` has room for 4, so a build whose actions each wait alone held 1.397 times
/// a 200 kB bound): builds of 500 to 3,000 unique actions, queued, granted and
/// started, and builds that only queue.
#[test]
fn builds_of_unique_actions_stay_within_a_quarter_of_the_bound() {
    for (builds, actions, leases, bound) in [
        (10, 3000, true, 200_000),
        (100, 500, true, 200_000),
        (30, 3000, false, 2_000_000),
        (100, 500, false, 20_000_000),
    ] {
        within(bound, unique(builds, actions, leases));
    }
}

/// A rolling window of operations: 20 builds queue 20,000 actions in turn, each
/// joined by 3 minted twins and granted, and the operation 500 behind it finishes.
/// Made one by one, so that each is freed as it is applied.
fn rolling() -> impl Iterator<Item = Event> {
    (0..20_000u64).flat_map(|k| {
        (0..6).filter_map(move |step| match step {
            0 => {
                let inv = format!("b{}", k % 20);
                Some(ev(call(
                    &inv,
                    t(0),
                    CallKind::Execute {
                        action: action(k),
                        answer: ExecuteAnswer::Queued {
                            name: name(&inv, k),
                            operation: OperationId(k),
                        },
                    },
                )))
            }
            1..=3 => {
                let twin = format!("tw{}-{step}", k % 40);
                let mut c = call(
                    &twin,
                    t(2),
                    CallKind::Execute {
                        action: action(k),
                        answer: ExecuteAnswer::Joined {
                            name: name(&twin, k),
                            operation: OperationId(k),
                        },
                    },
                );
                c.minted = true;
                Some(ev(c))
            }
            4 => Some(granted(k, k + 1, "node-a", t(1))),
            _ => (k >= 500).then(|| finished_ok(k - 500, k - 499, 1, 0, t(3))),
        })
    })
}

/// Catches memory that operations leave behind as they come and go: waiting calls
/// joined, minted twins evicted, operations finished. Measured: 1.08 times a 200 kB
/// bound, the most of any mix, and 1.12 times a 50 kB one (where the uncounted first
/// nodes are about 5% of it).
#[test]
fn a_rolling_window_of_operations_stays_within_a_quarter_of_the_bound() {
    for bound in [50_000, 200_000, 2_000_000] {
        within(bound, rolling());
    }
}

/// The real memory and the counted bytes of records with no bound after `events`.
fn held(events: &[Event]) -> (usize, usize) {
    let base = live();
    let mut r = Records::new();
    for e in events {
        r.apply(e.clone());
    }
    let real = usize::try_from(live() - base).expect("the records hold memory");
    (real, r.bytes())
}

/// How much more real memory and counted bytes `long` holds than `short`; the two
/// differ only in the length of one text, so the difference is that text's copies.
fn grows(short: &[Event], long: &[Event]) -> (usize, usize) {
    let (a, b) = (held(short), held(long));
    (b.0 - a.0, b.1 - a.1)
}

/// An Execute by `by` of invocation `inv` (minted or not) that queued operation 1
/// under the REAPI name `name`.
fn execute(inv: &str, by: &str, minted: bool, name: &str) -> Event {
    let mut c = call(
        inv,
        t(0),
        CallKind::Execute {
            action: d(10),
            answer: ExecuteAnswer::Queued {
                name: name.to_owned(),
                operation: OperationId(1),
            },
        },
    );
    c.owner = owner(by);
    c.minted = minted;
    ev(c)
}

/// Catches copies of an id the records hold but do not count (plant
/// `RECORD_INDEX_ENTRIES = 0`: the record map's key, the last-seen index, the id's
/// list of keys and the id map's key go uncounted; plant `Waiter::bytes` without the
/// key's id: the waiting call's copy does): a record waiting on an operation, minted
/// or not, grows by exactly what the allocator is asked for when its id is 100 bytes
/// longer. A second caller's record (a collision, listed in the owner's) counts
/// every copy it adds too, and the id map's key once more: each record counts the
/// key, though the records of one id share it.
#[test]
fn every_copy_of_an_id_is_counted() {
    let one = |inv: &str, minted| [execute(inv, "", minted, "operations/1")];
    let two = |inv: &str, minted| {
        [
            execute(inv, "", minted, "operations/1"),
            execute(inv, "o2", minted, "operations/2"),
        ]
    };
    let long = "x".repeat(101);
    for minted in [false, true] {
        let (real, counted) = grows(&one("x", minted), &one(&long, minted));
        // The record's key and four index copies, the minted index, the waiting call.
        let copies = if minted { 7 } else { 6 };
        assert_eq!(real, copies * 100, "minted {minted}: the copies held");
        assert_eq!(counted, real, "minted {minted}: each copy counted");
        let (real, counted) = grows(&two("x", minted), &two(&long, minted));
        // Two records' copies, less the shared id map key, and the owner's list entry.
        assert_eq!(real, 2 * copies * 100, "minted {minted}: with a collision");
        assert_eq!(
            counted,
            real + 100,
            "minted {minted}: and the shared key again"
        );
    }
}

/// Catches a waiting call's REAPI name held but not counted (plant `Waiter::bytes`
/// as `size_of::<Self>()` alone): the name is held twice, in the row and in the
/// operation's list of waiting calls, and both copies count.
#[test]
fn a_waiting_calls_name_is_counted_in_its_row_and_its_operation() {
    let short = [execute("x", "", false, "operations/1")];
    let long = [execute(
        "x",
        "",
        false,
        &format!("operations/{}", "1".repeat(1000)),
    )];
    let (real, counted) = grows(&short, &long);
    assert_eq!(real, 2 * 999, "the name is held twice");
    assert_eq!(counted, real);
}

/// Catches a header text held but not counted (plant `Invocation::bytes` without
/// the owner, the tool, its version or the correlated id).
#[test]
fn a_records_header_texts_are_counted() {
    let header = |n: usize| {
        let mut c = call("x", t(0), CallKind::Downloaded(1));
        c.owner = owner(&"o".repeat(n));
        c.instance = "i".repeat(n);
        c.metadata = Metadata {
            tool: Some("t".repeat(n)),
            tool_version: Some("v".repeat(n)),
            correlated_invocations_id: Some("c".repeat(n)),
            ..Metadata::default()
        };
        [ev(c)]
    };
    let (real, counted) = grows(&header(1), &header(201));
    assert_eq!(
        real,
        5 * 200,
        "owner, instance, tool, version, correlated id"
    );
    assert_eq!(counted, real);
}

/// Catches an operation's attempts held in more than their length (plant: no
/// `reserve_exact` before the push in `Records::apply`'s grant: the list grows to 4
/// at its first lease): each lease adds one attempt to the operation and one to its
/// row, and the records grow by exactly what they count.
#[test]
fn each_lease_adds_what_it_counts() {
    let mut events = vec![queued("a", t(0), 10, 1)];
    for seq in 1..=4 {
        let before = events.clone();
        events.push(granted(1, seq, "node-a", t(1)));
        events.push(requeued(1, seq, RequeueReason::Silent, t(1)));
        let (real, counted) = grows(&before, &events);
        assert_eq!(counted, real, "lease {seq}");
    }
}

/// The `i`-th minted record joined to operation 1.
fn minted_twin(i: usize) -> Event {
    let Event::Call(mut c) = joined(&format!("m{i:03}"), t(0), 10, 1) else {
        unreachable!("joined makes a call")
    };
    c.minted = true;
    Event::Call(c)
}

/// The records after `twins` minted records joined `a`'s operation and a client's
/// record that needs all their room took it: their real memory and counted bytes.
fn after_twins_evicted(twins: usize) -> (usize, usize) {
    let a = queued("a", t(0), 10, 1);
    // Room for `a` and 100 twins; `c` fits beside `a` alone.
    let hundred: Vec<Event> = std::iter::once(a.clone())
        .chain((0..100).map(minted_twin))
        .collect();
    let bound = unbounded_bytes(&hundred);
    let c = wide_hit("c", bound - unbounded_bytes(&hundred[..1]));
    let events: Vec<Event> = std::iter::once(a)
        .chain((0..twins).map(minted_twin))
        .chain([c])
        .collect();
    let base = live();
    let mut r = Records::with_bound(ROW_CAP, bound);
    for e in &events {
        r.apply(e.clone());
    }
    assert_eq!(
        r.keys().map(ToString::to_string).collect::<Vec<_>>(),
        ["a", "c"],
        "every twin evicted"
    );
    let real = usize::try_from(live() - base).expect("the records hold memory");
    (real, r.bytes())
}

fn unbounded_bytes(events: &[Event]) -> usize {
    let mut r = Records::new();
    for e in events {
        r.apply(e.clone());
    }
    r.bytes()
}

/// A cache hit by `inv` whose record takes as near `room` bytes as 1,000 outputs'
/// paths allow, and no more.
fn wide_hit(inv: &str, room: usize) -> Event {
    let hit = |path: usize| {
        let mut l = listing(20, 0);
        l.outputs = (0..1000)
            .map(|k| Output {
                path: format!("{k:04}{}", "p".repeat(path)),
                digest: d(1),
                executable: false,
                directory: false,
            })
            .collect();
        ev(call(
            inv,
            t(1),
            CallKind::GetActionResult {
                action: d(20),
                hit: Some(l),
            },
        ))
    };
    let path = (0..)
        .take_while(|&p| unbounded_bytes(&[hit(p + 1)]) <= room)
        .last()
        .unwrap_or(0);
    hit(path)
}

/// Catches an evicted twin's place left in its operation's list of waiting calls
/// (plant: no `shrink_to_fit` after the retain in `Records::evict`): 100 minted
/// twins evicted to make room leave what one does.
#[test]
fn evicted_twins_leave_nothing_in_their_operation() {
    let (one, one_counted) = after_twins_evicted(1);
    let (hundred, hundred_counted) = after_twins_evicted(100);
    assert_eq!(hundred_counted, one_counted);
    assert_eq!(hundred, one, "real memory after 100 twins and after one");
}

/// The real memory each of `n` more events adds after `first`, applied in turn.
fn steps(first: &[Event], more: impl Fn(usize) -> Event, n: usize) -> Vec<usize> {
    let mut events = first.to_vec();
    (0..n)
        .map(|i| {
            let before = events.clone();
            events.push(more(i));
            grows(&before, &events).0
        })
        .collect()
}

/// Catches an id's list of keys, its owner's list of collisions or an operation's
/// list of waiting calls grown by `push` (plant: no `reserve_exact` before any one of
/// them): a list made by `push` has room for 4 and doubles, so the memory the k-th
/// caller adds jumps at the first and the fifth. Allocated to their length, each
/// caller adds the same: a record of its own (every index still one node) and one
/// slot in each list.
#[test]
fn each_caller_of_an_id_adds_the_same_memory() {
    let collision = |i: usize| {
        let mut c = call("x", t(0), CallKind::Downloaded(1));
        c.owner = owner(&format!("o{i}"));
        ev(c)
    };
    let collisions = steps(
        &[ev(call("x", t(0), CallKind::Downloaded(1)))],
        collision,
        8,
    );
    assert!(
        collisions.windows(2).all(|w| w[0] == w[1]),
        "each collision adds {collisions:?}"
    );
    let twin = |i: usize| joined(&format!("j{i}"), t(0), 10, 1);
    let twins = steps(&[queued("a", t(0), 10, 1)], twin, 8);
    assert!(
        twins.windows(2).all(|w| w[0] == w[1]),
        "each twin adds {twins:?}"
    );
}

/// The records after `n` minted callers of `x` other than its owner, then a client's
/// record that needs all their room took it: their real memory and counted bytes.
fn after_collisions_evicted(n: usize) -> (usize, usize) {
    let collision = |i: usize| {
        let mut c = call("x", t(0), CallKind::Downloaded(1));
        // Larger than `wide_hit`'s step, so that `c` leaves no room for one.
        c.owner = owner(&format!("o{i:03}{}", "p".repeat(2000)));
        c.minted = true;
        ev(c)
    };
    let x = ev(call("x", t(0), CallKind::Downloaded(1)));
    let hundred: Vec<Event> = std::iter::once(x.clone())
        .chain((0..100).map(collision))
        .collect();
    let bound = unbounded_bytes(&hundred);
    let c = wide_hit("c", bound - unbounded_bytes(&hundred[..1]));
    let events: Vec<Event> = std::iter::once(x)
        .chain((0..n).map(collision))
        .chain([c])
        .collect();
    let base = live();
    let mut r = Records::with_bound(ROW_CAP, bound);
    for e in &events {
        r.apply(e.clone());
    }
    assert_eq!(
        r.keys().map(ToString::to_string).collect::<Vec<_>>(),
        ["c", "x"],
        "every collision evicted"
    );
    let real = usize::try_from(live() - base).expect("the records hold memory");
    (real, r.bytes())
}

/// Catches an evicted collision's place left in its id's list of keys or its
/// owner's list of collisions (plant: no `shrink_to_fit` after either retain in
/// `Records::evict`): 100 collisions evicted leave what one does.
#[test]
fn evicted_collisions_leave_nothing_in_their_ids_lists() {
    let (one, one_counted) = after_collisions_evicted(1);
    let (hundred, hundred_counted) = after_collisions_evicted(100);
    assert_eq!(hundred_counted, one_counted);
    assert_eq!(
        hundred, one,
        "real memory after 100 collisions and after one"
    );
}

/// A run measured by lease `seq` of operation 1, with `numa` NUMA nodes.
fn ran(seq: u64, numa: u32) -> Event {
    Event::Ran {
        operation: OperationId(1),
        lease: lease(seq),
        run: Box::new(kbf_invocation::Run {
            usage: Some(kbf_invocation::RunUsage {
                numa: (0..numa)
                    .map(|node| kbf_invocation::NumaBytes { node, bytes: 1 })
                    .collect(),
                ..kbf_invocation::RunUsage::default()
            }),
            ..kbf_invocation::Run::default()
        }),
    }
}

/// The Command (and shape) of `d(10)` in invocation `a`, its cfg and label `n`
/// bytes each.
fn shaped(n: usize) -> Event {
    Event::Command {
        id: id("a"),
        owner: owner(""),
        action: d(10),
        command: d(11),
        shape: Some(Box::new(kbf_invocation::ActionShape {
            key: 7,
            cfg: Some("c".repeat(n)),
            target: Some(kbf_invocation::Target {
                kind: kbf_invocation::TargetKind::Target,
                label: kbf_invocation::Text::sent("l".repeat(n)),
            }),
            test: false,
        })),
    }
}

/// Catches a lease's run held but not counted (plant `run_bytes` without its NUMA
/// list, or without its box): it is held twice, in the operation's attempt and its
/// row's, and the records grow by exactly what they count. And a row's shape held but
/// not counted (plant `shape_bytes` without its texts or box).
#[test]
fn a_run_and_a_shape_add_what_they_count() {
    let base = vec![queued("a", t(0), 10, 1), granted(1, 1, "node-a", t(1))];
    for numa in [0, 40] {
        let mut events = base.clone();
        events.push(ran(1, numa));
        let (real, counted) = grows(&base, &events);
        assert!(real > 0, "the run is held");
        assert_eq!(counted, real, "{numa} NUMA nodes");
    }
    let (real, counted) = grows(
        &[queued("a", t(0), 10, 1)],
        &[queued("a", t(0), 10, 1), shaped(300)],
    );
    assert!(real > 600, "the shape's texts are held");
    assert_eq!(counted, real);
}
