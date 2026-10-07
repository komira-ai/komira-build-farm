//! Capabilities and ContentAddressableStorage over gRPC, against the in-process cache.

mod common;

use common::{Blob, Farm, days};
use kbf_front::MAX_BATCH_TOTAL_BYTES;
use kbf_meta::Command;
use kbf_proto::reapi::{
    self, BatchReadBlobsRequest, BatchUpdateBlobsRequest, Directory, DirectoryNode, FileNode,
    FindMissingBlobsRequest, GetCapabilitiesRequest, GetTreeRequest,
};
use tonic::Code;

/// Catches: capabilities that let a client try to write the action cache
/// (`update_enabled`), name a digest function other than SHA-256, advertise a
/// compressor the reads and writes do not decode, or leave the batch limit at 0
/// ("unlimited") while the server refuses batches over 4 MiB.
#[tokio::test]
async fn capabilities_advertise_the_cache_contract() {
    let farm = Farm::start().await;
    let caps = farm
        .caps()
        .get_capabilities(GetCapabilitiesRequest::default())
        .await
        .expect("GetCapabilities")
        .into_inner();
    let cache = caps.cache_capabilities.expect("cache capabilities");
    assert_eq!(
        cache.digest_functions,
        [reapi::digest_function::Value::Sha256 as i32]
    );
    assert!(
        !cache
            .action_cache_update_capabilities
            .expect("update capabilities")
            .update_enabled
    );
    assert_eq!(
        cache.max_batch_total_size_bytes,
        MAX_BATCH_TOTAL_BYTES as i64
    );
    let priorities = cache.cache_priority_capabilities.expect("priorities");
    assert_eq!(priorities.priorities.len(), 1);
    assert!(cache.supported_compressors.is_empty());
    assert!(cache.supported_batch_update_compressors.is_empty());
    assert!(!cache.split_blob_support && !cache.splice_blob_support);
    assert!(caps.execution_capabilities.is_none());
    let high = caps.high_api_version.expect("high version");
    assert_eq!((high.major, high.minor), (2, 3));
}

/// Catches: FindMissingBlobs dropping a digest from its answer, which buck2 and Bazel
/// read as "present" and so never upload. Three ways it can happen:
/// - a digest that does not parse is skipped instead of failing the call;
/// - a blob the index holds at an unreachable object is reported present;
/// - (control) an absent digest asked twice is answered once.
#[tokio::test]
async fn find_missing_blobs_never_omits_a_requested_digest() {
    let farm = Farm::start().await;
    let present = Blob::new("present");
    let unreachable = Blob::new("held, but its object is gone");
    let absent = Blob::new("never uploaded");
    let empty = Blob::new("");
    farm.upload(&[&present]).await;
    farm.upload(&[&unreachable]).await;
    farm.delete_object_of(&unreachable).await;
    let read = farm.cache.read_blob(&unreachable.digest).await;
    assert!(
        read.is_err(),
        "a read of the deleted object fails: {read:?}"
    );

    let missing = farm
        .find_missing(&[&present, &absent, &unreachable, &empty, &absent])
        .await;
    assert_eq!(missing, [absent.digest, unreachable.digest, absent.digest]);

    let mut malformed = absent.proto.clone();
    malformed.hash = malformed.hash.to_uppercase();
    let refused = farm
        .cas()
        .find_missing_blobs(FindMissingBlobsRequest {
            blob_digests: vec![present.proto.clone(), malformed],
            ..Default::default()
        })
        .await
        .expect_err("a malformed digest fails the call");
    assert_eq!(refused.code(), Code::InvalidArgument);
}

/// Catches: answering FindMissingBlobs from a read whose touch lost the race with a
/// collection. The blob was reported present but is gone, so the client would never
/// upload it; the answer must come from a read whose touch committed.
#[tokio::test]
async fn find_missing_asks_again_when_a_collection_wins_the_touch_race() {
    let farm = Farm::start().await;
    let blob = Blob::new("collected under the reader");
    farm.upload(&[&blob]).await;
    // Old enough that a report must touch it, young enough to be held.
    farm.cache.tick(days(2)).await.expect("tick");
    assert!(farm.find_missing(&[&blob]).await.is_empty());

    farm.cache.tick(days(4)).await.expect("tick");
    farm.cache
        .meta()
        .before_next_touch(vec![Command::Tick(days(30)), Command::Collect]);
    assert_eq!(farm.find_missing(&[&blob]).await, [blob.digest]);
}

