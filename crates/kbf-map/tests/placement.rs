//! Placement: who owns a digest, and how that moves with the map.
//!
//! Each test names the defect it catches; the mutant that was planted to watch it fail
//! is in the pull request.

mod common;

use common::{digests, ids, map, member};
use kbf_map::{Map, MemberId, MemberState};
use kbf_types::{Digest, DigestFunction};

use MemberState::{Active, CatchingUp, Down, Draining, Joining};

/// Six members of unequal weight, all active.
const SIX: [(u64, u32); 6] = [(11, 1), (12, 1), (13, 2), (14, 3), (15, 1), (16, 2)];

fn six(r: u8, skip: Option<u64>) -> Map {
    let members = SIX
        .iter()
        .filter(|(id, _)| Some(*id) != skip)
        .map(|&(id, w)| member(id, w, Active))
        .collect();
    map(1, r, members)
}

fn sorted(list: &[MemberId]) -> Vec<MemberId> {
    let mut out = list.to_vec();
    out.sort();
    out
}

fn without(list: &[MemberId], id: MemberId) -> Vec<MemberId> {
    list.iter().copied().filter(|m| *m != id).collect()
}

/// `after` is `before` with `gone` taken out and the next member appended, or `before`
/// itself when `gone` was not an owner.
fn assert_only_gone_blobs_move(before: &[MemberId], after: &[MemberId], gone: MemberId) {
    if before.contains(&gone) {
        let kept = without(before, gone);
        assert_eq!(
            &after[..kept.len()],
            &kept[..],
            "owners that stay keep their rank"
        );
        assert_eq!(after.len(), before.len());
        assert!(
            !before.contains(after.last().unwrap()),
            "a new owner fills the slot"
        );
    } else {
        assert_eq!(before, after, "a digest {gone} did not own must not move");
    }
}

// Catches: a placement that reshuffles digests when membership changes (a modulo or a
// ring with shared state), and a score that depends on the other members. Exact per
// digest, for removal and for the reverse (adding), at R=1 and R=2.
#[test]
fn removing_or_adding_a_member_moves_only_its_blobs() {
    for r in [1, 2] {
        for &(gone, _) in &SIX {
            let (full, short) = (six(r, None), six(r, Some(gone)));
            let mut owned = 0;
            for d in digests(20_000) {
                let (before, after) = (full.write_owners(&d), short.write_owners(&d));
                owned += usize::from(before.contains(&MemberId(gone)));
                assert_only_gone_blobs_move(&before, &after, MemberId(gone));
            }
            assert!(
                owned > 0,
                "member {gone} owned nothing: the test would be vacuous"
            );
        }
    }
}

// Catches: the same property over the 100,000 digests of the plan, once, for one member.
#[test]
fn removal_at_one_hundred_thousand_digests() {
    let (full, short) = (six(2, None), six(2, Some(14)));
    for d in digests(100_000) {
        let before = full.write_owners(&d);
        assert_only_gone_blobs_move(&before, &short.write_owners(&d), MemberId(14));
    }
}

// Catches: a score that ignores the weight (every member then gets an equal share).
#[test]
fn weights_set_the_share_of_primaries() {
    let m = map(1, 1, vec![member(1, 1, Active), member(2, 2, Active)]);
    let n = 90_000;
    let heavy = digests(n)
        .iter()
        .filter(|d| m.write_owners(d) == ids(&[2]))
        .count();
    let share = heavy as f64 / n as f64;
    assert!(
        (share - 2.0 / 3.0).abs() < 0.01,
        "weight-2 share {share}, want 2/3 +- 0.01"
    );
}

// Catches: a score that does not mix in the member id (all members then score alike and
// the lowest id wins everything).
#[test]
fn equal_members_share_evenly() {
    let m = map(1, 1, (1..=6).map(|i| member(i, 1, Active)).collect());
    let n = 60_000;
    let mut wins = [0usize; 6];
    for d in digests(n) {
        wins[(m.write_owners(&d)[0].0 - 1) as usize] += 1;
    }
    for (i, w) in wins.iter().enumerate() {
        let share = *w as f64 / n as f64;
        assert!(
            (share - 1.0 / 6.0).abs() < 0.01,
            "member {} has share {share}",
            i + 1
        );
    }
}

fn digest(hash: [u8; 32], size: u64) -> Digest {
    Digest::new(DigestFunction::Sha256, hash, size)
}

