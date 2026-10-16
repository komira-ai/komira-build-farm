//! ByteStream over gRPC, against the in-process cache.

mod common;

use std::time::Duration;

use common::{Blob, Farm, pseudo_random};
use futures::channel::mpsc;
use kbf_front::{MAX_UNSTAGED_BLOB_BYTES, READ_CHUNK_BYTES};
use kbf_proto::google::bytestream::{QueryWriteStatusRequest, ReadRequest, WriteRequest};
use tonic::Code;

fn read_name(blob: &Blob) -> String {
    format!(
        "main/blobs/{}/{}",
        blob.digest.hash_hex(),
        blob.digest.size_bytes
    )
}

fn write_name(blob: &Blob) -> String {
    format!(
        "main/uploads/7d5e6f1a-test/blobs/{}/{}",
        blob.digest.hash_hex(),
        blob.digest.size_bytes
    )
}

/// The write requests that send `data` for `name` in `chunk`-byte pieces from offset 0.
fn chunks(name: &str, data: &[u8], chunk: usize) -> Vec<WriteRequest> {
    let pieces: Vec<&[u8]> = data.chunks(chunk).collect();
    let last = pieces.len() - 1;
    pieces
        .iter()
        .enumerate()
        .map(|(i, piece)| WriteRequest {
            resource_name: if i == 0 {
                name.to_owned()
            } else {
                String::new()
            },
            write_offset: (i * chunk) as i64,
            finish_write: i == last,
            data: piece.to_vec(),
        })
        .collect()
}

/// Reads `[offset, offset + limit)` of `blob` and returns the messages' data.
async fn read(farm: &Farm, blob: &Blob, offset: i64, limit: i64) -> Result<Vec<Vec<u8>>, Code> {
    let mut stream = farm
        .bytestream()
        .read(ReadRequest {
            resource_name: read_name(blob),
            read_offset: offset,
            read_limit: limit,
        })
        .await
        .map_err(|s| s.code())?
        .into_inner();
    let mut messages = Vec::new();
    while let Some(m) = stream.message().await.map_err(|s| s.code())? {
        messages.push(m.data);
    }
    Ok(messages)
}

async fn status(farm: &Farm, blob: &Blob) -> (i64, bool) {
    let s = farm
        .bytestream()
        .query_write_status(QueryWriteStatusRequest {
            resource_name: write_name(blob),
        })
        .await
        .expect("QueryWriteStatus")
        .into_inner();
    (s.committed_size, s.complete)
}

/// Catches: an upload that is not then reported present, or reads that ignore
/// `read_offset` or `read_limit`, send messages over the chunk size, or accept an
/// offset past the end (which REAPI makes OUT_OF_RANGE).
#[tokio::test]
async fn bytestream_upload_is_then_present_and_readable_with_ranges() {
    let farm = Farm::start().await;
    let blob = Blob::new(pseudo_random(
        2 * READ_CHUNK_BYTES + READ_CHUNK_BYTES / 2,
        7,
    ));
    let size = blob.data.len();
    assert_eq!(farm.find_missing(&[&blob]).await, [blob.digest]);

    let written = farm
        .bytestream()
        .write(futures::stream::iter(chunks(
            &write_name(&blob),
            &blob.data,
            1 << 20,
        )))
        .await
        .expect("Write")
        .into_inner();
    assert_eq!(written.committed_size, size as i64);
    assert!(farm.find_missing(&[&blob]).await.is_empty());

    let whole = read(&farm, &blob, 0, 0).await.expect("whole read");
    assert_eq!(whole.len(), 3);
    assert!(whole.iter().all(|m| m.len() <= READ_CHUNK_BYTES));
    assert_eq!(whole.concat(), blob.data);

    let window = read(&farm, &blob, 1000, 5000).await.expect("window");
    assert_eq!(window.concat(), blob.data[1000..6000]);
    let across = READ_CHUNK_BYTES as i64 - 10;
    let window = read(&farm, &blob, across, 20).await.expect("window");
    assert_eq!(
        window.concat(),
        blob.data[across as usize..across as usize + 20]
    );
    let tail = read(&farm, &blob, 2 * READ_CHUNK_BYTES as i64, 0)
        .await
        .expect("tail");
    assert_eq!(tail.concat(), blob.data[2 * READ_CHUNK_BYTES..]);
    let past_limit = read(&farm, &blob, size as i64 - 3, 100)
        .await
        .expect("short");
    assert_eq!(past_limit.concat(), blob.data[size - 3..]);
    assert_eq!(
        read(&farm, &blob, size as i64, 0)
            .await
            .expect("at end")
            .concat(),
        b""
    );

    assert_eq!(
        read(&farm, &blob, size as i64 + 1, 0).await,
        Err(Code::OutOfRange)
    );
    assert_eq!(read(&farm, &blob, -1, 0).await, Err(Code::OutOfRange));
    let absent = Blob::new("absent");
    assert_eq!(read(&farm, &absent, 0, 0).await, Err(Code::NotFound));
}

