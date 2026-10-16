//! What the conformance checks do not reach: a digest that does not parse, a server half
//! that is gone, a file that changed after the walk hashed it, and blobs too large to
//! gather streamed to the store.

use std::os::fd::OwnedFd;

use kbf_front::{Cache, MetaLog as _, SegmentsConfig};
use kbf_transfer::{FileSource, RecordHasher, Source, Stamp, digest_of, fetch};

use super::*;

/// The plane over a new in-memory cache, its calls run on `server`.
fn plane(server: &tokio::runtime::Runtime) -> LocalDataPlane {
    let _context = server.enter();
    let store = LocalStore::new(Arc::new(Cache::memory()), SegmentsConfig::DEFAULT);
    LocalDataPlane::new(store, server.handle().clone())
}

/// A file that takes any bytes and keeps none.
fn null_file() -> tokio::fs::File {
    let file = std::fs::OpenOptions::new().write(true).open("/dev/null");
    tokio::fs::File::from_std(file.expect("/dev/null"))
}

/// A runtime standing in for the server half.
fn server() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("runtime")
}

/// Catches a digest that is not one (a hash that is not hex) read as an empty blob or
/// as anything but `Missing`, and a FindMissing that holds one answered as anything but
/// every digest missing (what the transfer engine answers when the server refuses such
/// a call). Beside them, in the same test binary as the failures (each test binary has
/// its own copy of the plane), a held blob reads back and an absent one is `Missing`.
#[test]
fn a_digest_that_does_not_parse_is_missing() {
    let server = server();
    let plane = plane(&server);
    server.block_on(async {
        let bad = Digest {
            hash: "not hex".to_owned(),
            size_bytes: 3,
        };
        assert_eq!(plane.get(&bad).await, Err(CasError::Missing(label(&bad))));
        let mut sink = null_file();
        let into = plane.read_to(&bad, &mut sink).await;
        assert_eq!(into, Err(CasError::Missing(label(&bad))));
        let held = plane.put(b"held".to_vec()).await.expect("put");
        assert_eq!(
            plane.missing(&[held.clone(), bad.clone()]).await,
            [held.clone(), bad]
        );
        assert!(plane.missing(std::slice::from_ref(&held)).await.is_empty());
        assert_eq!(plane.get(&held).await, Ok(b"held".to_vec()));
        let absent = digest_of(b"absent");
        assert_eq!(
            plane.get(&absent).await,
            Err(CasError::Missing(label(&absent)))
        );
    });
}

/// Catches a call into a server half whose runtime is gone panicking, hanging, or
/// taken for an answer: every read and write fails UNAVAILABLE (a read is not
/// `Missing`, which callers would answer by uploading), and FindMissing answers every
/// digest missing.
#[test]
fn a_gone_server_half_fails_every_call() {
    let gone = server();
    let plane = plane(&gone);
    drop(gone);
    let caller = server();
    caller.block_on(async {
        let digest = digest_of(b"anything");
        let read = plane.get(&digest).await;
        assert!(matches!(read, Err(CasError::Unavailable(..))), "{read:?}");
        let into = plane.read_to(&digest, &mut null_file()).await;
        assert!(matches!(into, Err(CasError::Unavailable(..))), "{into:?}");
        let put = plane.put(b"anything".to_vec()).await;
        assert!(matches!(put, Err(CasError::Unavailable(..))), "{put:?}");
        assert_eq!(plane.missing(std::slice::from_ref(&digest)).await, [digest]);
        let records = plane
            .put_records(vec![Record::bytes(b"output".to_vec())])
            .await;
        assert!(
            matches!(records, Err(CasError::Unavailable(..))),
            "{records:?}"
        );
    });
}

