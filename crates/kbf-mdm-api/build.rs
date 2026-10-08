//! Compiles `kbf.mdmgate.v1` into Rust, client and server, with `protox` (no `protoc`
//! binary). Nothing generated is checked in; the code lands in `OUT_DIR`.

use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?).join("proto");
    let descriptors = protox::compile(["kbf/mdmgate/v1/gate.proto"], [root.clone()])?;
    tonic_prost_build::configure()
        .build_client(true)
        .build_server(true)
        .compile_fds(descriptors)?;
    println!("cargo:rerun-if-changed={}", root.display());
    Ok(())
}
