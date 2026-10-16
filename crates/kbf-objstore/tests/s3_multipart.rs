//! The S3 store's streamed writes against the loopback S3 service of `s3::fake`: a large
//! object goes as a multipart upload of bounded parts, and every failure aborts the
//! upload, so the service is left with no incomplete one.

use std::time::Duration;

use bytes::Bytes;
use futures::StreamExt;
use futures::stream;
use kbf_objstore::s3::fake::{Failure, FakeS3};
use kbf_objstore::s3::{MULTIPART_THRESHOLD, PART_BYTES};
use kbf_objstore::{BodyStream, ByteRange, Capabilities, ObjectKey, ObjectStore, ObjectStoreError};

const CONDITIONAL: Capabilities = Capabilities {
    conditional_put: true,
    object_lock: false,
};

/// Pieces of a size that does not divide the part size.
const PIECE: usize = (1 << 20) + 7;

fn key(name: &str) -> ObjectKey {
    ObjectKey::new(format!("cas/{name}")).unwrap()
}

/// `len` bytes that differ from their neighbours, so a part sent twice, dropped or out
/// of order reads back wrong.
fn pattern(len: usize) -> Bytes {
    (0..len)
        .map(|i| (i % 251) as u8 ^ (i >> 16) as u8)
        .collect::<Vec<u8>>()
        .into()
}

/// `bytes` in [`PIECE`]-byte pieces, then `tail` if given.
fn body(bytes: &Bytes, tail: Option<ObjectStoreError>) -> BodyStream {
    let mut pieces: Vec<Result<Bytes, ObjectStoreError>> = (0..bytes.len())
        .step_by(PIECE)
        .map(|at| Ok(bytes.slice(at..(at + PIECE).min(bytes.len()))))
        .collect();
    pieces.extend(tail.map(Err));
    stream::iter(pieces).boxed()
}

/// Catches: a large object sent as one PUT (S3 refuses one over 5 GiB, and the whole
/// object is held to send it), parts larger than the part size (memory not bounded by a
/// part), parts cut where the body's pieces end, and parts completed out of order or
/// with bytes lost. Two parts and a short third, read back whole.
#[tokio::test]
async fn a_large_object_goes_in_parts_of_the_part_size_and_reads_back() {
    let s3 = FakeS3::start().await;
    let store = s3.store(CONDITIONAL);
    let bytes = pattern(2 * PART_BYTES as usize + 1000);
    let k = key("large");
    store
        .put_stream(&k, bytes.len() as u64, body(&bytes, None))
        .await
        .expect("streamed");
    let stats = s3.stats();
    assert_eq!(
        (stats.single_puts, stats.parts, stats.completed),
        (0, 3, 1),
        "{stats:?}"
    );
    assert!(
        stats.largest_body <= PART_BYTES,
        "a request carried {} bytes",
        stats.largest_body
    );
    assert_eq!(s3.incomplete_uploads(), 0);
    let all = ByteRange::new(0, bytes.len() as u64).unwrap();
    assert_eq!(store.get_range(&k, all).await.unwrap(), bytes);
    let requests = s3.requests();
    let order: Vec<&str> = requests
        .iter()
        .map(|r| r.split('?').next().unwrap_or_default())
        .collect();
    assert_eq!(
        order,
        [
            "POST /kbf-fake/cas/large",
            "PUT /kbf-fake/cas/large",
            "PUT /kbf-fake/cas/large",
            "PUT /kbf-fake/cas/large",
            "POST /kbf-fake/cas/large",
            "GET /kbf-fake/cas/large",
        ]
    );
    assert!(requests[1].contains("partNumber=1"), "{requests:?}");
    assert!(requests[3].contains("partNumber=3"), "{requests:?}");
}

/// Catches: the threshold off by one either way: an object of exactly
/// [`MULTIPART_THRESHOLD`] bytes is one PUT, one byte more is a multipart upload.
#[tokio::test]
async fn the_threshold_is_the_largest_single_put() {
    let s3 = FakeS3::start().await;
    let store = s3.store(CONDITIONAL);
    let at = pattern(MULTIPART_THRESHOLD as usize);
    store
        .put_stream(&key("at"), at.len() as u64, body(&at, None))
        .await
        .unwrap();
    assert_eq!((s3.stats().single_puts, s3.stats().parts), (1, 0));
    let over = pattern(MULTIPART_THRESHOLD as usize + 1);
    store
        .put_stream(&key("over"), over.len() as u64, body(&over, None))
        .await
        .unwrap();
    assert_eq!((s3.stats().single_puts, s3.stats().parts), (1, 2));
    assert_eq!(s3.object_len(&key("over")), Some(over.len() as u64));
}

