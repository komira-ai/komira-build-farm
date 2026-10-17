//! Reading an invocation back: how many records one read takes, what a restored record
//! says of what it was made from, and a read of a chunk the store lost. Each test names the
//! defect it catches.

use kbf_history::records::{RunOutcome, RunRecord, WireDigest};
use kbf_history::{
    Event, EventQueue, HistoryIndex, HistoryRoot, IndexRoot, Inline, InvKey, InvRef,
    InvocationView, Limits, Minter, QueueLimits, Reader, Today, Writer, WriterId,
};
use kbf_invocation::WallTime;
use kbf_objstore::{Capabilities, KeyPrefix, MemoryStore, ObjectStore, PageSize};
use kbf_state::fake::{Fake, Flaw};
use kbf_state::{ManualClock, Store};
use std::sync::Arc;

use super::{Durable, Gathered, RESTORED_SERIAL};
use crate::history::{HistoryReads, Shared};
use crate::http_invocations::Request;

struct Rig {
    durable: Durable,
    objects: Shared<MemoryStore>,
    inv: InvKey,
}

/// History of invocation `name` holding `n` runs of instance `main`, each in a chunk of
/// its own, closed, and a [`Durable`] over it.
async fn rig(name: &str, n: u64) -> Rig {
    let objects = Shared(Arc::new(MemoryStore::new(Capabilities {
        conditional_put: true,
        object_lock: false,
    })));
    let store: Arc<dyn Store> = Arc::new(Fake::with_flaw(
        Arc::new(ManualClock::default()),
        Flaw::None,
    ));
    let root = HistoryRoot::under(&KeyPrefix::new("kbf/").expect("a prefix")).expect("a root");
    let index = HistoryIndex::new(store, IndexRoot::default(), Arc::new(Inline));
    let (minter, queue) = (
        Arc::new(Minter::new(0)),
        Arc::new(EventQueue::new(QueueLimits::default())),
    );
    let limits = Limits {
        chunk_bytes: 1,
        txns_per_step: 64,
        ..Limits::default()
    };
    let mut writer = Writer::new(
        objects.clone(),
        root.clone(),
        index.clone(),
        WriterId(1),
        Arc::clone(&minter),
        Arc::clone(&queue),
        limits,
    );
    let inv = InvRef::named(name);
    for i in 0..n {
        let minted = minter.mint(&inv.key, 1_000 + i);
        let mut record = RunRecord {
            instance: "main".to_owned(),
            action: Some(WireDigest::new([u8::try_from(i % 250).unwrap_or(0); 32], i)),
            outcome: RunOutcome::Ok.into(),
            ..RunRecord::default()
        };
        record.set_run_id(minted.id);
        assert!(queue.offer(Event::Run {
            inv: inv.clone(),
            record: Box::new(record),
            failed: None,
            stream_opened_ms: minted.stream_opened_ms,
        }));
    }
    assert!(queue.offer(Event::Close { inv: inv.clone() }));
    for _ in 0..50 {
        writer.step(5_000).await;
        if writer.pending_rows() == 0 && writer.unwritten_chunks() == 0 {
            break;
        }
    }
    let reader = Reader::new(objects.clone(), root, index);
    let reads = HistoryReads::new(reader, Arc::default(), Arc::new(Today::new(0)));
    Rig {
        durable: Durable::new(Arc::new(reads)),
        objects,
        inv: inv.key,
    }
}

fn allow(_: &str) -> bool {
    true
}

/// Catches: a read that stops at the first page of chunks (an invocation of more than 32
/// chunks missing rows with nothing to say so), one that never stops (no bound on the
/// records one request holds), a bound that marks an invocation capped when its last
/// page was the one that reached it, and a reader that no longer answers (its header
/// lists an instance `allow` refuses) given records.
#[tokio::test]
async fn a_read_gathers_every_page_up_to_its_bound() {
    let mut rig = rig("big", 40).await;
    let all = rig.durable.gather(&rig.inv, &allow).await.unwrap().unwrap();
    assert_eq!(
        (all.runs.len(), all.capped),
        (40, false),
        "two pages of chunks"
    );
    rig.durable.max_records = 10;
    let cut = rig.durable.gather(&rig.inv, &allow).await.unwrap().unwrap();
    assert_eq!(
        (cut.runs.len(), cut.capped),
        (32, true),
        "one page, and more behind"
    );
    // The bound is reached by the last page: nothing more to read, so not capped.
    let small = self::rig("small", 3).await;
    let mut durable = small.durable;
    durable.max_records = 1;
    let got = durable.gather(&small.inv, &allow).await.unwrap().unwrap();
    assert_eq!((got.runs.len(), got.capped), (3, false));
    let none = durable.gather(&small.inv, &|_: &str| false).await.unwrap();
    assert!(none.is_none());
}

