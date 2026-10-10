//! Generated protocol code: the Remote Execution API v2 and the `kbf.worker.v1`
//! server-daemon protocol, compiled from vendored `.proto` files by `build.rs`.
//!
//! Modules mirror the proto packages, because generated code names other packages by
//! relative paths (`super::super::...`). The vendored files, their licences and the
//! upstream commits they come from are under `proto/third_party` (see its `PINS`).

// Generated code follows the proto comments and shapes, not clippy style.
#![allow(clippy::all)]

/// `build.bazel.*`: the Remote Execution API and its semantic version message.
pub mod build {
    pub mod bazel {
        pub mod remote {
            pub mod execution {
                pub mod v2 {
                    tonic::include_proto!("build.bazel.remote.execution.v2");
                }
            }
        }
        pub mod semver {
            tonic::include_proto!("build.bazel.semver");
        }
    }
}

/// `google.*`: the googleapis packages the Remote Execution API imports, and ByteStream.
pub mod google {
    pub mod api {
        tonic::include_proto!("google.api");
    }
    pub mod bytestream {
        tonic::include_proto!("google.bytestream");
    }
    pub mod longrunning {
        tonic::include_proto!("google.longrunning");
    }
    pub mod rpc {
        tonic::include_proto!("google.rpc");
    }
}

/// `kbf.*`: kbf's own protocols.
pub mod kbf {
    pub mod worker {
        /// The protocol between `kbf-daemon` and `kbf-server`.
        pub mod v1 {
            tonic::include_proto!("kbf.worker.v1");
        }
    }
}

/// The compiled files, every one `build.rs` generates code for and their imports, as
/// an encoded `google.protobuf.FileDescriptorSet` (`prost_types::FileDescriptorSet`):
/// the services and methods a listener can serve, for tests that must cover them all.
pub const FILE_DESCRIPTOR_SET: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/kbf_descriptors.bin"));

/// The Remote Execution API v2, under a short name.
pub use build::bazel::remote::execution::v2 as reapi;
/// The `kbf.worker.v1` protocol, under a short name.
pub use kbf::worker::v1 as worker;