/// Catches: a failed part that leaves its upload behind (an incomplete upload holds
/// storage until a lifecycle rule removes it), or a write answered as stored.
#[tokio::test]
async fn a_failed_part_aborts_the_upload() {
    let s3 = FakeS3::start().await;
    let store = s3.store(CONDITIONAL);
    s3.fail(Failure::Part(2));
    let bytes = pattern(3 * PART_BYTES as usize);
    let k = key("part-fails");
    let wrote = store
        .put_stream(&k, bytes.len() as u64, body(&bytes, None))
        .await;
    assert!(
        matches!(wrote, Err(ObjectStoreError::Service { status: 500, .. })),
        "{wrote:?}"
    );
    assert_eq!(s3.incomplete_uploads(), 0, "the upload was left behind");
    assert_eq!(s3.stats().aborted, 1);
    assert_eq!(s3.object_len(&k), None);
}

/// Catches: a body that failed, or ended short or long, completed as if whole, or its
/// upload left behind.
#[tokio::test]
async fn a_broken_body_aborts_the_upload() {
    let s3 = FakeS3::start().await;
    let store = s3.store(CONDITIONAL);
    let bytes = pattern(2 * PART_BYTES as usize + 5);
    let len = bytes.len() as u64;
    let broke = ObjectStoreError::Body("the source broke".into());
    let cases = [
        ("failed", len, body(&bytes, Some(broke))),
        ("short", len + 1, body(&bytes, None)),
        ("long", len - 1, body(&bytes, None)),
    ];
    for (name, declared, piece) in cases {
        let k = key(name);
        let wrote = store.put_stream(&k, declared, piece).await;
        assert!(
            matches!(wrote, Err(ObjectStoreError::Body(_))),
            "{name}: {wrote:?}"
        );
        assert_eq!(s3.incomplete_uploads(), 0, "{name}: left behind");
        assert_eq!(s3.object_len(&k), None, "{name}");
    }
    assert_eq!(s3.stats().completed, 0);
    assert_eq!(s3.stats().aborted, 3);
}

/// Catches: a completion answered 200 with an `<Error>` body taken for success (an
/// object reported written that is not there), and its upload left behind.
#[tokio::test]
async fn a_completion_that_fails_late_aborts_the_upload() {
    let s3 = FakeS3::start().await;
    let store = s3.store(CONDITIONAL);
    s3.fail(Failure::CompleteLate);
    let bytes = pattern(PART_BYTES as usize + 1);
    let k = key("late");
    let wrote = store
        .put_stream(&k, bytes.len() as u64, body(&bytes, None))
        .await;
    assert!(
        matches!(&wrote, Err(ObjectStoreError::Service { status: 200, code, .. }) if code == "InternalError"),
        "{wrote:?}"
    );
    assert_eq!(s3.incomplete_uploads(), 0);
    assert_eq!(s3.object_len(&k), None);
}

/// Catches: a multipart write that overwrites an existing key on a store claiming
/// conditional writes (the flusher relies on `AlreadyExists` to find a conflict), or
/// that leaves its refused upload behind.
#[tokio::test]
async fn a_conditional_completion_over_an_existing_key_is_refused_and_aborted() {
    let s3 = FakeS3::start().await;
    let store = s3.store(CONDITIONAL);
    let k = key("taken");
    store
        .put_new(&k, Bytes::from_static(b"first"), None)
        .await
        .unwrap();
    let bytes = pattern(PART_BYTES as usize + 1);
    let wrote = store
        .put_stream(&k, bytes.len() as u64, body(&bytes, None))
        .await;
    assert!(
        matches!(wrote, Err(ObjectStoreError::AlreadyExists(_))),
        "{wrote:?}"
    );
    assert_eq!(s3.incomplete_uploads(), 0);
    assert_eq!(s3.object_len(&k), Some(5));
    let condition = s3.requests();
    assert!(
        condition
            .iter()
            .any(|r| r.starts_with("POST /kbf-fake/cas/taken?uploadId=")),
        "{condition:?}"
    );
}

