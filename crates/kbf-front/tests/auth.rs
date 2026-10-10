//! Authentication and authorization of every REAPI method, over gRPC.
//!
//! The methods are read from the vendored service definitions (`remote_execution.proto`
//! and `bytestream.proto` in kbf-proto), the files the served routes are generated
//! from, and the table below must name each one: a method added to a service (a
//! vendored-proto update) fails these tests until the table says which authorizer
//! covers it. Each test then calls every method through a server built as kbf-server
//! builds its REAPI listener ([`Farm::with_policy`]).

mod common;

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use common::{Blob, Farm};
use futures::future::BoxFuture;
use futures::stream;
use kbf_auth::{
    AllowAuthenticator, AllowAuthorizer, AuthenticationMetadata, Authenticator, Authorizer,
    Authorizers, DenyAuthenticator, DenyAuthorizer, Policy,
};
use kbf_front::{Dispatch, Stage, Submission, Ticket};
use kbf_proto::google::bytestream::{QueryWriteStatusRequest, ReadRequest, WriteRequest};
use kbf_proto::reapi::{
    BatchReadBlobsRequest, BatchUpdateBlobsRequest, ExecuteRequest, FindMissingBlobsRequest,
    GetActionResultRequest, GetCapabilitiesRequest, GetChunkMappingRequest, GetTreeRequest,
    RegisterChunkMappingRequest, SpliceBlobRequest, SplitBlobRequest, UpdateActionResultRequest,
    WaitExecutionRequest,
};
use serde_json::{Value, json};
use tokio::sync::watch;
use tonic::codegen::http::request::Parts;
use tonic::{Code, Status};

/// The authorizer a method asks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Slot {
    Capabilities,
    CasGet,
    CasPut,
    CasFindMissing,
    AcGet,
    Execute,
}

const SLOTS: [Slot; 6] = [
    Slot::Capabilities,
    Slot::CasGet,
    Slot::CasPut,
    Slot::CasFindMissing,
    Slot::AcGet,
    Slot::Execute,
];

/// Authorizers with `pick(slot)` in each slot.
fn authorizers(pick: impl Fn(Slot) -> Arc<dyn Authorizer>) -> Authorizers {
    Authorizers {
        capabilities: pick(Slot::Capabilities),
        cas_get: pick(Slot::CasGet),
        cas_put: pick(Slot::CasPut),
        cas_find_missing: pick(Slot::CasFindMissing),
        ac_get: pick(Slot::AcGet),
        execute: pick(Slot::Execute),
    }
}

fn policy(authenticator: impl Authenticator, authorizers: Authorizers) -> Policy {
    Policy {
        authenticator: Arc::new(authenticator),
        authorizers,
    }
}

/// One call of a method, on `instance`, reduced to whether it was answered.
type Call = for<'a> fn(&'a Farm, &'a str) -> BoxFuture<'a, Result<(), Status>>;

/// Defines a [`Call`] named `$name`: `$body` is the gRPC call, with `$farm` and
/// `$instance` in scope.
macro_rules! call {
    ($name:ident, |$farm:ident, $instance:ident| $body:expr) => {
        fn $name<'a>($farm: &'a Farm, $instance: &'a str) -> BoxFuture<'a, Result<(), Status>> {
            Box::pin(async move { $body.await.map(|_| ()) })
        }
    };
}

/// A resource name under `instance` (no leading `/` for the empty instance).
fn under(instance: &str, rest: &str) -> String {
    if instance.is_empty() {
        rest.to_owned()
    } else {
        format!("{instance}/{rest}")
    }
}

fn blob() -> Blob {
    Blob::new(b"auth test blob".to_vec())
}

call!(get_capabilities, |farm, instance| farm
    .caps()
    .get_capabilities(GetCapabilitiesRequest {
        instance_name: instance.to_owned(),
    }));
call!(find_missing_blobs, |farm, instance| farm
    .cas()
    .find_missing_blobs(FindMissingBlobsRequest {
        instance_name: instance.to_owned(),
        ..Default::default()
    }));
call!(batch_update_blobs, |farm, instance| farm
    .cas()
    .batch_update_blobs(BatchUpdateBlobsRequest {
        instance_name: instance.to_owned(),
        ..Default::default()
    }));
call!(batch_read_blobs, |farm, instance| farm
    .cas()
    .batch_read_blobs(BatchReadBlobsRequest {
        instance_name: instance.to_owned(),
        ..Default::default()
    }));
