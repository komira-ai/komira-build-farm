//! ByteStream over gRPC, against the in-process cache.

mod common;

use std::time::Duration;

use common::{Blob, Farm, pseudo_random};
use futures::channel::mpsc;
use kbf_front::{MAX_BLOB_BYTES, READ_CHUNK_BYTES};
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

/// Catches: a write whose resource name claims more than `MAX_BLOB_BYTES` being
/// accepted and buffered, so any client can make the front hold as much memory as it
/// names. The stream is left open after its first message: a front without the cap
/// waits for the rest and never answers; one with it refuses at once.
#[tokio::test]
async fn bytestream_write_refuses_a_blob_over_the_cap_before_buffering() {
    let farm = Farm::start().await;
    let size = MAX_BLOB_BYTES + 1;
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
    let answer = tokio::time::timeout(Duration::from_secs(10), client.write(rx))
        .await
        .expect("refused without waiting for the rest of the blob");
    assert_eq!(
        answer.expect_err("over the cap").code(),
        Code::InvalidArgument
    );
    drop(tx);
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
