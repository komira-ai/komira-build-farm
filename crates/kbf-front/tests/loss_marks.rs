//! Loss marks through the cache (docs/design/architecture-v2.md, section 8): a write
//! tier abandoned, or a block gone at a start, before the flusher put its blobs in the
//! store marks those blobs lost; the lost-blob gauge and counter on `/metrics` count
//! them; an action-cache entry naming one is a miss; a re-upload clears the mark.
//!
//! Every test runs the same [`Farm`] as `write_tier.rs`. Each test's comment names the
//! defect it is there to catch.

mod common;
#[allow(dead_code)]
#[path = "write_tier/support.rs"]
mod support;

use kbf_front::{CacheError, Losses, digest_to_proto};
use kbf_meta::{BlobAnswer, Role};
use kbf_objstore::Capabilities;
use kbf_proto::reapi::{ActionResult, OutputFile};
use kbf_store::Fs as _;
use kbf_tier::block_name;
use kbf_types::Digest;
use support::{Farm, SEGMENT, TestBlob, TierCache, blobs, start, verified};

const GIB: u64 = 1 << 30;

/// An action whose result names `output` as its one output file.
fn result_naming(output: &TestBlob) -> ActionResult {
    ActionResult {
        output_files: vec![OutputFile {
            path: "out".to_owned(),
            digest: Some(digest_to_proto(&output.digest)),
            ..Default::default()
        }],
        ..Default::default()
    }
}

async fn losses(cache: &TierCache) -> Losses {
    cache.losses().await.expect("losses")
}

/// `/metrics`' text for `losses`.
fn rendered(losses: Losses) -> String {
    let mut out = String::new();
    losses.render(&mut out);
    out
}

/// Catches: a tier abandoned with unflushed blobs whose digests are not marked lost
/// (the gauge and the counter stay 0 while the farm holds no copy of them); an
/// action-cache entry whose output was on it served as a hit; a lost blob answered
/// `NOT_FOUND` or with bytes; a re-upload that does not clear its digest's mark (the
/// gauge stays up); and the counter falling with the gauge.
#[tokio::test]
async fn an_abandoned_tier_marks_its_unflushed_blobs_lost_until_uploaded_again() {
    let farm = Farm::new(Capabilities::default());
    let cache = farm.start(GIB).await;
    let blobs = blobs(1, 2, 4096);
    cache.store_blobs(verified(&blobs)).await.expect("stored");
    let action = kbf_segments::sha256(b"an action");
    let result = result_naming(&blobs[0]);
    cache
        .write_action_result(Role::Daemon, action, &result)
        .await
        .expect("written");
    let hit = cache.action_result(&action).await.expect("lookup");
    assert!(hit.is_some(), "a hit while the tier holds its output");
    assert_eq!(losses(&cache).await, Losses::default());
    let ours = cache.tier_id().expect("tier");
    drop(cache);

    let fs = farm.fs.restart();
    let (cache, startup) = start(&fs, &farm.log, &farm.store, GIB, Some(ours))
        .await
        .expect("abandon");
    assert!(!startup.abandoned.is_empty());
    // The two blobs and the stored ActionResult were all staged on the abandoned tier.
    let staged = 3;
    assert_eq!(
        losses(&cache).await,
        Losses {
            lost_blobs: staged,
            blob_losses: staged,
        }
    );
    let text = rendered(losses(&cache).await);
    assert!(text.contains("\nkbf_cache_lost_blobs 3\n"), "{text}");
    assert!(text.contains("\nkbf_cache_blob_losses_total 3\n"), "{text}");
    assert!(
        text.contains("# TYPE kbf_cache_lost_blobs gauge\n"),
        "{text}"
    );
    assert!(
        text.contains("# TYPE kbf_cache_blob_losses_total counter\n"),
        "{text}"
    );

    let lookup = cache.action_result(&action).await.expect("lookup");
    assert!(lookup.is_none(), "a miss: its output's only copy is gone");
    for b in &blobs {
        assert_eq!(farm.log.answer(&b.digest), BlobAnswer::Unavailable);
        let read = cache.read_blob(&b.digest).await;
        assert!(matches!(read, Err(CacheError::Unreachable(_))), "{read:?}");
    }
    let digests: Vec<Digest> = blobs.iter().map(|b| b.digest).collect();
    assert_eq!(cache.find_missing(&digests).await.expect("find"), digests);

    cache
        .store_blobs(verified(&blobs[..1]))
        .await
        .expect("re-upload");
    assert_eq!(
        losses(&cache).await,
        Losses {
            lost_blobs: staged - 1,
            blob_losses: staged,
        },
        "the re-uploaded blob's mark is cleared; the counter keeps it"
    );
    assert_eq!(
        cache.read_blob(&blobs[0].digest).await.expect("healed"),
        blobs[0].bytes
    );
}

/// Catches: a restart that finds a staged block gone (the tier's contents lost) not
/// marking its blobs lost, or marking the blobs of a block that survived.
#[tokio::test]
async fn a_block_gone_at_a_start_marks_only_its_blobs_lost() {
    let farm = Farm::new(Capabilities::default());
    let cache = farm.start(GIB).await;
    let size = usize::try_from(SEGMENT).expect("size") + 1;
    let (gone, kept) = (TestBlob::new(1, size), TestBlob::new(2, size));
    for b in [&gone, &kept] {
        cache.store_blobs(vec![b.verified()]).await.expect("stored");
    }
    let object = farm.log.object_of(&gone.digest);
    assert_ne!(object, farm.log.object_of(&kept.digest));
    drop(cache);

    let fs = farm.fs.restart();
    fs.remove(&block_name(object)).expect("remove");
    let (cache, startup) = start(&fs, &farm.log, &farm.store, GIB, None)
        .await
        .expect("start");
    assert_eq!(startup.recovery.lost, [object]);
    assert_eq!(
        losses(&cache).await,
        Losses {
            lost_blobs: 1,
            blob_losses: 1,
        }
    );
    assert_eq!(
        cache.read_blob(&kept.digest).await.expect("kept"),
        kept.bytes
    );
}