// Catches: any change to the hash, its seed, the byte layout or the score formula. The
// lists were produced by an independent implementation (Python, `math.log`), not by this
// crate.
#[test]
fn rankings_are_pinned() {
    let m = six(3, None);
    let cases = [
        (digest([0; 32], 0), [11, 16, 13]),
        (digest([0xff; 32], 1), [13, 16, 14]),
        (digest(std::array::from_fn(|i| i as u8), 4096), [13, 16, 11]),
        (
            digest(std::array::from_fn(|i| 32 + i as u8), 1 << 40),
            [16, 13, 12],
        ),
        (digest([7; 32], 123_456), [16, 14, 13]),
    ];
    for (d, want) in cases {
        assert_eq!(m.write_owners(&d), ids(&want), "digest {d}");
    }
}

// Catches: a size that is ignored (two digests with one hash and two sizes share an
// owner list for every hash) or a platform-dependent order of equal scores.
#[test]
fn the_size_is_part_of_the_key() {
    let m = six(3, None);
    let moved = (0..200u8)
        .filter(|i| m.write_owners(&digest([*i; 32], 1)) != m.write_owners(&digest([*i; 32], 2)))
        .count();
    assert!(
        moved > 50,
        "only {moved} of 200 owner lists changed with the size"
    );
}

/// Five members, the sixth (`X`) in `state`; compare against the map without it and the
/// map with it active.
const X: u64 = 13;

fn six_with(state: MemberState) -> Map {
    let members = SIX
        .iter()
        .map(|&(id, w)| member(id, w, if id == X { state } else { Active }))
        .collect();
    map(2, 2, members)
}

/// `base` then the members of `extra` it lacks.
fn then(base: &[MemberId], extra: &[MemberId]) -> Vec<MemberId> {
    let mut out = base.to_vec();
    out.extend(extra.iter().copied().filter(|m| !base.contains(m)));
    out
}

// Catches (rows Joining and CatchingUp of the table): a joiner that is not a write owner
// at once, or one read before the members that hold the data. Read order is the owners
// without the joiner, then the joiner (and whoever else it displaced from the write set).
#[test]
fn a_joining_node_takes_writes_at_once_and_is_read_last() {
    let x = MemberId(X);
    for state in [Joining, CatchingUp] {
        let (m, active) = (six_with(state), six_with(Active));
        let without_x = six(2, Some(X));
        let mut joined = 0;
        for d in digests(5_000) {
            let (mine, theirs) = (m.write_owners(&d), active.write_owners(&d));
            assert_eq!(sorted(&mine), sorted(&theirs), "the same write set");
            let base = without_x.write_owners(&d);
            let order = m.read_order(&d);
            assert_eq!(order, then(&base, &m.write_owners(&d)), "{state:?}");
            if m.write_owners(&d).contains(&x) {
                joined += 1;
                assert_eq!(order.last(), Some(&x), "{state:?} is read last");
            } else {
                assert_eq!(order, base);
            }
        }
        assert_ne!(joined, 0, "{state:?} owned nothing");
    }
}

// Catches (row Active): an active member that is not first-class for both.
#[test]
fn an_active_node_reads_and_writes_in_rank_order() {
    let m = six_with(Active);
    for d in digests(2_000) {
        assert_eq!(m.write_owners(&d), m.read_order(&d));
        assert_eq!(m.write_owners(&d).len(), 2);
    }
}

// Catches (row Draining): a draining member that still takes writes, or one read after
// its successor (which has no data yet).
#[test]
fn a_draining_node_takes_no_writes_and_is_read_before_its_successor() {
    let x = MemberId(X);
    let (m, active, without_x) = (six_with(Draining), six_with(Active), six(2, Some(X)));
    let mut held = 0;
    for d in digests(5_000) {
        let write = m.write_owners(&d);
        assert_eq!(
            write,
            without_x.write_owners(&d),
            "its successor takes the slot"
        );
        assert!(!write.contains(&x));
        let order = m.read_order(&d);
        assert_eq!(order, then(&active.write_owners(&d), &write));
        held += usize::from(order.contains(&x));
    }
    assert_ne!(held, 0, "the draining member was never read");
}

// Catches (row Down): a down member whose slot is given away at once (flapping would
// remap its blobs twice), or one still listed for reading.
#[test]
fn a_down_node_keeps_its_write_slot_and_is_not_read() {
    let x = MemberId(X);
    let (m, active) = (six_with(Down), six_with(Active));
    let mut kept = 0;
    for d in digests(5_000) {
        assert_eq!(m.write_owners(&d), active.write_owners(&d));
        assert_eq!(m.read_order(&d), without(&active.write_owners(&d), x));
        kept += usize::from(m.write_owners(&d).contains(&x));
    }
    assert_ne!(kept, 0, "the down member owned nothing");
}

// Catches: a write set padded with a member that cannot take writes.
#[test]
fn too_few_writers_give_fewer_owners() {
    let m = map(1, 2, vec![member(1, 1, Active), member(2, 1, Draining)]);
    let d = digest([3; 32], 3);
    assert_eq!(m.write_owners(&d), ids(&[1]));
    let mut order = m.read_order(&d);
    order.sort();
    assert_eq!(
        order,
        ids(&[1, 2]),
        "both hold data; neither is listed twice"
    );
}

