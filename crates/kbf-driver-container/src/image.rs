//! The `container-image` platform property: `docker://<repo>@sha256:<digest>`.
//!
//! One action digest must never mean two sets of bytes, so an image is named
//! only by a per-architecture manifest digest. A tag is refused by its spelling, in
//! [`ImageRef::parse`] (in `kbf-types`, so the deploy tool checks `--image` with it too).
//! An image index digest (a multi-architecture list) cannot be told from a manifest
//! digest by spelling: the node's image store fetches the manifest by the digest and
//! refuses an index (`kbf-image`). The Podman path reads the manifest bytes Podman's
//! store keeps under the digest instead and decides with [`manifest_kind`].

use base64::Engine as _;
use sha2::{Digest as _, Sha256};

/// The platform property that names an action's image.
pub const PROPERTY: &str = "container-image";

pub use kbf_types::{ImageError, ImageRef};

const SHA256: &str = "sha256:";

/// Why this node cannot run an image named by digest.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ImageCheckError {
    /// The node's image store does not hold it.
    #[error("image {0} is not in this node's image store")]
    Missing(ImageRef),
    /// Its digest names an image index (a multi-architecture list).
    #[error("container-image {0} names an image index; name the per-architecture manifest digest")]
    Index(ImageRef),
    /// The image could not be fetched or unpacked, or the store cannot vouch for it.
    #[error("{0}")]
    Failed(String),
}

/// What a manifest digest names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ManifestKind {
    /// One image for one architecture.
    Image,
    /// An image index (OCI) or manifest list (Docker): one entry per architecture.
    Index,
}

/// The file, in an image's directory of the containers/storage image store
/// (`<graph root>/<driver>-images/<image id>/`), that holds the manifest pulled under
/// `digest`. containers/storage names an image's big-data items `=` + base64 of the
/// key, and the key of a manifest is `manifest-<digest>`.
#[must_use]
pub fn manifest_file(digest: &str) -> String {
    let key = format!("manifest-{digest}");
    format!("={}", base64::engine::general_purpose::STANDARD.encode(key))
}

/// Decides what `bytes`, the manifest stored under `digest`, describe. Fails if the
/// bytes do not hash to `digest` (the store is not to be trusted then) or are not a
/// JSON object.
pub fn manifest_kind(bytes: &[u8], digest: &str) -> Result<ManifestKind, String> {
    let actual = format!("{SHA256}{}", hex::encode(Sha256::digest(bytes)));
    if actual != digest {
        return Err(format!(
            "the image store's manifest for {digest} hashes to {actual}"
        ));
    }
    let json: serde_json::Value = serde_json::from_slice(bytes)
        .map_err(|e| format!("the image store's manifest for {digest} is not JSON: {e}"))?;
    let object = json
        .as_object()
        .ok_or_else(|| format!("the image store's manifest for {digest} is not a JSON object"))?;
    let media_type = object
        .get("mediaType")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let index = object.contains_key("manifests")
        || media_type.ends_with(".index.v1+json")
        || media_type.ends_with(".manifest.list.v2+json");
    Ok(if index {
        ManifestKind::Index
    } else {
        ManifestKind::Image
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches a file name containers/storage would not use for the manifest key, so
    /// every check would find no manifest.
    #[test]
    fn manifest_file_is_containers_storage_big_data_naming() {
        assert_eq!(
            manifest_file(&format!("sha256:{}", "a".repeat(64))),
            "=bWFuaWZlc3Qtc2hhMjU2OmFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWE="
        );
    }

    fn sha(bytes: &[u8]) -> String {
        format!("{SHA256}{}", hex::encode(Sha256::digest(bytes)))
    }

    /// Catches an image index (OCI) or manifest list (Docker) passing as one image: its
    /// digest names a different image on each architecture (the index mutant).
    #[test]
    fn indexes_and_lists_are_told_from_images() {
        let cases: [(&[u8], ManifestKind); 5] = [
            (
                br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{},"layers":[]}"#,
                ManifestKind::Image,
            ),
            (
                br#"{"schemaVersion":2,"config":{},"layers":[]}"#,
                ManifestKind::Image,
            ),
            (br#"{"manifests":[]}"#, ManifestKind::Index),
            (
                br#"{"mediaType":"application/vnd.oci.image.index.v1+json"}"#,
                ManifestKind::Index,
            ),
            (
                br#"{"mediaType":"application/vnd.docker.distribution.manifest.list.v2+json"}"#,
                ManifestKind::Index,
            ),
        ];
        for (bytes, want) in cases {
            assert_eq!(
                manifest_kind(bytes, &sha(bytes)),
                Ok(want),
                "{}",
                String::from_utf8_lossy(bytes)
            );
        }
    }

    /// Catches a store whose bytes are not the manifest asked for, or not a manifest at
    /// all, being trusted.
    #[test]
    fn unverifiable_manifests_are_errors() {
        let bytes = br#"{"manifests":[]}"#;
        let other = sha(b"other");
        assert!(
            manifest_kind(bytes, &other)
                .expect_err("hash")
                .contains("hashes to")
        );
        assert!(
            manifest_kind(b"not json", &sha(b"not json"))
                .expect_err("json")
                .contains("not JSON")
        );
        assert!(
            manifest_kind(b"[1]", &sha(b"[1]"))
                .expect_err("object")
                .contains("not a JSON object")
        );
    }
}