/// Catches: a write that commits bytes not matching the digest it names, or that
/// accepts a `write_offset` other than the bytes received so far (uploads do not
/// resume, so a client sending a suffix must be told, not silently merged).
#[tokio::test]
async fn bytestream_write_refuses_wrong_bytes_and_offsets() {
    let farm = Farm::start().await;
    let blob = Blob::new(pseudo_random(30, 1));
    let name = write_name(&blob);

    let mut wrong = chunks(&name, &blob.data, 30);
    wrong[0].data[0] ^= 1;
    let refused = farm
        .bytestream()
        .write(futures::stream::iter(wrong))
        .await
        .expect_err("wrong bytes");
    assert_eq!(refused.code(), Code::InvalidArgument);

    let mut skewed = chunks(&name, &blob.data, 10);
    skewed[1].write_offset = 15;
    let refused = farm
        .bytestream()
        .write(futures::stream::iter(skewed))
        .await
        .expect_err("skewed offset");
    assert_eq!(refused.code(), Code::InvalidArgument);

    let mut renamed = chunks(&name, &blob.data, 10);
    renamed[1].resource_name = write_name(&Blob::new(pseudo_random(30, 2)));
    let refused = farm
        .bytestream()
        .write(futures::stream::iter(renamed))
        .await
        .expect_err("renamed");
    assert!(
        refused.message().contains("changed resource"),
        "{refused:?}"
    );

    let mut long = chunks(&name, &blob.data, 10);
    long[2].data.push(0);
    let refused = farm
        .bytestream()
        .write(futures::stream::iter(long))
        .await
        .expect_err("too long");
    assert!(
        refused.message().contains("more than the 30 bytes"),
        "{refused:?}"
    );

    let mut unfinished = chunks(&name, &blob.data, 10);
    unfinished[2].finish_write = false;
    let refused = farm
        .bytestream()
        .write(futures::stream::iter(unfinished))
        .await
        .expect_err("no finish_write");
    assert_eq!(refused.code(), Code::InvalidArgument);

    assert_eq!(farm.find_missing(&[&blob]).await, [blob.digest]);
    assert_eq!(status(&farm, &blob).await, (0, false));
}

