//! Batches through the server's REAPI listener, with its in-flight layer in the path
//! (`kbf_server::inflight`): the limit it advertises round-trips on the real wire, and
//! the bytes the layer counts are all given back.

mod support;

use kbf_front::MAX_BATCH_TOTAL_BYTES;
use kbf_proto::reapi::content_addressable_storage_client::ContentAddressableStorageClient;
use kbf_proto::reapi::{BatchReadBlobsRequest, Digest};
use support::{Blob, Cell};
use tonic::Code;

/// Catches: a listener whose layers or limits stop a batch the cache services take
/// (the layer is applied only in `bind_server_with_policy`, which `tests/` of kbf-front
/// do not run through): four 16 MiB blobs uploaded in one call and read back in one,
/// intact; and a read asking for one byte over the advertised limit refused
/// INVALID_ARGUMENT by the handler, not by the transport.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_large_batch_round_trips_through_the_listener_and_one_over_the_limit_is_refused() {
    let cell = Cell::start().await;
    let blobs: Vec<Blob> = (1u8..=4).map(|i| Blob::new(vec![i; 16 << 20])).collect();
    let refs: Vec<&Blob> = blobs.iter().collect();
    cell.upload(&refs).await;
    let mut cas = ContentAddressableStorageClient::new(cell.channel())
        .max_decoding_message_size(kbf_front::MAX_MESSAGE_BYTES);
    let read = cas
        .batch_read_blobs(BatchReadBlobsRequest {
            digests: blobs.iter().map(|b| b.proto.clone()).collect(),
            ..Default::default()
        })
        .await
        .expect("BatchReadBlobs")
        .into_inner();
    for (blob, answer) in blobs.iter().zip(&read.responses) {
        assert_eq!(answer.status.as_ref().expect("status").code, 0);
        assert!(answer.data == blob.data, "a blob read back unchanged");
    }
    let over = cas
        .batch_read_blobs(BatchReadBlobsRequest {
            digests: vec![Digest {
                hash: blobs[0].proto.hash.clone(),
                size_bytes: MAX_BATCH_TOTAL_BYTES as i64 + 1,
            }],
            ..Default::default()
        })
        .await
        .expect_err("over the limit");
    assert_eq!(over.code(), Code::InvalidArgument);
}