call!(get_tree, |farm, instance| farm.cas().get_tree(
    GetTreeRequest {
        instance_name: instance.to_owned(),
        root_digest: Some(blob().proto),
        ..Default::default()
    }
));
call!(split_blob, |farm, instance| farm.cas().split_blob(
    SplitBlobRequest {
        instance_name: instance.to_owned(),
        ..Default::default()
    }
));
call!(get_chunk_mapping, |farm, instance| farm
    .cas()
    .get_chunk_mapping(GetChunkMappingRequest {
        instance_name: instance.to_owned(),
        ..Default::default()
    }));
call!(splice_blob, |farm, instance| farm.cas().splice_blob(
    SpliceBlobRequest {
        instance_name: instance.to_owned(),
        ..Default::default()
    }
));
call!(register_chunk_mapping, |farm, instance| farm
    .cas()
    .register_chunk_mapping(stream::iter([
        RegisterChunkMappingRequest {
            instance_name: instance.to_owned(),
            ..Default::default()
        }
    ])));
call!(get_action_result, |farm, instance| farm
    .ac()
    .get_action_result(GetActionResultRequest {
        instance_name: instance.to_owned(),
        action_digest: Some(blob().proto),
        ..Default::default()
    }));
call!(update_action_result, |farm, instance| farm
    .ac()
    .update_action_result(UpdateActionResultRequest {
        instance_name: instance.to_owned(),
        ..Default::default()
    }));
call!(execute, |farm, instance| farm.exec().execute(
    ExecuteRequest {
        instance_name: instance.to_owned(),
        action_digest: Some(blob().proto),
        ..Default::default()
    }
));
call!(wait_execution, |farm, instance| farm.exec().wait_execution(
    WaitExecutionRequest {
        name: format!("ops/{instance}"),
    }
));
call!(read, |farm, instance| farm.bytestream().read(ReadRequest {
    resource_name: under(
        instance,
        &format!("blobs/{}/{}", blob().proto.hash, blob().proto.size_bytes)
    ),
    ..Default::default()
}));
fn upload(instance: &str) -> WriteRequest {
    let b = blob();
    WriteRequest {
        resource_name: under(
            instance,
            &format!("uploads/u-1/blobs/{}/{}", b.proto.hash, b.proto.size_bytes),
        ),
        write_offset: 0,
        finish_write: true,
        data: b.data,
    }
}

call!(write, |farm, instance| farm
    .bytestream()
    .write(stream::iter([upload(instance)])));
call!(query_write_status, |farm, instance| farm
    .bytestream()
    .query_write_status(QueryWriteStatusRequest {
        resource_name: under(
            instance,
            &format!(
                "uploads/u-1/blobs/{}/{}",
                blob().proto.hash,
                blob().proto.size_bytes
            ),
        ),
    }));

/// Every REAPI method: its gRPC path, the authorizer it asks (`None`: refused
/// whatever the policy), and a call of it.
const METHODS: &[(&str, Option<Slot>, Call)] = &[
    (
        "/build.bazel.remote.execution.v2.Capabilities/GetCapabilities",
        Some(Slot::Capabilities),
        get_capabilities,
    ),
    (
        "/build.bazel.remote.execution.v2.ContentAddressableStorage/FindMissingBlobs",
        Some(Slot::CasFindMissing),
        find_missing_blobs,
    ),
    (
        "/build.bazel.remote.execution.v2.ContentAddressableStorage/BatchUpdateBlobs",
        Some(Slot::CasPut),
        batch_update_blobs,
    ),
    (
        "/build.bazel.remote.execution.v2.ContentAddressableStorage/BatchReadBlobs",
        Some(Slot::CasGet),
        batch_read_blobs,
    ),
    (
        "/build.bazel.remote.execution.v2.ContentAddressableStorage/GetTree",
        Some(Slot::CasGet),
        get_tree,
    ),
    (
        "/build.bazel.remote.execution.v2.ContentAddressableStorage/SplitBlob",
        Some(Slot::CasGet),
        split_blob,
    ),
    (
        "/build.bazel.remote.execution.v2.ContentAddressableStorage/GetChunkMapping",
        Some(Slot::CasGet),
        get_chunk_mapping,
    ),
    (
        "/build.bazel.remote.execution.v2.ContentAddressableStorage/SpliceBlob",
        Some(Slot::CasPut),
        splice_blob,
    ),
    (
        "/build.bazel.remote.execution.v2.ContentAddressableStorage/RegisterChunkMapping",
        Some(Slot::CasPut),
        register_chunk_mapping,
    ),
    (
        "/build.bazel.remote.execution.v2.ActionCache/GetActionResult",
        Some(Slot::AcGet),
        get_action_result,
    ),
    (
        "/build.bazel.remote.execution.v2.ActionCache/UpdateActionResult",
        None,
        update_action_result,
    ),
    (
        "/build.bazel.remote.execution.v2.Execution/Execute",
        Some(Slot::Execute),
        execute,
    ),
    (
        "/build.bazel.remote.execution.v2.Execution/WaitExecution",
        Some(Slot::Execute),
        wait_execution,
    ),
    (
        "/google.bytestream.ByteStream/Read",
        Some(Slot::CasGet),
        read,
    ),
    (
        "/google.bytestream.ByteStream/Write",
        Some(Slot::CasPut),
        write,
    ),
    (
        "/google.bytestream.ByteStream/QueryWriteStatus",
        Some(Slot::CasPut),
        query_write_status,
    ),
];