// Catches: a lookup across generations that forgets the previous owners, or resolves one
// to the wrong map. A member that left between the generations is still tried, after the
// current owners, and comes back as the `Member` the previous map holds.
#[test]
fn the_previous_generation_is_read_after_the_current_one() {
    let (prev, cur) = (six(2, None), six(2, Some(14)));
    let mut appended = 0;
    for d in digests(5_000) {
        let now = cur.read_order(&d);
        assert!(
            cur.read_order_with_previous(None, &d)
                .iter()
                .map(|m| m.id)
                .eq(now.iter().copied())
        );
        let across = cur.read_order_with_previous(Some(&prev), &d);
        let across_ids: Vec<MemberId> = across.iter().map(|m| m.id).collect();
        assert_eq!(across_ids, then(&now, &prev.read_order(&d)));
        assert_eq!(&across_ids[..now.len()], &now[..]);
        for m in &across {
            let holder = if now.contains(&m.id) { &cur } else { &prev };
            assert_eq!(
                Some(*m),
                holder.member(m.id),
                "resolved from the map that has it"
            );
        }
        appended += across.len() - now.len();
    }
    assert_ne!(appended, 0, "no digest needed the previous generation");
}

/// A member of `members` in `state`, all others active, for the joiner properties.
fn with_states(r: u8, states: &[(u64, MemberState)]) -> Map {
    let members = SIX
        .iter()
        .map(|&(id, w)| {
            let state = states
                .iter()
                .find(|(i, _)| *i == id)
                .map_or(Active, |(_, s)| *s);
            member(id, w, state)
        })
        .collect();
    map(3, r, members)
}

// Catches (the plan's "joining node not an owner until active"): a joiner ranked by score
// alone, which makes it the chain head, the only flusher and the FindMissing target of a
// slice it holds nothing of. Precisely: while a settled writer exists a joiner is never
// rank 1; it is still in the write set (the set is the top r by score, as for an active
// member); and it is last in the read order. Several joiners at once, r = 1, 2 and 3, so
// that the top r can be all joiners.
#[test]
fn a_joiner_is_never_rank_one_but_still_takes_writes_and_is_read_last() {
    let joiners = [(13, Joining), (14, CatchingUp), (16, Joining)];
    for r in [1u8, 2, 3] {
        let (m, as_active) = (with_states(r, &joiners), with_states(r, &[]));
        let (mut in_set, mut rank_one_by_score) = (0, 0);
        for d in digests(5_000) {
            let owners = m.write_owners(&d);
            assert_eq!(owners.len(), usize::from(r));
            let first = m.member(owners[0]).unwrap();
            assert!(first.state == Active, "r={r}: {} is rank 1", first.id);
            let by_score = as_active.write_owners(&d);
            rank_one_by_score +=
                usize::from(joiners.iter().any(|(i, _)| MemberId(*i) == by_score[0]));
            let mut seen_joiner = false;
            for id in &owners {
                let joiner = m.member(*id).unwrap().state != Active;
                assert!(joiner || !seen_joiner, "settled owners come before joiners");
                seen_joiner |= joiner;
                in_set += usize::from(joiner);
            }
            let order = m.read_order(&d);
            let last_settled = order
                .iter()
                .rposition(|i| m.member(*i).unwrap().state == Active);
            let first_joiner = order
                .iter()
                .position(|i| m.member(*i).unwrap().state != Active);
            if let (Some(a), Some(b)) = (last_settled, first_joiner) {
                assert!(
                    a < b,
                    "r={r}: a joiner is read before a settled owner: {order:?}"
                );
            }
        }
        // With r = 1 the only slot is rank 1, so a joiner owns nothing while a settled
        // writer exists (its slice reaches it by the catch-up copy); from r = 2 on it
        // shares the write set.
        if r == 1 {
            assert_eq!(in_set, 0);
        } else {
            assert_ne!(in_set, 0, "r={r}: no joiner took a write: vacuous");
        }
        assert_ne!(
            rank_one_by_score, 0,
            "r={r}: no joiner would have won by score: vacuous"
        );
    }
}

// Catches: no answer (or a refusal) when only joiners can take writes, as on a fresh
// farm. The choice is documented on `write_owners`: the best-scored joiner is primary
// then, and the order is the one an active farm would give.
#[test]
fn with_only_joiners_the_best_scored_one_is_primary() {
    let all_joining = |state| {
        map(
            1,
            2,
            SIX.iter().map(|&(id, w)| member(id, w, state)).collect(),
        )
    };
    let active = all_joining(Active);
    for state in [Joining, CatchingUp] {
        let m = all_joining(state);
        for d in digests(1_000) {
            assert_eq!(m.write_owners(&d), active.write_owners(&d));
            assert_eq!(m.read_order(&d), active.write_owners(&d));
        }
    }
}

