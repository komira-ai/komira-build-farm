//! Compiles the vendored REAPI and googleapis protos and `kbf.worker.v1` into Rust.
//!
//! Parsing is done by `protox`, in pure Rust, so the build needs no `protoc` binary.
//! Nothing generated is checked in; the code lands in `OUT_DIR`.

use std::path::PathBuf;

use prost::Message as _;

/// The files to generate code for. Their imports are resolved from the include roots
/// and generated too; well-known `google.protobuf` types map to `prost-types`.
const FILES: &[&str] = &[
    "build/bazel/remote/execution/v2/remote_execution.proto",
    "build/bazel/semver/semver.proto",
    "google/bytestream/bytestream.proto",
    "google/longrunning/operations.proto",
    "google/rpc/code.proto",
    "google/rpc/error_details.proto",
    "google/rpc/status.proto",
    "kbf/worker/v1/worker.proto",
];

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?).join("proto");
    let includes = [
        root.join("third_party/remote-apis"),
        root.join("third_party/googleapis"),
        root.clone(),
    ];

    let descriptors = protox::compile(FILES, includes)?;
    // `FILE_DESCRIPTOR_SET` in lib.rs: the services and methods, for tests that must
    // cover every method a listener serves.
    let out = PathBuf::from(std::env::var("OUT_DIR")?);
    std::fs::write(out.join("kbf_descriptors.bin"), descriptors.encode_to_vec())?;
    tonic_prost_build::configure()
        .build_client(true)
        .build_server(true)
        .compile_fds(descriptors)?;

    println!("cargo:rerun-if-changed={}", root.display());
    Ok(())
}