fn request(id: &str) -> Request {
    Request::parse(&format!("/v1/invocations/{id}"), "").expect("a request")
}

/// Catches: a restored record that is complete when its reading was cut at the bound or
/// had records it could not use, a hole with no time, a serial that is a recorder's (a
/// page token made for a record the recorder held would be read against it) or that
/// repeats (two records restored one after the other taken for one), and times and
/// instances that are not the header's.
#[tokio::test]
async fn a_restored_record_says_what_it_was_made_from() {
    let rig = rig("made", 2).await;
    let view = rig
        .durable
        .reads
        .invocation(&rig.inv, &allow)
        .await
        .unwrap()
        .unwrap();
    assert!(view.complete);
    let gathered = Gathered {
        runs: rig
            .durable
            .gather(&rig.inv, &allow)
            .await
            .unwrap()
            .unwrap()
            .runs,
        hits: Vec::new(),
        capped: false,
    };
    let request = request("made");
    let whole = rig.durable.restore(&request, &view, &gathered);
    assert!(whole.complete() && !whole.rows_capped());
    assert_eq!(whole.gap_from(), None, "a complete record has no hole");
    assert!(whole.serial() & RESTORED_SERIAL != 0);
    let h = &view.header;
    assert_eq!(
        whole.first_seen(),
        WallTime::from_unix_micros(h.first_seen_ms * 1_000)
    );
    assert_eq!(
        whole.last_seen(),
        WallTime::from_unix_micros(h.last_seen_ms * 1_000)
    );
    assert_eq!(whole.instances().iter().collect::<Vec<_>>(), ["main"]);
    assert_eq!(whole.rows().len(), 2);

    let capped = Gathered {
        runs: gathered.runs.clone(),
        hits: Vec::new(),
        capped: true,
    };
    let cut = rig.durable.restore(&request, &view, &capped);
    assert!(!cut.complete() && cut.rows_capped());
    assert_eq!(
        cut.gap_from(),
        Some(WallTime::from_unix_micros(h.last_chunk_ms * 1_000))
    );
    assert_ne!(cut.serial(), whole.serial(), "each restoring has its own");
    // A record with a digest that is no digest is a hole too.
    let mut broken = gathered.runs.clone();
    broken[0].action = None;
    let skipped = Gathered {
        runs: broken,
        hits: Vec::new(),
        capped: false,
    };
    let torn = rig.durable.restore(&request, &view, &skipped);
    assert!(!torn.complete() && !torn.rows_capped());
    assert_eq!(torn.rows().len(), 1);
    // A header that was never closed is a hole whatever was read.
    let open = InvocationView {
        complete: false,
        ..view.clone()
    };
    let unclosed = rig.durable.restore(&request, &open, &gathered);
    assert!(!unclosed.complete() && !unclosed.rows_capped());
    assert_eq!(unclosed.gap_from(), cut.gap_from());
}

/// Catches: a read that fails answered as a read that found nothing (a record whose chunk
/// the object store lost shown as an invocation with fewer rows), a failure that is not
/// carried to the caller for it to answer `503`.
#[tokio::test]
async fn a_chunk_the_store_lost_is_an_error_not_fewer_records() {
    let rig = rig("lost", 3).await;
    let rows = rig.durable.reads.invocation(&rig.inv, &allow).await;
    assert!(rows.unwrap().is_some());
    let prefix = KeyPrefix::new("kbf/").expect("a prefix");
    let size = PageSize::new(100).expect("a size");
    let listed = rig
        .objects
        .list(&prefix, None, size)
        .await
        .expect("a listing");
    assert!(!listed.objects.is_empty());
    for object in listed.objects {
        rig.objects.delete(&object.key).await.expect("a delete");
    }
    let got = rig.durable.gather(&rig.inv, &allow).await;
    assert!(
        matches!(got, Err(kbf_history::ReadError::Object(_))),
        "{:?}",
        got.map(|g| g.map(|g| g.runs.len()))
    );
}
