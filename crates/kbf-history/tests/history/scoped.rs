//! The `Scoped` store refuses every key outside `history/` before the store sees it.

use std::sync::atomic::Ordering;

use bytes::Bytes;
use kbf_history::Scoped;
use kbf_objstore::{ByteRange, KeyPrefix, ObjectKey, ObjectStore, ObjectStoreError, PageSize};

use crate::support::{Objects, root};

fn denied(e: ObjectStoreError) -> bool {
    matches!(e, ObjectStoreError::Service { status: 403, ref code, .. } if code == "AccessDenied")
}

#[test]
fn a_cas_key_is_refused_by_every_call_and_never_reaches_the_store() {
    // Catches: a history writer bug that builds a `cas/` key writing, reading or
    // deleting a blob (defence in depth behind the prefix-limited credential).
    let objects = Objects::new();
    let scoped = Scoped::history(objects.clone(), &root());
    assert_eq!(scoped.prefix().as_str(), "kbf/history/");
    let cas = ObjectKey::new("kbf/cas/1/2").unwrap();
    let near = ObjectKey::new("kbf/history-x/1").unwrap();
    let range = ByteRange::new(0, 1).unwrap();
    futures::executor::block_on(async {
        for key in [&cas, &near] {
            assert!(denied(
                scoped
                    .put_new(key, Bytes::from_static(b"x"), None)
                    .await
                    .unwrap_err()
            ));
            assert!(denied(scoped.get_range(key, range).await.unwrap_err()));
            assert!(denied(scoped.may_hold(key).await.unwrap_err()));
            assert!(denied(scoped.delete(key).await.unwrap_err()));
        }
        let page = PageSize::new(10).unwrap();
        let err = scoped
            .list(&KeyPrefix::new("kbf/").unwrap(), None, page)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("outside kbf/history/"), "{err}");
    });
    let c = objects.counts();
    let calls = [&c.puts, &c.gets, &c.may_holds, &c.deletes, &c.lists];
    assert!(calls.iter().all(|n| n.load(Ordering::SeqCst) == 0));
}

#[test]
fn a_history_key_passes_through() {
    // Catches: a wrapper that refuses its own prefix, or answers without asking the
    // store.
    let objects = Objects::new();
    let scoped = Scoped::history(objects.clone(), &root());
    let key = ObjectKey::new("kbf/history/r30/inv/x").unwrap();
    futures::executor::block_on(async {
        scoped
            .put_new(&key, Bytes::from_static(b"abc"), None)
            .await
            .unwrap();
        let got = scoped
            .get_range(&key, ByteRange::new(1, 5).unwrap())
            .await
            .unwrap();
        assert_eq!(&got[..], b"bc");
        assert!(scoped.may_hold(&key).await.unwrap());
        let page = scoped
            .list(
                &KeyPrefix::new("kbf/history/r30/").unwrap(),
                None,
                PageSize::new(10).unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(page.objects.len(), 1);
        scoped.delete(&key).await.unwrap();
    });
    assert_eq!(scoped.capabilities(), objects.capabilities());
    assert_eq!(scoped.inner().counts().deletes.load(Ordering::SeqCst), 1);
}