/// Catches a file stored with other bytes than the walk hashed: one rewritten after its
/// stamp was taken fails the upload with `Read`, and its digest stays missing.
#[test]
fn a_file_changed_after_the_walk_is_not_stored() {
    // Under the test binary's directory: unit tests get no `CARGO_TARGET_TMPDIR`.
    let dir = std::env::current_exe()
        .expect("test binary")
        .parent()
        .expect("deps")
        .join("kbf-all-in-one-unit")
        .join(format!("changed-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("scratch");
    let walked = b"what the walk hashed";
    std::fs::write(dir.join("out"), walked).expect("write");
    let stamp = Stamp::of(&std::fs::File::open(dir.join("out")).expect("open")).expect("stamp");
    let mut hasher = RecordHasher::new();
    hasher.update(walked);
    let (digest, crc32c) = hasher.finish();
    // Longer than before, so the stamp's size differs whatever the clock's grain.
    std::fs::write(dir.join("out"), b"what the action wrote later").expect("rewrite");
    let root = OwnedFd::from(std::fs::File::open(&dir).expect("open the directory"));
    let source = FileSource::new(
        Arc::new(root),
        Arc::from(Vec::new()),
        "out".to_owned(),
        stamp,
    );
    let record = Record {
        digest,
        crc32c,
        source: Source::File(source),
    };
    let server = server();
    let plane = plane(&server);
    server.block_on(async {
        let stored = plane.put_records(vec![record]).await;
        assert!(matches!(stored, Err(CasError::Read(..))), "{stored:?}");
        let asked = [proto_digest(&digest)];
        assert_eq!(plane.missing(&asked).await, asked);
        assert!(fetch(&plane, &asked[0]).await.is_err());
    });
}

/// `len` bytes that differ for each `seed`.
fn patterned(seed: u8, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| seed.wrapping_add((i % 253) as u8))
        .collect()
}

/// `bytes` as chunks of a megabyte.
fn chunked(bytes: &[u8]) -> Chunks {
    let chunks: Vec<std::io::Result<Vec<u8>>> =
        bytes.chunks(1 << 20).map(|c| Ok(c.to_vec())).collect();
    Box::pin(futures::stream::iter(chunks))
}

/// Catches a blob too large to gather refused, or gathered whole (mutants: the refusal
/// restored, the size check dropped): without a write tier, `put_chunks` of a blob one
/// byte over `MAX_UNSTAGED_BLOB_BYTES` is streamed to the store and held after; again,
/// once held, it is not read at all; one whose bytes are not its digest's is `Corrupt`
/// and not held; one whose chunk fails to be read is `Read`; one with more bytes than
/// its digest names, which the store refuses, is `Unavailable`; a digest that does not
/// parse is refused. A small blob is gathered and stored as before.
#[test]
fn put_chunks_streams_a_large_blob_and_stores_a_small_one() {
    let server = server();
    let plane = plane(&server);
    server.block_on(async {
        let size = usize::try_from(MAX_UNSTAGED_BLOB_BYTES + 1).expect("size");
        let bytes = patterned(9, size);
        let large = digest_of(&bytes);
        let stored = plane.put_chunks(large.clone(), chunked(&bytes)).await;
        assert_eq!(stored, Ok(large.clone()));
        assert!(plane.missing(std::slice::from_ref(&large)).await.is_empty());
        assert_eq!(plane.get(&large).await, Ok(bytes.clone()));
        let never: Chunks = Box::pin(futures::stream::once(async {
            panic!("a chunk of a held blob was read")
        }));
        assert_eq!(plane.put_chunks(large.clone(), never).await, Ok(large));

        let other = patterned(10, size);
        let claimed = digest_of(&patterned(11, size));
        let wrong = plane.put_chunks(claimed.clone(), chunked(&other)).await;
        assert!(
            matches!(&wrong, Err(CasError::Corrupt(blob, actual))
                if *blob == label(&claimed) && *actual == label(&digest_of(&other))),
            "{wrong:?}"
        );
        assert_eq!(
            plane.missing(std::slice::from_ref(&claimed)).await,
            [claimed]
        );

        let mut failing: Vec<std::io::Result<Vec<u8>>> = vec![Ok(vec![0; 1 << 20])];
        failing.push(Err(std::io::Error::other("gone")));
        let failing: Chunks = Box::pin(futures::stream::iter(failing));
        let unread = digest_of(&patterned(12, size));
        let read = plane.put_chunks(unread, failing).await;
        assert!(
            matches!(&read, Err(CasError::Read(_, why)) if why.contains("gone")),
            "{read:?}"
        );
        let mut longer = patterned(15, size);
        let claimed = digest_of(&longer);
        longer.extend_from_slice(&[0; 2 << 20]);
        let long = plane.put_chunks(claimed, chunked(&longer)).await;
        assert!(matches!(long, Err(CasError::Unavailable(..))), "{long:?}");
        let not_a_digest = Digest {
            hash: "not hex".to_owned(),
            size_bytes: i64::try_from(size).expect("size"),
        };
        let refused = plane.put_chunks(not_a_digest, chunked(&other)).await;
        assert!(
            matches!(refused, Err(CasError::Unavailable(..))),
            "{refused:?}"
        );

        let small = digest_of(b"small");
        let chunks: Chunks = Box::pin(futures::stream::iter([
            Ok(b"sma".to_vec()),
            Ok(b"ll".to_vec()),
        ]));
        assert_eq!(plane.put_chunks(small.clone(), chunks).await, Ok(small));
        let chunks: Chunks = Box::pin(futures::stream::iter([Ok(b"other".to_vec())]));
        let wrong = plane.put_chunks(digest_of(b"small!"), chunks).await;
        assert!(matches!(wrong, Err(CasError::Corrupt(..))), "{wrong:?}");
        let failing: Chunks = Box::pin(futures::stream::iter([Err(std::io::Error::other("gone"))]));
        let read = plane.put_chunks(digest_of(b"x"), failing).await;
        assert!(matches!(read, Err(CasError::Read(..))), "{read:?}");
    });
}

/// Catches an upload with a large record refused without a write tier, or failed
/// (mutant: FAILED_PRECONDITION at `Begin` taken for a failure): `put_records` of a
/// record one byte over `MAX_UNSTAGED_BLOB_BYTES` and a small one, with no tier, stores
/// both, each by itself.
#[test]
fn without_a_tier_an_upload_with_a_large_record_is_stored_record_by_record() {
    let server = server();
    let plane = plane(&server);
    let size = usize::try_from(MAX_UNSTAGED_BLOB_BYTES + 1).expect("size");
    let large = patterned(13, size);
    let record = |bytes: &[u8]| {
        let mut hasher = RecordHasher::new();
        hasher.update(bytes);
        let (digest, crc32c) = hasher.finish();
        kbf_transfer::Record {
            digest,
            crc32c,
            source: Source::Bytes(bytes::Bytes::copy_from_slice(bytes)),
        }
    };
    let records = vec![record(&large), record(b"small")];
    let digests: Vec<Digest> = records.iter().map(|r| proto_digest(&r.digest)).collect();
    server.block_on(async {
        plane.put_records(records).await.expect("stored");
        assert!(plane.missing(&digests).await.is_empty());
        assert_eq!(plane.get(&digests[0]).await, Ok(large));
    });
}

/// Catches a streamed blob reported stored when the server half is gone: `put_chunks`
/// of a large blob after the server half's runtime shut down is `Unavailable`.
#[test]
fn put_chunks_of_a_large_blob_without_a_server_half_is_unavailable() {
    let server = server();
    let plane = plane(&server);
    let size = usize::try_from(MAX_UNSTAGED_BLOB_BYTES + 1).expect("size");
    let bytes = patterned(14, size);
    let digest = digest_of(&bytes);
    drop(server);
    let here = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    let gone = here.block_on(plane.put_chunks(digest, chunked(&bytes)));
    assert!(
        matches!(&gone, Err(CasError::Unavailable(_, why)) if why.contains("did not answer")),
        "{gone:?}"
    );
}

/// Catches a `read_to` that writes other bytes than the blob's, takes a blob whose
/// bytes fail their digest (past its first window) for read, answers a file it cannot
/// write as anything but `Write`, or a blob not held as anything but `Missing`. A held
/// blob reads into its file; one of 9 MiB with its last byte flipped in the store is
/// `Unavailable`; a file opened read-only is `Write`; an absent blob is `Missing`.
#[test]
fn read_to_writes_the_blob_or_says_why_not() {
    use kbf_meta::BlobAnswer;
    use kbf_objstore::{ByteRange, ObjectStore};
    let server = server();
    let cache = {
        let _context = server.enter();
        Arc::new(Cache::memory())
    };
    let store = {
        let _context = server.enter();
        LocalStore::new(Arc::clone(&cache), SegmentsConfig::DEFAULT)
    };
    let plane = LocalDataPlane::new(store, server.handle().clone());
    let path = std::env::temp_dir().join(format!("kbf-read-to-{}", std::process::id()));
    server.block_on(async {
        let held = plane.put(b"held bytes".to_vec()).await.expect("put");
        let mut file = tokio::fs::File::create(&path).await.expect("create");
        plane.read_to(&held, &mut file).await.expect("read");
        assert_eq!(std::fs::read(&path).expect("read back"), b"held bytes");
        let mut read_only = tokio::fs::File::open(&path).await.expect("open");
        let refused = plane.read_to(&held, &mut read_only).await;
        assert!(matches!(refused, Err(CasError::Write(..))), "{refused:?}");
        let absent = digest_of(b"absent");
        let missing = plane.read_to(&absent, &mut file).await;
        assert_eq!(missing, Err(CasError::Missing(label(&absent))));

        let large = vec![5u8; 9 << 20];
        let digest = plane.put(large.clone()).await.expect("put");
        let parsed = parse(&digest).expect("a digest");
        let answer = cache
            .meta()
            .query(move |s| s.blob(&parsed))
            .await
            .expect("query");
        let BlobAnswer::Present { location, .. } = answer else {
            panic!("{answer:?}");
        };
        let key = cache.object_key(location.object).expect("key");
        let all = ByteRange::new(0, u64::from(u32::MAX)).expect("range");
        let mut stored = cache
            .objects()
            .get_range(&key, all)
            .await
            .expect("get")
            .to_vec();
        let last = usize::try_from(location.offset).expect("offset") + large.len() - 1;
        stored[last] ^= 1;
        cache.objects().delete(&key).await.expect("delete");
        let replaced = cache.objects().put_new(&key, stored.into(), None).await;
        replaced.expect("put");
        let damaged = plane.read_to(&digest, &mut file).await;
        assert!(
            matches!(damaged, Err(CasError::Unavailable(..))),
            "{damaged:?}"
        );
    });
    let _ = std::fs::remove_file(&path);
}