/// The gRPC path of every method of every service the REAPI routes serve, from the
/// vendored service definitions.
fn served_methods() -> BTreeSet<String> {
    let proto = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../kbf-proto/proto/third_party");
    let files = [
        "build/bazel/remote/execution/v2/remote_execution.proto",
        "google/bytestream/bytestream.proto",
    ];
    let set = protox::compile(files, [proto.join("remote-apis"), proto.join("googleapis")])
        .expect("the vendored protos compile");
    let mut methods = BTreeSet::new();
    for file in set.file.iter().filter(|f| files.contains(&f.name())) {
        for service in &file.service {
            for method in &service.method {
                methods.insert(format!(
                    "/{}.{}/{}",
                    file.package(),
                    service.name(),
                    method.name()
                ));
            }
        }
    }
    methods
}

/// Whether a call was refused by an authorizer (rather than answered, or failed for
/// another reason after authorization).
fn refused_by_authorizer(answer: &Result<(), Status>) -> bool {
    matches!(answer, Err(s) if s.code() == Code::PermissionDenied
        && s.message().starts_with("Authorization"))
}

/// A dispatch for WaitExecution: the operation `ops/<instance>` exists, under
/// `<instance>`, for every instance. Nothing is ever submitted (every Execute here
/// names an action that is not in the CAS).
#[derive(Default)]
struct Stub {
    stages: Mutex<Vec<watch::Sender<Stage>>>,
}

impl Dispatch for Stub {
    fn submit(&self, _submission: Submission) -> Result<Ticket, Status> {
        Err(Status::unavailable("the stub runs nothing"))
    }

    fn wait(&self, name: &str) -> Option<Ticket> {
        let instance = name.strip_prefix("ops/")?;
        let (tx, rx) = watch::channel(Stage::Queued);
        self.stages
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(tx);
        Some(Ticket {
            name: name.to_owned(),
            instance: instance.to_owned(),
            action: kbf_segments::sha256(b"stub"),
            stage: rx,
        })
    }
}

async fn serve(policy: Policy) -> Farm {
    Farm::with_policy(Arc::new(Stub::default()), policy).await
}

/// Catches: a REAPI method the table does not name (a method added to a vendored
/// service that no test covers, and whose authorizer nobody chose), and a table entry
/// for a method that is not served.
#[test]
fn the_table_names_every_served_method() {
    let table: BTreeSet<String> = METHODS.iter().map(|(p, _, _)| (*p).to_owned()).collect();
    assert_eq!(table.len(), METHODS.len(), "a method is listed twice");
    assert_eq!(table, served_methods());
}

/// Catches (per method, the planted mutant "a method skips the authorizer"): a method
/// that serves the call without asking its authorizer, and one that asks another
/// authorizer than the one documented for it. With only its slot denying, each method
/// is refused by authorization; with every other slot denying, it is not.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_method_asks_exactly_its_own_authorizer() {
    for slot in SLOTS {
        let only = |s| -> Arc<dyn Authorizer> {
            if s == slot {
                Arc::new(DenyAuthorizer)
            } else {
                Arc::new(AllowAuthorizer)
            }
        };
        let all_but = |s| -> Arc<dyn Authorizer> {
            if s == slot {
                Arc::new(AllowAuthorizer)
            } else {
                Arc::new(DenyAuthorizer)
            }
        };
        let denied = serve(policy(AllowAuthenticator::default(), authorizers(only))).await;
        let others = serve(policy(AllowAuthenticator::default(), authorizers(all_but))).await;
        for (path, method_slot, call) in METHODS {
            let mine = *method_slot == Some(slot);
            let answer = call(&denied, "i").await;
            assert_eq!(
                refused_by_authorizer(&answer),
                mine,
                "{path} with only {slot:?} denying: {answer:?}"
            );
            if let Err(e) = &answer
                && mine
            {
                assert_eq!(e.message(), "Authorization: Permission denied", "{path}");
            }
            let answer = call(&others, "i").await;
            assert_eq!(
                refused_by_authorizer(&answer),
                method_slot.is_some() && !mine,
                "{path} with all but {slot:?} denying: {answer:?}"
            );
        }
    }
}

