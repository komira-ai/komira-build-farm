//! ActionCache over gRPC, against the in-process cache. Entries are written the way a
//! daemon writes them, through `Cache::write_action_result`.

mod common;

use common::{Blob, Farm, days};
use kbf_front::CacheError;
use kbf_meta::{ActionWriteError, Role};
use kbf_proto::reapi::{
    ActionResult, Directory, FileNode, GetActionResultRequest, OutputDirectory, OutputFile, Tree,
    UpdateActionResultRequest,
};
use tonic::Code;

fn output_file(path: &str, blob: &Blob) -> OutputFile {
    OutputFile {
        path: path.to_owned(),
        digest: Some(blob.proto.clone()),
        ..Default::default()
    }
}

async fn get(farm: &Farm, action: &Blob) -> Result<ActionResult, Code> {
    farm.ac()
        .get_action_result(GetActionResultRequest {
            action_digest: Some(action.proto.clone()),
            ..Default::default()
        })
        .await
        .map(tonic::Response::into_inner)
        .map_err(|s| s.code())
}

/// Catches: a client `UpdateActionResult` that is accepted, which would let any client
/// put a result of its choosing in front of everyone (RFC 16.2); and a refused
/// daemon-path write by a client that still stores its result blob.
#[tokio::test]
async fn client_update_action_result_is_refused() {
    let farm = Farm::start().await;
    let output = Blob::new("an output the client claims");
    farm.upload(&[&output]).await;
    let action = Blob::new("an action");
    let result = ActionResult {
        output_files: vec![output_file("out", &output)],
        exit_code: 0,
        ..Default::default()
    };

    let refused = farm
        .ac()
        .update_action_result(UpdateActionResultRequest {
            action_digest: Some(action.proto.clone()),
            action_result: Some(result.clone()),
            ..Default::default()
        })
        .await
        .expect_err("a client may not write the action cache");
    assert_eq!(refused.code(), Code::PermissionDenied);
    assert_eq!(get(&farm, &action).await, Err(Code::NotFound));

    let as_client = farm
        .cache
        .write_action_result(Role::Client, action.digest, &result)
        .await;
    assert!(
        matches!(
            as_client,
            Err(CacheError::ActionWrite(ActionWriteError::NotDaemon))
        ),
        "{as_client:?}"
    );
    let result_blob = Blob::of(&result);
    assert_eq!(
        farm.find_missing(&[&result_blob]).await,
        [result_blob.digest],
        "a refused write stores nothing"
    );

    // Control: the same entry, written by a daemon, is served.
    farm.cache
        .write_action_result(Role::Daemon, action.digest, &result)
        .await
        .expect("daemon write");
    assert_eq!(get(&farm, &action).await, Ok(result));
}

/// Catches: a hit served while an output file has been collected (the closure leaves
/// output files out, or the lookup checks only the result blob). The result blob and
/// stdout are kept fresh, so only the output file's absence can turn the hit into a miss.
#[tokio::test]
async fn get_action_result_misses_when_an_output_blob_is_missing() {
    let farm = Farm::start().await;
    let output = Blob::new("output bytes");
    let stdout = Blob::new("stdout bytes");
    farm.upload(&[&output, &stdout]).await;
    let action = Blob::new("compile main.c");
    let result = ActionResult {
        output_files: vec![output_file("main.o", &output)],
        stdout_digest: Some(stdout.proto.clone()),
        ..Default::default()
    };
    farm.cache
        .write_action_result(Role::Daemon, action.digest, &result)
        .await
        .expect("daemon write");
    assert_eq!(get(&farm, &action).await, Ok(result.clone()));

    // Keep the result blob and stdout fresh, then let the output file expire.
    let result_blob = Blob::of(&result);
    farm.cache.tick(days(5)).await.expect("tick");
    assert!(farm.find_missing(&[&result_blob, &stdout]).await.is_empty());
    farm.cache.tick(days(9)).await.expect("tick");
    farm.cache.collect().await.expect("collect");
    assert!(farm.find_missing(&[&result_blob, &stdout]).await.is_empty());
    assert_eq!(
        farm.find_missing(&[&output]).await,
        [output.digest],
        "the output was collected"
    );

    assert_eq!(get(&farm, &action).await, Err(Code::NotFound));
}