/// Catches: BatchUpdateBlobs storing bytes under a digest they do not hash to (which
/// would serve wrong bytes for that digest forever after), or failing the whole batch
/// for one bad blob; and BatchReadBlobs not answering each digest on its own, or
/// accepting a batch over the advertised limit.
#[tokio::test]
async fn batch_update_verifies_each_blob_and_batch_read_answers_each() {
    let farm = Farm::start().await;
    let good = Blob::new("good");
    let lying = Blob {
        data: b"not what the digest says".to_vec(),
        ..Blob::new("what the digest says....")
    };
    let other = Blob::new("also good");
    let mut malformed = other.proto.clone();
    malformed.size_bytes = -1;
    let request =
        |digest: &reapi::Digest, data: &[u8]| reapi::batch_update_blobs_request::Request {
            digest: Some(digest.clone()),
            data: data.to_vec(),
            ..Default::default()
        };
    let response = farm
        .cas()
        .batch_update_blobs(BatchUpdateBlobsRequest {
            requests: vec![
                request(&good.proto, &good.data),
                request(&lying.proto, &lying.data),
                request(&other.proto, &other.data),
                request(&malformed, &other.data),
            ],
            ..Default::default()
        })
        .await
        .expect("BatchUpdateBlobs")
        .into_inner();
    let codes: Vec<i32> = response
        .responses
        .iter()
        .map(|r| r.status.as_ref().expect("status").code)
        .collect();
    let invalid = Code::InvalidArgument as i32;
    assert_eq!(codes, [0, invalid, 0, invalid]);
    assert_eq!(
        farm.find_missing(&[&good, &lying, &other]).await,
        [lying.digest]
    );

    let read = farm
        .cas()
        .batch_read_blobs(BatchReadBlobsRequest {
            digests: vec![good.proto.clone(), lying.proto.clone(), other.proto.clone()],
            ..Default::default()
        })
        .await
        .expect("BatchReadBlobs")
        .into_inner();
    let answers: Vec<(i32, &[u8])> = read
        .responses
        .iter()
        .map(|r| (r.status.as_ref().expect("status").code, r.data.as_slice()))
        .collect();
    assert_eq!(
        answers,
        [
            (0, good.data.as_slice()),
            (Code::NotFound as i32, &[][..]),
            (0, other.data.as_slice())
        ]
    );

    let mut huge = good.proto.clone();
    huge.size_bytes = MAX_BATCH_TOTAL_BYTES as i64 + 1;
    let refused = farm
        .cas()
        .batch_read_blobs(BatchReadBlobsRequest {
            digests: vec![huge],
            ..Default::default()
        })
        .await
        .expect_err("over the limit");
    assert_eq!(refused.code(), Code::InvalidArgument);
}

fn dir_node(name: &str, blob: &Blob) -> DirectoryNode {
    DirectoryNode {
        name: name.to_owned(),
        digest: Some(blob.proto.clone()),
    }
}

/// Catches: GetTree stopping below the first level, sending a directory twice, ignoring
/// the page size or page token, or failing (instead of leaving out) a missing child;
/// and answering anything but NOT_FOUND for a missing root.
#[tokio::test]
async fn get_tree_returns_every_present_directory_in_pages() {
    let farm = Farm::start().await;
    let file = Blob::new("a file");
    let leaf = Blob::of(&Directory {
        files: vec![FileNode {
            name: "f".to_owned(),
            digest: Some(file.proto.clone()),
            ..Default::default()
        }],
        ..Default::default()
    });
    let middle = Blob::of(&Directory {
        directories: vec![dir_node("leaf", &leaf)],
        ..Default::default()
    });
    let lost = Blob::of(&Directory {
        directories: vec![dir_node("never-uploaded-child", &Blob::new("x"))],
        ..Default::default()
    });
    let root = Blob::of(&Directory {
        directories: vec![
            dir_node("lost", &lost),
            dir_node("m1", &middle),
            dir_node("m2", &middle),
        ],
        ..Default::default()
    });
    farm.upload(&[&leaf, &middle, &root]).await;

    let get = |root: &Blob, page_size: i32, page_token: String| GetTreeRequest {
        root_digest: Some(root.proto.clone()),
        page_size,
        page_token,
        ..Default::default()
    };
    let mut stream = farm
        .cas()
        .get_tree(get(&root, 1, String::new()))
        .await
        .expect("GetTree")
        .into_inner();
    let mut pages = Vec::new();
    while let Some(page) = stream.message().await.expect("page") {
        pages.push(page);
    }
    let sent: Vec<Vec<u8>> = pages
        .iter()
        .flat_map(|p| p.directories.iter().map(prost::Message::encode_to_vec))
        .collect();
    assert_eq!(
        sent,
        [root.data.clone(), middle.data.clone(), leaf.data.clone()]
    );
    assert!(pages.iter().all(|p| p.directories.len() == 1));
    let tokens: Vec<&str> = pages.iter().map(|p| p.next_page_token.as_str()).collect();
    assert_eq!(tokens, ["1", "2", ""]);

    let mut resumed = farm
        .cas()
        .get_tree(get(&root, 0, "1".to_owned()))
        .await
        .expect("GetTree from a token")
        .into_inner();
    let page = resumed.message().await.expect("page").expect("one page");
    assert_eq!(page.directories.len(), 2);
    assert!(page.next_page_token.is_empty());

    let missing = farm
        .cas()
        .get_tree(get(&lost, 0, String::new()))
        .await
        .expect_err("missing root");
    assert_eq!(missing.code(), Code::NotFound);
}