/// Catches: a write whose resource name claims more than one in-memory blob being
/// admitted for, or buffered to, the size it names (any client could make the front
/// hold as much memory as it claims), and a large blob refused as too large. A write
/// naming 5 GiB + 1 bytes, left open after its first message, holds one streamed
/// write's admission ([`MAX_UNSTAGED_BLOB_BYTES`]) and no receive buffer; when its
/// stream ends without `finish_write` it is refused, nothing is stored, and the
/// admission is returned.
#[tokio::test]
async fn bytestream_write_of_a_huge_blob_is_streamed_not_buffered() {
    let farm = Farm::start().await;
    let size = (5u64 << 30) + 1;
    let name = format!(
        "main/uploads/7d5e6f1a-test/blobs/{}/{size}",
        "ab".repeat(32)
    );
    let (tx, rx) = mpsc::unbounded();
    tx.unbounded_send(WriteRequest {
        resource_name: name,
        write_offset: 0,
        finish_write: false,
        data: vec![0; 1024],
    })
    .expect("send");
    let mut client = farm.bytestream();
    let write = tokio::spawn(async move { client.write(rx).await });
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while farm.cache.bytes_admitted() == 0 {
        assert!(!write.is_finished(), "{:?}", write.await);
        assert!(tokio::time::Instant::now() < deadline, "never admitted");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(farm.cache.bytes_admitted(), MAX_UNSTAGED_BLOB_BYTES);
    assert_eq!(farm.cache.write_buffer_peak(), 0, "nothing buffered whole");
    drop(tx);
    let refused = tokio::time::timeout(Duration::from_secs(10), write)
        .await
        .expect("answered once the stream ended")
        .expect("joined")
        .expect_err("no finish_write");
    assert_eq!(refused.code(), Code::InvalidArgument, "{refused:?}");
    assert!(
        refused.message().contains("without finish_write"),
        "{refused:?}"
    );
    assert_eq!(farm.cache.bytes_admitted(), 0);
}

/// Catches: QueryWriteStatus claiming progress that is not durable: bytes a live
/// stream has buffered, or a blob whose stored object is gone. A client trusting
/// either would skip an upload the cache does not have.
#[tokio::test]
async fn query_write_status_reports_only_durable_progress() {
    let farm = Farm::start().await;
    let blob = Blob::new(pseudo_random(64 << 10, 3));
    let size = blob.data.len() as i64;
    let mut requests = chunks(&write_name(&blob), &blob.data, 16 << 10).into_iter();

    let (tx, rx) = mpsc::unbounded();
    let mut client = farm.bytestream();
    let upload = tokio::spawn(async move { client.write(rx).await });
    for _ in 0..3 {
        tx.unbounded_send(requests.next().expect("chunk"))
            .expect("send");
    }
    assert_eq!(status(&farm, &blob).await, (0, false), "mid-upload");
    for r in requests {
        tx.unbounded_send(r).expect("send");
    }
    let written = upload.await.expect("join").expect("Write").into_inner();
    assert_eq!(written.committed_size, size);
    assert_eq!(status(&farm, &blob).await, (size, true), "after the write");

    farm.delete_object_of(&blob).await;
    assert_eq!(read(&farm, &blob, 0, 0).await, Err(Code::Unavailable));
    assert_eq!(status(&farm, &blob).await, (0, false), "object gone");

    farm.upload(&[&blob]).await;
    assert_eq!(status(&farm, &blob).await, (size, true), "re-uploaded");
    assert_eq!(
        read(&farm, &blob, 0, 0).await.expect("healed").concat(),
        blob.data
    );
}

/// Catches: the empty blob reported missing by QueryWriteStatus. REAPI holds it present
/// everywhere, so no client uploads it (the transfer engine's FindMissing skips it); a
/// client that asks whether its write is durable must hear yes without a write, as
/// FindMissing says.
#[tokio::test]
async fn the_empty_blob_is_durable_without_a_write() {
    let farm = Farm::start().await;
    let empty = Blob::new(Vec::new());
    assert!(farm.find_missing(&[&empty]).await.is_empty());
    assert_eq!(status(&farm, &empty).await, (0, true));
}

/// A blob over [`MAX_UNSTAGED_BLOB_BYTES`], so a Write streams it to the store, and
/// over two read windows: 20 MiB.
fn large_blob(seed: u64) -> Blob {
    Blob::new(pseudo_random(20 << 20, seed))
}

/// Catches a streamed Write that stores other bytes than it was sent (a piece lost or
/// repeated between messages and parts), and a windowed Read that hands over other
/// bytes than asked for at a window's edge, or the bytes of a range other than its
/// own. A 20 MiB blob is written in 1 MiB messages, then read whole and across the
/// first window's end.
#[tokio::test]
async fn a_large_blob_is_streamed_in_and_read_back_in_windows() {
    let farm = Farm::start().await;
    let blob = large_blob(31);
    let messages = chunks(&write_name(&blob), &blob.data, 1 << 20);
    let written = farm
        .bytestream()
        .write(futures::stream::iter(messages))
        .await
        .expect("written")
        .into_inner();
    assert_eq!(written.committed_size, blob.data.len() as i64);
    assert_eq!(farm.find_missing(&[&blob]).await, []);
    let whole = read(&farm, &blob, 0, 0).await.expect("read").concat();
    assert!(whole == blob.data, "the whole blob reads back");
    let window = usize::try_from(kbf_front::READ_WINDOW_BYTES).expect("window");
    let edge = read(&farm, &blob, (window - 5) as i64, 10)
        .await
        .expect("read")
        .concat();
    assert_eq!(edge, blob.data[window - 5..window + 5]);
}

/// Catches a streamed Write stored under a digest its bytes do not have, or answered
/// other than INVALID_ARGUMENT: one byte of a 20 MiB blob changed in flight is
/// refused, and the blob stays missing.
#[tokio::test]
async fn a_large_write_that_fails_its_digest_is_refused_and_not_stored() {
    let farm = Farm::start().await;
    let blob = large_blob(32);
    let mut sent = blob.data.clone();
    sent[17 << 20] ^= 1;
    let messages = chunks(&write_name(&blob), &sent, 1 << 20);
    let refused = farm
        .bytestream()
        .write(futures::stream::iter(messages))
        .await
        .expect_err("a changed byte");
    assert_eq!(refused.code(), Code::InvalidArgument, "{refused:?}");
    assert!(refused.message().contains("hashes to"), "{refused:?}");
    assert_eq!(farm.find_missing(&[&blob]).await, [blob.digest]);
}

/// Catches a streamed Write that takes messages the in-memory path refuses: one that
/// skips ahead, one past the size the resource names, and one that changes the
/// resource. Each is INVALID_ARGUMENT naming why, and nothing is stored.
#[tokio::test]
async fn a_large_write_out_of_order_too_long_or_renamed_is_refused() {
    let farm = Farm::start().await;
    let blob = large_blob(33);
    let name = write_name(&blob);
    let mut skipped = chunks(&name, &blob.data, 1 << 20);
    skipped.remove(1);
    let mut long = chunks(&name, &blob.data, 1 << 20);
    if let Some(last) = long.last_mut() {
        last.data.push(0);
    }
    let mut renamed = chunks(&name, &blob.data, 1 << 20);
    renamed[2].resource_name = write_name(&large_blob(34));
    for (messages, why) in [
        (skipped, "write_offset"),
        (long, "more than the"),
        (renamed, "changed resource"),
    ] {
        let refused = farm
            .bytestream()
            .write(futures::stream::iter(messages))
            .await
            .expect_err(why);
        assert_eq!(refused.code(), Code::InvalidArgument, "{refused:?}");
        assert!(refused.message().contains(why), "{why}: {refused:?}");
    }
    assert_eq!(farm.find_missing(&[&blob]).await, [blob.digest]);
}

/// Catches a windowed Read that sends a damaged blob's last window, or ends without
/// an error, or leaves the object readable: a 20 MiB blob with one bit flipped in the
/// store ends its Read UNAVAILABLE, and FindMissing then reports it missing (marked
/// corrupt), so a client uploads it again.
#[tokio::test]
async fn a_large_blob_damaged_in_the_store_ends_its_read_with_an_error() {
    let farm = Farm::start().await;
    let blob = large_blob(35);
    let messages = chunks(&write_name(&blob), &blob.data, 1 << 20);
    farm.bytestream()
        .write(futures::stream::iter(messages))
        .await
        .expect("written");
    farm.corrupt_object_of(&blob).await;
    assert_eq!(read(&farm, &blob, 0, 0).await, Err(Code::Unavailable));
    assert_eq!(farm.find_missing(&[&blob]).await, [blob.digest]);
}

/// Catches a whole read of a blob too large to decode (the server holding it whole),
/// and a windowed read of a range outside its blob: a blob of 64 MiB + 1, streamed in,
/// is INVALID_ARGUMENT to `read_blob` and reads back whole through ByteStream; bytes
/// 5..3 of it, or past its end, are refused.
#[tokio::test]
async fn a_blob_too_large_to_decode_is_read_only_in_windows() {
    let farm = Farm::start().await;
    let size = kbf_front::MAX_DECODED_BLOB_BYTES as usize + 1;
    let blob = Blob::new(pseudo_random(size, 36));
    let messages = chunks(&write_name(&blob), &blob.data, 4 << 20);
    farm.bytestream()
        .write(futures::stream::iter(messages))
        .await
        .expect("written");
    let whole = farm.cache.read_blob(&blob.digest).await;
    assert!(
        matches!(whole, Err(kbf_front::CacheError::Invalid(_))),
        "{whole:?}"
    );
    let read = read(&farm, &blob, 0, 0).await.expect("read").concat();
    assert!(read == blob.data, "read back in windows");
    for (from, to) in [(5, 3), (0, size as u64 + 1)] {
        let refused = farm.cache.read_windows(&blob.digest, from, to).await;
        assert!(
            matches!(refused, Err(kbf_front::CacheError::Invalid(_))),
            "{from}..{to}"
        );
    }
}
