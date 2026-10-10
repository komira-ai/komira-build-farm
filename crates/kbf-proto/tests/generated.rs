//! The generated types and services exist with the shapes the rest of kbf relies on.
//!
//! These are compile tests first: if `build.rs` stops generating a package, a module
//! path moves, or a field is renamed upstream, this file stops compiling. The asserts
//! catch what compiling alone cannot:
//! - a message whose fields do not survive an encode/decode round trip (a wrong prost
//!   mapping, or the well-known types mapped to something other than `prost-types`);
//! - a gRPC service path that differs from the one clients dial, which would make
//!   every REAPI or worker call fail with UNIMPLEMENTED.

use kbf_proto::google::bytestream::byte_stream_server::ByteStreamServer;
use kbf_proto::google::longrunning::operations_server::OperationsServer;
use kbf_proto::google::rpc::Status;
use kbf_proto::grpc::health::v1::health_server::HealthServer;
use kbf_proto::reapi::{
    self, ActionResult, Digest, FindMissingBlobsRequest, action_cache_server::ActionCacheServer,
    capabilities_server::CapabilitiesServer,
    content_addressable_storage_server::ContentAddressableStorageServer,
    execution_server::ExecutionServer,
};
use kbf_proto::worker::{
    self, Capability, DaemonMessage, Hello, LeaseId, Result as WorkerResult, ServerMessage, Start,
    daemon_message, server_message, worker_server::WorkerServer,
};
use prost::Message;
use tonic::server::NamedService;

fn digest(hash: &str, size_bytes: i64) -> Digest {
    Digest {
        hash: hash.to_owned(),
        size_bytes,
    }
}

#[test]
fn find_missing_blobs_request_round_trips() {
    let req = FindMissingBlobsRequest {
        instance_name: "main".to_owned(),
        blob_digests: vec![digest(&"ab".repeat(32), 42), digest(&"cd".repeat(32), 0)],
        digest_function: reapi::digest_function::Value::Sha256 as i32,
    };
    let back = FindMissingBlobsRequest::decode(req.encode_to_vec().as_slice()).expect("decode");
    assert_eq!(back, req);
    assert_eq!(back.blob_digests[0].size_bytes, 42);
}

#[test]
fn action_result_uses_prost_types_for_well_known_types() {
    let result = ActionResult {
        exit_code: 3,
        execution_metadata: Some(reapi::ExecutedActionMetadata {
            worker: "node-1".to_owned(),
            worker_start_timestamp: Some(prost_types::Timestamp {
                seconds: 1_800_000_000,
                nanos: 5,
            }),
            ..Default::default()
        }),
        ..Default::default()
    };
    let back = ActionResult::decode(result.encode_to_vec().as_slice()).expect("decode");
    assert_eq!(back, result);
}

#[test]
fn services_have_their_upstream_names() {
    assert_eq!(
        <ExecutionServer<()> as NamedService>::NAME,
        "build.bazel.remote.execution.v2.Execution"
    );
    assert_eq!(
        <ActionCacheServer<()> as NamedService>::NAME,
        "build.bazel.remote.execution.v2.ActionCache"
    );
    assert_eq!(
        <ContentAddressableStorageServer<()> as NamedService>::NAME,
        "build.bazel.remote.execution.v2.ContentAddressableStorage"
    );
    assert_eq!(
        <CapabilitiesServer<()> as NamedService>::NAME,
        "build.bazel.remote.execution.v2.Capabilities"
    );
    assert_eq!(
        <ByteStreamServer<()> as NamedService>::NAME,
        "google.bytestream.ByteStream"
    );
    assert_eq!(
        <OperationsServer<()> as NamedService>::NAME,
        "google.longrunning.Operations"
    );
    assert_eq!(
        <WorkerServer<()> as NamedService>::NAME,
        "kbf.worker.v1.Worker"
    );
    assert_eq!(
        <HealthServer<()> as NamedService>::NAME,
        "grpc.health.v1.Health"
    );
}