/// Catches: a write dropped mid-upload (its caller went away: a client's stream cut, a
/// flush cancelled at shutdown) that leaves its upload behind.
#[tokio::test]
async fn a_write_dropped_mid_upload_aborts_it() {
    let s3 = FakeS3::start().await;
    let store = s3.store(CONDITIONAL);
    let first = pattern(PART_BYTES as usize + 3);
    // One part's worth and more, then a body that never ends.
    let pending = stream::iter(vec![Ok(first)]).chain(stream::pending());
    let k = key("dropped");
    let writing = tokio::spawn(async move {
        let _ = store.put_stream(&k, 4 * PART_BYTES, pending.boxed()).await;
    });
    // However slow the machine: the first part reaches the service, then the write
    // waits for a body that never comes, and is dropped.
    for _ in 0..1000 {
        if s3.stats().parts == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(s3.stats().parts, 1, "the first part was sent");
    assert_eq!(s3.incomplete_uploads(), 1, "the upload is open");
    writing.abort();
    assert!(writing.await.is_err_and(|e| e.is_cancelled()));
    for _ in 0..500 {
        if s3.incomplete_uploads() == 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the dropped write's upload was not aborted");
}

/// Catches a failure of any other step taken for a stored object, or a failure that
/// leaves its upload behind: a refused create (nothing to abort), a refused completion
/// and a part answered without an ETag each fail the write, store nothing, and leave no
/// incomplete upload.
#[tokio::test]
async fn every_failed_step_fails_the_write_and_leaves_no_upload() {
    let s3 = FakeS3::start().await;
    let store = s3.store(CONDITIONAL);
    let bytes = pattern(PART_BYTES as usize + 1);
    for (failure, name) in [
        (Failure::Create, "create"),
        (Failure::Complete, "complete"),
        (Failure::NoEtag, "no-etag"),
    ] {
        s3.fail(failure);
        let k = key(name);
        let wrote = store
            .put_stream(&k, bytes.len() as u64, body(&bytes, None))
            .await;
        let expected = match failure {
            Failure::NoEtag => {
                matches!(&wrote, Err(ObjectStoreError::Protocol(m)) if m.contains("ETag"))
            }
            _ => matches!(&wrote, Err(ObjectStoreError::Service { status: 500, .. })),
        };
        assert!(expected, "{name}: {wrote:?}");
        assert_eq!(s3.incomplete_uploads(), 0, "{name}: left behind");
        assert_eq!(s3.object_len(&k), None, "{name}");
    }
    assert_eq!(s3.stats().aborted, 2, "create had nothing to abort");
}

/// Catches an abort's own failure taking the place of the write's error, or hiding
/// it: the write fails with its part's error, and the upload the store would not
/// abort is left to the bucket's lifecycle rule.
#[tokio::test]
async fn an_abort_that_fails_leaves_the_write_failed_with_its_own_error() {
    let s3 = FakeS3::start().await;
    let store = s3.store(CONDITIONAL);
    s3.fail(Failure::Part(1));
    s3.fail(Failure::Abort);
    let bytes = pattern(PART_BYTES as usize + 1);
    let wrote = store
        .put_stream(&key("abort-fails"), bytes.len() as u64, body(&bytes, None))
        .await;
    assert!(
        matches!(wrote, Err(ObjectStoreError::Service { status: 500, .. })),
        "{wrote:?}"
    );
    assert_eq!(s3.incomplete_uploads(), 1);
}

/// Catches the S3 store reading a missing object or a range past an object's end as
/// anything but `NotFound` and `InvalidRange` (the cache marks an object missing on the
/// first), and a conditional single PUT over an existing key not refused.
#[tokio::test]
async fn missing_objects_ranges_past_the_end_and_taken_keys_are_told_apart() {
    let s3 = FakeS3::start().await;
    let store = s3.store(CONDITIONAL);
    let k = key("small");
    let one = ByteRange::new(0, 1).unwrap();
    let missing = store.get_range(&k, one).await;
    assert!(
        matches!(missing, Err(ObjectStoreError::NotFound(_))),
        "{missing:?}"
    );
    store
        .put_new(&k, Bytes::from_static(b"abc"), None)
        .await
        .unwrap();
    let past = store.get_range(&k, ByteRange::new(3, 1).unwrap()).await;
    assert!(
        matches!(past, Err(ObjectStoreError::InvalidRange { .. })),
        "{past:?}"
    );
    let again = store.put_new(&k, Bytes::from_static(b"xyz"), None).await;
    assert!(
        matches!(again, Err(ObjectStoreError::AlreadyExists(_))),
        "{again:?}"
    );
    assert_eq!(
        store.get_range(&k, one).await.unwrap(),
        Bytes::from_static(b"a")
    );
}

/// Catches a fake S3 service that completes what S3 would refuse: an upload it never
/// began, a completion that does not parse, one naming a part it does not hold, and one
/// whose first part is under 5 MiB; and one that answers a request it does not serve.
/// Each is refused with S3's code, and nothing is completed.
#[tokio::test]
async fn the_fake_refuses_what_s3_refuses() {
    let s3 = FakeS3::start().await;
    let client = reqwest::Client::new();
    let url = |path: &str| format!("{}/kbf-fake/cas/k{path}", s3.endpoint());
    let complete = |id: &str, body: String| {
        client
            .post(url(&format!("?uploadId={id}")))
            .body(body)
            .send()
    };
    let refused = |answer: reqwest::Response| async move {
        assert_eq!(answer.status(), 400);
        answer.text().await.unwrap()
    };
    let never = complete("never", String::new()).await.unwrap();
    assert!(refused(never).await.contains("NoSuchUpload"));
    let created = client.post(url("?uploads")).send().await.unwrap();
    assert!(
        created
            .text()
            .await
            .unwrap()
            .contains("<UploadId>upload-1</UploadId>")
    );
    let mut etags = Vec::new();
    for n in 1..=2 {
        let part = client
            .put(url(&format!("?partNumber={n}&uploadId=upload-1")))
            .body("small")
            .send()
            .await
            .unwrap();
        etags.push(part.headers()["etag"].to_str().unwrap().to_owned());
    }
    let named = |parts: &[(u32, &str)]| {
        let parts: String = parts
            .iter()
            .map(|(n, e)| format!("<Part><PartNumber>{n}</PartNumber><ETag>{e}</ETag></Part>"))
            .collect();
        format!("<CompleteMultipartUpload>{parts}</CompleteMultipartUpload>")
    };
    let cases = [
        ("not xml".to_owned(), "MalformedXML"),
        (named(&[(9, &etags[0])]), "InvalidPart"),
        (named(&[(1, "\"other\"")]), "InvalidPart"),
        (named(&[(1, &etags[0]), (2, &etags[1])]), "EntityTooSmall"),
    ];
    for (body, code) in cases {
        let answer = complete("upload-1", body).await.unwrap();
        assert!(refused(answer).await.contains(code), "{code}");
    }
    let other = client.head(url("")).send().await.unwrap();
    assert_eq!(other.status(), 501);
    assert_eq!((s3.stats().completed, s3.incomplete_uploads()), (0, 1));
}

/// Catches a store without conditional writes sent a condition (S3 refuses a key
/// written again only when asked), or refusing an overwrite it should take: on such a
/// store a single PUT and a multipart upload over an existing key both replace it, and
/// an abort of an upload the service never began is refused.
#[tokio::test]
async fn a_store_without_conditional_writes_replaces_a_key() {
    let s3 = FakeS3::start().await;
    let store = s3.store(Capabilities::default());
    let k = key("replaced");
    store
        .put_new(&k, Bytes::from_static(b"one"), None)
        .await
        .unwrap();
    store
        .put_new(&k, Bytes::from_static(b"two!"), None)
        .await
        .unwrap();
    assert_eq!(s3.object_len(&k), Some(4));
    let bytes = pattern(PART_BYTES as usize + 1);
    let len = bytes.len() as u64;
    store.put_stream(&k, len, body(&bytes, None)).await.unwrap();
    assert_eq!(s3.object_len(&k), Some(len));
    let requests = s3.requests();
    assert_eq!(s3.stats().completed, 1, "{requests:?}");
    let unknown = format!("{}/kbf-fake/cas/replaced?uploadId=never", s3.endpoint());
    let refused = reqwest::Client::new().delete(unknown).send().await.unwrap();
    assert_eq!(refused.status(), 500);
}