/// Catches: a hit served while stdout has been collected (the closure leaves stdout and
/// stderr out, so a client gets a result whose stdout it cannot fetch). The mirror of
/// the case above: the result blob and the output file are kept fresh, so only
/// stdout's absence can turn the hit into a miss.
#[tokio::test]
async fn get_action_result_misses_when_stdout_is_missing() {
    let farm = Farm::start().await;
    let output = Blob::new("output bytes");
    let stdout = Blob::new("stdout bytes");
    farm.upload(&[&output, &stdout]).await;
    let action = Blob::new("compile util.c");
    let result = ActionResult {
        output_files: vec![output_file("util.o", &output)],
        stdout_digest: Some(stdout.proto.clone()),
        ..Default::default()
    };
    farm.cache
        .write_action_result(Role::Daemon, action.digest, &result)
        .await
        .expect("daemon write");
    assert_eq!(get(&farm, &action).await, Ok(result.clone()));

    // Keep the result blob and the output fresh, then let stdout expire.
    let result_blob = Blob::of(&result);
    farm.cache.tick(days(5)).await.expect("tick");
    assert!(farm.find_missing(&[&result_blob, &output]).await.is_empty());
    farm.cache.tick(days(9)).await.expect("tick");
    farm.cache.collect().await.expect("collect");
    assert!(farm.find_missing(&[&result_blob, &output]).await.is_empty());
    assert_eq!(
        farm.find_missing(&[&stdout]).await,
        [stdout.digest],
        "stdout was collected"
    );

    assert_eq!(get(&farm, &action).await, Err(Code::NotFound));
}

/// Catches: a hit served while a file inside an output directory's tree is
/// unreachable (the closure includes the tree blob but not the files it names), and
/// a miss that stays a miss after the file is uploaded again.
#[tokio::test]
async fn get_action_result_misses_when_a_tree_file_is_unreachable() {
    let farm = Farm::start().await;
    let inner = Blob::new("a file inside the output directory");
    farm.upload(&[&inner]).await;
    let tree = Blob::of(&Tree {
        root: Some(Directory {
            files: vec![FileNode {
                name: "lib.so".to_owned(),
                digest: Some(inner.proto.clone()),
                ..Default::default()
            }],
            ..Default::default()
        }),
        children: Vec::new(),
    });
    farm.upload(&[&tree]).await;
    let action = Blob::new("link lib");
    let result = ActionResult {
        output_directories: vec![OutputDirectory {
            path: "out".to_owned(),
            tree_digest: Some(tree.proto.clone()),
            ..Default::default()
        }],
        ..Default::default()
    };
    farm.cache
        .write_action_result(Role::Daemon, action.digest, &result)
        .await
        .expect("daemon write");
    assert_eq!(get(&farm, &action).await, Ok(result.clone()));

    farm.delete_object_of(&inner).await;
    let read = farm.cache.read_blob(&inner.digest).await;
    assert!(
        matches!(read, Err(CacheError::Unreachable(_))),
        "a read finds the object gone: {read:?}"
    );
    assert_eq!(get(&farm, &action).await, Err(Code::NotFound));

    farm.upload(&[&inner]).await;
    assert_eq!(get(&farm, &action).await, Ok(result), "healed by re-upload");
}

/// Catches: a result prepared (its blob stored, ready to be accepted) while an output
/// it names is absent or unreachable, which would let the server accept a result whose
/// files nobody can fetch; and a prepared record committed for a client.
#[tokio::test]
async fn prepare_refuses_outputs_that_are_not_held() {
    let farm = Farm::start().await;
    let absent = Blob::new("an output never uploaded");
    let result = ActionResult {
        output_files: vec![output_file("out", &absent)],
        ..Default::default()
    };
    let refused = farm.cache.prepare_action_result(&result).await;
    assert!(
        matches!(refused, Err(CacheError::ActionWrite(ActionWriteError::Absent(d))) if d == absent.digest),
        "{refused:?}"
    );
    let result_blob = Blob::of(&result);
    assert_eq!(
        farm.find_missing(&[&result_blob]).await,
        [result_blob.digest],
        "a refused result stored"
    );

    let gone = Blob::new("an output whose object is lost");
    farm.upload(&[&gone]).await;
    farm.delete_object_of(&gone).await;
    assert!(farm.cache.read_blob(&gone.digest).await.is_err());
    let result = ActionResult {
        output_files: vec![output_file("out", &gone)],
        ..Default::default()
    };
    let refused = farm.cache.prepare_action_result(&result).await;
    assert!(
        matches!(refused, Err(CacheError::ActionWrite(ActionWriteError::Unreachable(d))) if d == gone.digest),
        "{refused:?}"
    );

    let fine = Blob::new("an output that is held");
    farm.upload(&[&fine]).await;
    let result = ActionResult {
        output_files: vec![output_file("out", &fine)],
        ..Default::default()
    };
    let record = farm
        .cache
        .prepare_action_result(&result)
        .await
        .expect("prepare");
    assert_eq!(record.result, Blob::of(&result).digest);
    let action = Blob::new("prepared");
    let as_client = farm
        .cache
        .commit_action_record(Role::Client, action.digest, record.clone())
        .await;
    assert!(
        matches!(
            as_client,
            Err(CacheError::ActionWrite(ActionWriteError::NotDaemon))
        ),
        "{as_client:?}"
    );
    assert_eq!(get(&farm, &action).await, Err(Code::NotFound));
    farm.cache
        .commit_action_record(Role::Daemon, action.digest, record)
        .await
        .expect("daemon commit");
    assert_eq!(get(&farm, &action).await, Ok(result));
}