#[test]
fn worker_messages_round_trip() {
    let hello = DaemonMessage {
        message: Some(daemon_message::Message::Hello(Hello {
            protocol_version: 1,
            node_id: "node-1".to_owned(),
            daemon_version: "0.1.0".to_owned(),
            capabilities: vec![
                Capability {
                    key: "arch".to_owned(),
                    value: "x86_64".to_owned(),
                },
                Capability {
                    key: "drivers".to_owned(),
                    value: "container".to_owned(),
                },
            ],
            report_hash: vec![7; 32],
            instance_id: "3f9c0a7d5e1b4c2a8d6f0e9b7a5c3d1e".to_owned(),
        })),
    };
    let back = DaemonMessage::decode(hello.encode_to_vec().as_slice()).expect("decode");
    assert_eq!(back, hello);

    let start = ServerMessage {
        message: Some(server_message::Message::Start(Start {
            lease_id: Some(LeaseId { term: 2, seq: 9 }),
            kind: "action".to_owned(),
            action_digest: Some(digest(&"ef".repeat(32), 140)),
            millicpus: 1500,
            memory_bytes: 1 << 30,
            heartbeat_seq: 12,
            valid_for_ms: 14_000,
        })),
    };
    let back = ServerMessage::decode(start.encode_to_vec().as_slice()).expect("decode");
    assert_eq!(back, start);

    let cancel = ServerMessage {
        message: Some(server_message::Message::Cancel(worker::Cancel {
            lease_id: Some(LeaseId { term: 2, seq: 8 }),
        })),
    };
    let back = ServerMessage::decode(cancel.encode_to_vec().as_slice()).expect("decode");
    assert_eq!(back, cancel);

    let result = DaemonMessage {
        message: Some(daemon_message::Message::Result(WorkerResult {
            lease_id: Some(LeaseId { term: 2, seq: 9 }),
            status: Some(Status::default()),
            action_result: Some(ActionResult::default()),
            action_digest: Some(Digest {
                hash: "cd".repeat(32),
                size_bytes: 9,
            }),
            memory_kill: worker::MemoryKill::NodePressure as i32,
        })),
    };
    let back = DaemonMessage::decode(result.encode_to_vec().as_slice()).expect("decode");
    assert_eq!(back, result);

    let heartbeat = DaemonMessage {
        message: Some(daemon_message::Message::Heartbeat(worker::Heartbeat {
            seq: 11,
            report_hash: vec![7; 32],
            running: vec![LeaseId { term: 2, seq: 9 }],
        })),
    };
    let back = DaemonMessage::decode(heartbeat.encode_to_vec().as_slice()).expect("decode");
    assert_eq!(back, heartbeat);

    let usage = worker::ResourceUsage {
        cpu_user_micros: 1,
        cpu_system_micros: 2,
        peak_memory_bytes: 3,
        wall_micros: 4,
    };
    let back = worker::ResourceUsage::decode(usage.encode_to_vec().as_slice()).expect("decode");
    assert_eq!(back, usage);
}

/// Catches a renumbered `Result.memory_kill` field or `MemoryKill` value: a daemon and
/// a server built from different revisions would read an own-limit kill as a busy
/// node's (or the reverse), and the server would raise the booking for the wrong one.
#[test]
fn memory_kill_keeps_its_wire_numbers() {
    let only = |kill: worker::MemoryKill| WorkerResult {
        memory_kill: kill as i32,
        ..WorkerResult::default()
    };
    // Field 5, varint: tag 5 << 3 = 0x28.
    assert_eq!(
        only(worker::MemoryKill::OwnLimit).encode_to_vec(),
        [0x28, 1]
    );
    assert_eq!(
        only(worker::MemoryKill::NodePressure).encode_to_vec(),
        [0x28, 2]
    );
    assert!(
        only(worker::MemoryKill::Unspecified)
            .encode_to_vec()
            .is_empty()
    );
    assert_eq!(
        worker::MemoryKill::try_from(2),
        Ok(worker::MemoryKill::NodePressure)
    );
}
