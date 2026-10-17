//! The shared object store. Each test names the defect it catches.

use std::sync::Arc;

use bytes::Bytes;
use kbf_objstore::{
    ByteRange, Capabilities, KeyPrefix, MemoryStore, ObjectKey, ObjectStore, ObjectStoreError,
    PageSize,
};

use super::Shared;

/// Catches: a handle on the store that is not the store: a write through one handle that
/// the other cannot read (the writer's chunks invisible to the reader), a capability,
/// listing, deletion or `may_hold` answered by something else than the store itself.
#[tokio::test]
async fn two_handles_are_one_store() {
    let capabilities = Capabilities {
        conditional_put: true,
        object_lock: false,
    };
    let writer = Shared(Arc::new(MemoryStore::new(capabilities)));
    let reader = writer.clone();
    assert_eq!(reader.capabilities(), capabilities);
    let key = ObjectKey::new("kbf/history/a").expect("a key");
    writer
        .put_new(&key, Bytes::from_static(b"chunk"), None)
        .await
        .expect("a write");
    let range = ByteRange::new(1, 3).expect("a range");
    assert_eq!(reader.get_range(&key, range).await.unwrap(), "hun");
    assert!(reader.may_hold(&key).await.unwrap());
    let prefix = KeyPrefix::new("kbf/").expect("a prefix");
    let page = reader
        .list(&prefix, None, PageSize::new(10).expect("a size"))
        .await
        .expect("a listing");
    assert_eq!(page.objects.len(), 1);
    reader.delete(&key).await.expect("a delete");
    let gone = writer.get_range(&key, range).await.unwrap_err();
    assert!(matches!(gone, ObjectStoreError::NotFound(_)), "{gone}");
}
