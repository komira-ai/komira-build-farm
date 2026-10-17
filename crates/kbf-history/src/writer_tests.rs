use super::*;

#[test]
fn the_slowest_list_keeps_the_top_twenty_slowest_first() {
    // Catches: a list that grows without bound, keeps the fastest, or puts a tie
    // ahead of an earlier run of the same time.
    let mut list = Vec::new();
    for us in 0..30u64 {
        let r = RunRecord {
            seq: us as u32,
            ..RunRecord::default()
        };
        rank(&mut list, &r, us % 25);
    }
    assert_eq!(list.len(), SLOWEST);
    let times: Vec<u64> = list.iter().map(|s| s.us).collect();
    assert_eq!(times[..3], [24, 23, 22]);
    assert!(times.windows(2).all(|w| w[0] >= w[1]));
    let r = RunRecord::default();
    rank(&mut list, &r, 0);
    assert!(
        list.iter().all(|s| s.us >= 5),
        "a run faster than every kept one stays out"
    );
}

#[test]
fn a_header_notes_instances_once_sorted_and_the_time_span() {
    // Catches: an instance listed twice (an all-must-allow check would ask twice),
    // or a first-seen time that a later, earlier-stamped record does not move.
    let mut h = InvocationHeader::default();
    note(&mut h, "b", 20);
    note(&mut h, "a", 30);
    note(&mut h, "b", 10);
    assert_eq!(h.instances, ["a", "b"]);
    assert_eq!((h.first_seen_ms, h.last_seen_ms), (10, 30));
}

#[test]
fn a_chunk_breaks_on_a_new_day_or_stream_but_not_for_hits() {
    // Catches: two streams in one chunk (the block index would find the wrong run)
    // or a chunk spanning two days (its key carries one).
    let d1 = Day::from_number(20_261_016);
    let d2 = Day::from_number(20_261_017);
    let empty = Building::default();
    assert!(!empty.breaks_on(d1, Some(1)));
    let b = Building {
        day: Some(d1),
        stream: Some(1),
        ..Building::default()
    };
    assert!(!b.breaks_on(d1, Some(1)));
    assert!(!b.breaks_on(d1, None));
    assert!(b.breaks_on(d1, Some(2)));
    assert!(b.breaks_on(d2, None));
}

#[test]
fn a_merged_header_sums_counts_joins_instances_and_widens_the_span() {
    // Catches: a merge that keeps only the stored counts (what the writer added while
    // the read failed would vanish), instances listed twice, and an addition with no
    // records (a close alone, first seen 0) that moves the first-seen time to 0.
    let stored = InvocationHeader {
        record_id: "b1".into(),
        instances: vec!["a".into(), "c".into()],
        runs: 3,
        hits: 1,
        failed: 1,
        chunks: 2,
        first_seen_ms: 10,
        last_seen_ms: 50,
        last_chunk_ms: 60,
        state: InvState::Closed.into(),
        ..InvocationHeader::default()
    };
    let mut added = InvocationHeader::default();
    note(&mut added, "c", 5);
    note(&mut added, "b", 40);
    added.runs = 1;
    added.hits = 4;
    added.failed = 1;
    added.dropped = 2;
    added.chunks = 1;
    added.last_chunk_ms = 70;
    let h = merge(stored.clone(), &added);
    assert_eq!(h.instances, ["a", "b", "c"]);
    assert_eq!(
        (h.runs, h.hits, h.failed, h.dropped, h.chunks),
        (4, 5, 2, 2, 3)
    );
    assert_eq!(
        (h.first_seen_ms, h.last_seen_ms, h.last_chunk_ms),
        (5, 50, 70)
    );
    assert_eq!((h.state(), h.record_id.as_str()), (InvState::Open, "b1"));
    let closed = InvocationHeader {
        state: InvState::Closed.into(),
        ..InvocationHeader::default()
    };
    let h = merge(stored, &closed);
    assert_eq!(
        (h.first_seen_ms, h.runs, h.state()),
        (10, 3, InvState::Closed)
    );
}

#[test]
fn a_header_lists_at_most_64_instances_and_marks_the_rest_truncated() {
    // Catches: a header that grows with every instance name a client sends (the names
    // are the client's, and every reader check walks the list), a truncation that is
    // not marked (a reader would authorize on a partial list), and a merge that drops
    // the mark when the stored header already lists every name the addition kept.
    let mut h = InvocationHeader::default();
    for n in 0..MAX_INSTANCES {
        note(&mut h, &format!("i{n:03}"), 1);
    }
    assert_eq!(h.instances.len(), MAX_INSTANCES);
    assert!(!h.instances_truncated, "64 names fit");
    note(&mut h, "i000", 2);
    assert!(
        !h.instances_truncated,
        "a name already listed is not one more"
    );
    note(&mut h, "zzz", 3);
    assert_eq!(h.instances.len(), MAX_INSTANCES);
    assert!(h.instances_truncated);
    let stored = InvocationHeader {
        instances: h.instances.clone(),
        ..InvocationHeader::default()
    };
    let merged = merge(stored, &h);
    assert_eq!(merged.instances.len(), MAX_INSTANCES);
    assert!(merged.instances_truncated, "the addition's mark is kept");
}