/// UpdateActionResult is refused whatever the policy: its refusal is not an
/// authorizer's, and an all-allow policy does not open it.
#[tokio::test]
async fn update_action_result_is_refused_under_any_policy() {
    let farm = serve(Policy::allow_all()).await;
    let e = update_action_result(&farm, "i").await.expect_err("refused");
    assert_eq!(e.code(), Code::PermissionDenied);
    assert!(!refused_by_authorizer(&Err(e)));
}

/// Records which calls it saw, and refuses every one.
#[derive(Default)]
struct Counting(Mutex<BTreeSet<String>>);

impl Authenticator for Counting {
    fn authenticate<'a>(
        &'a self,
        call: &'a Parts,
    ) -> BoxFuture<'a, Result<Arc<AuthenticationMetadata>, Status>> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(call.uri.path().to_owned());
        Box::pin(std::future::ready(Err(Status::unauthenticated(
            "counted and refused",
        ))))
    }
}

/// Catches: a method served without going through the authenticator (a route added
/// outside the layered server), and a refusal that reaches the service anyway or that
/// is answered with another status than the authenticator's.
#[tokio::test]
async fn every_method_passes_through_the_authenticator_first() {
    let counting = Arc::new(Counting::default());
    let farm = serve(Policy {
        authenticator: Arc::clone(&counting) as Arc<dyn Authenticator>,
        authorizers: authorizers(|_| Arc::new(AllowAuthorizer)),
    })
    .await;
    for (path, _, call) in METHODS {
        let e = call(&farm, "i").await.expect_err(path);
        assert_eq!(e.code(), Code::Unauthenticated, "{path}: {e:?}");
        assert_eq!(e.message(), "counted and refused", "{path}");
    }
    let seen = counting
        .0
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    assert_eq!(seen, served_methods());

    let farm = serve(policy(
        DenyAuthenticator::new("nobody"),
        authorizers(|_| Arc::new(AllowAuthorizer)),
    ))
    .await;
    for (path, _, call) in METHODS {
        let e = call(&farm, "i").await.expect_err(path);
        assert_eq!(
            (e.code(), e.message()),
            (Code::Unauthenticated, "nobody"),
            "{path}"
        );
    }
}

/// Records the caller's public metadata and the instance name of every question,
/// and allows each.
#[derive(Default)]
struct Recording(Mutex<Vec<(Option<Value>, String)>>);

impl Authorizer for Recording {
    fn authorize<'a>(
        &'a self,
        metadata: &'a AuthenticationMetadata,
        instance_names: &'a [&'a str],
    ) -> BoxFuture<'a, Vec<Result<(), Status>>> {
        let mut seen = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        for name in instance_names {
            seen.push((metadata.public().cloned(), (*name).to_owned()));
        }
        Box::pin(std::future::ready(vec![Ok(()); instance_names.len()]))
    }
}

/// Catches: an authorizer asked about another instance name than the call's (the
/// ByteStream resource name or the WaitExecution operation misread, or the instance
/// dropped), and a service that reads empty metadata instead of what the
/// authenticator found (the extension lost between layer and handler).
#[tokio::test]
async fn authorizers_see_the_callers_metadata_and_the_calls_instance() {
    let recording = Arc::new(Recording::default());
    let caller = AuthenticationMetadata::new(Some(json!({"user": "ci"})), Some(json!("p")));
    let farm = serve(policy(
        AllowAuthenticator::new(caller),
        authorizers(|_| Arc::clone(&recording) as Arc<dyn Authorizer>),
    ))
    .await;
    for (path, slot, call) in METHODS {
        let before = recording
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len();
        let _ = call(&farm, "team/ci").await;
        let seen = recording.0.lock().unwrap_or_else(PoisonError::into_inner);
        let asked = &seen[before..];
        match slot {
            Some(_) => assert_eq!(
                asked,
                [(Some(json!({"user": "ci"})), "team/ci".to_owned())],
                "{path}"
            ),
            None => assert!(asked.is_empty(), "{path} asked an authorizer"),
        }
    }
}

/// Catches: the routes without a policy refusing anything (behaviour must be
/// unchanged for a server with no policy file), or refusing a call no layer
/// authenticated because its metadata is empty.
#[tokio::test]
async fn with_no_policy_no_call_is_refused() {
    for farm in [
        Farm::with_execution(Arc::new(Stub::default())).await,
        serve(Policy::allow_all()).await,
    ] {
        for (path, _, call) in METHODS {
            let answer = call(&farm, "").await;
            assert!(!refused_by_authorizer(&answer), "{path}: {answer:?}");
            assert!(
                !matches!(&answer, Err(e) if e.code() == Code::Unauthenticated),
                "{path}: {answer:?}"
            );
        }
    }
}