/// FNV-1a 64 over the member ids (one byte each) of the owner lists, in order.
fn fnv_of_owner_lists(m: &Map, n: u64) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for d in digests(n) {
        for id in m.write_owners(&d) {
            h = (h ^ id.0).wrapping_mul(0x0100_0000_01b3);
        }
    }
    h
}

// Catches: a change of one ulp anywhere in the score that moves a single owner list. The
// number is the FNV-1a 64 checksum of the rank 1..3 owner lists of 1,000 digests over
// SIX, produced by the Python reference (it repeats the Rust operations on Python floats;
// see the bit-exact test in `hrw.rs` for how); a changed list changes the checksum.
#[test]
fn the_owner_lists_of_a_thousand_digests_have_a_pinned_checksum() {
    assert_eq!(
        fnv_of_owner_lists(&six(3, None), 1_000),
        0x016c_2fbb_5ba6_c4db
    );
}

fn count_ranks(m: &Map, n: u64) -> ([usize; 6], [usize; 6]) {
    let (mut first, mut second) = ([0; 6], [0; 6]);
    for d in digests(n) {
        let owners = m.write_owners(&d);
        first[(owners[0].0 - 11) as usize] += 1;
        second[(owners[1].0 - 11) as usize] += 1;
    }
    (first, second)
}

// Catches: weights swapped between members, a score that ignores or misapplies a weight,
// or any drift of the distribution. The exact counts over `digests(20_000)` for SIX (ids
// 11 to 16, weights 1 1 2 3 1 2, total 10) come from the Python reference.
//
// Primaries follow the weights: expected 2000 2000 4000 6000 2000 4000, within the
// sampling noise of a binomial draw (up to about 2.5 percent for 2000, 1.3 for 6000).
//
// The replica (rank 2) share is NOT proportional to the weight. The probability that
// member j is second is the sum over i != j of (w_i / W) * w_j / (W - w_i): the winner is
// some i, and j then wins among the rest. That is 2301.6 2301.6 4047.6 5000.0 2301.6
// 4047.6 of 20,000, so the weight-3 member holds 25.0 percent of the replicas for 30
// percent of the primaries, and each weight-1 member 11.5 percent for 10 percent of the
// primaries: heavier members are over-represented as primaries and under-represented as
// replicas. Weights balance the primary load (and the flush work, which follows rank 1),
// not the total bytes held.
#[test]
fn primaries_and_replicas_have_pinned_counts() {
    let (first, second) = count_ranks(&six(2, None), 20_000);
    assert_eq!(first, [1945, 2014, 4111, 5915, 2000, 4015]);
    assert_eq!(second, [2252, 2375, 4021, 5061, 2257, 4034]);
}

// Catches: a score in which a member's weight reaches the others, or a placement that
// reshuffles when a weight changes. Raising the weight of 14 from 3 to 6 moves blobs only
// to 14 (exact: the ranking of the other five is identical before and after, for every
// digest, which also rules out a move between two others), and the number moved matches
// the expected change of its share, 6/13 - 3/10 = 16.15 percent of 20,000 = 3231; the
// binomial deviation is about 52, and the reference counts 3236, so a tolerance of 150
// (about 3 deviations) is allowed.
#[test]
fn changing_a_weight_moves_blobs_only_to_or_from_that_member() {
    let with_weight = |w14: u32, r: u8| {
        let members = SIX
            .iter()
            .map(|&(id, w)| member(id, if id == 14 { w14 } else { w }, Active))
            .collect();
        map(1, r, members)
    };
    {
        let (before, after) = (with_weight(3, 6), with_weight(6, 6));
        for d in digests(20_000) {
            assert_eq!(
                without(&before.write_owners(&d), MemberId(14)),
                without(&after.write_owners(&d), MemberId(14)),
                "digest {d}: others changed order"
            );
        }
    }
    let (before, after) = (with_weight(3, 1), with_weight(6, 1));
    let (mut to_14, mut other) = (0usize, 0usize);
    for d in digests(20_000) {
        let (a, b) = (before.write_owners(&d), after.write_owners(&d));
        if a != b {
            if b == ids(&[14]) {
                to_14 += 1;
            } else {
                other += 1;
            }
        }
    }
    assert_eq!(other, 0, "a blob moved without involving member 14");
    assert!(
        to_14.abs_diff(3231) <= 150,
        "{to_14} moved to 14, want 3231 +- 150"
    );
    assert_eq!(to_14, 3236, "the reference's exact count");
}
