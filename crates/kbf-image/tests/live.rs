//! The client against the registries kbf's images come from: Docker Hub and gcr.io, over
//! the network, by the manifests the tests and the M2 cell pin. Run by hand (`cargo test
//! -p kbf-image --test live -- --ignored`); the real-`crun` jobs of CI run the same
//! fetches through the driver.

mod support;

use kbf_image::{Ownership, Registry, Unpack};
use kbf_types::ImageRef;
use support::registry::my_ids;
use support::scratch;

/// busybox 1.37.0's `linux/amd64` and `linux/arm64` manifests.
const BUSYBOX_AMD64: &str =
    "sha256:66a6306db78bf2dbf3487f293aa8d6990d8e506fdffab9cc43fe422becf886e4";
const BUSYBOX_ARM64: &str =
    "sha256:d82c2ab94640ded77cf76514ce6a84870761105058a4a9e51b05a8a79be97a6c";
/// The M2 cell's distroless image (`gcr.io`), its `linux/amd64` manifest.
const DISTROLESS_AMD64: &str =
    "sha256:d2add786f2a5f43d1ab3ae54cd3193de929d0d12c378ff60891921f56f3e47ff";

async fn fetch_and_unpack(reference: &str, name: &str) -> std::path::PathBuf {
    let image = ImageRef::parse(reference).expect("an image");
    let dir = scratch(name);
    let layers = Registry::new()
        .fetch(&image, &dir)
        .await
        .unwrap_or_else(|e| panic!("{reference}: {e}"));
    let sources: Vec<_> = layers
        .iter()
        .map(|l| (dir.join(&l.name), l.compression))
        .collect();
    let (uid, gid) = my_ids();
    let root = dir.join("rootfs");
    let unpacked = Unpack::new(Ownership::AllTo { uid, gid })
        .run(&root, &sources)
        .unwrap_or_else(|e| panic!("{reference}: {e}"));
    eprintln!("{reference}: {} layers, {unpacked:?}", layers.len());
    root
}

/// Docker Hub: an anonymous Bearer token, a blob redirect to its CDN, a gzip layer.
#[tokio::test]
#[ignore = "needs the network"]
async fn busybox_from_docker_hub_unpacks() {
    let digest = if std::env::consts::ARCH == "aarch64" {
        BUSYBOX_ARM64
    } else {
        BUSYBOX_AMD64
    };
    let root = fetch_and_unpack(
        &format!("docker://docker.io/library/busybox@{digest}"),
        "live-busybox",
    )
    .await;
    assert!(root.join("bin/busybox").is_file());
    assert!(root.join("bin/sh").exists());
}

/// gcr.io: the other registry the cell pulls from, and an image with several layers and
/// no shell.
#[tokio::test]
#[ignore = "needs the network"]
async fn distroless_from_gcr_unpacks() {
    if std::env::consts::ARCH != "x86_64" {
        return;
    }
    let root = fetch_and_unpack(
        &format!("docker://gcr.io/distroless/base-debian12@{DISTROLESS_AMD64}"),
        "live-distroless",
    )
    .await;
    assert!(root.join("etc/os-release").is_file());
}
