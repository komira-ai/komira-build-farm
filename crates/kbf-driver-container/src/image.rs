//! The `container-image` platform property: `docker://<repo>@sha256:<digest>`.
//!
//! One action digest must never mean two sets of bytes (RFC 10.8), so an image is named
//! only by a per-architecture manifest digest. A tag is refused here, by its spelling.
//! An image index digest (a multi-architecture list) cannot be told from a manifest
//! digest by spelling, nor by Podman's image metadata: Podman 4.9 records one `Digest`
//! per image, the digest of its first pull, and lists every digest it was pulled by
//! under `RepoDigests`. So the runtime reads the manifest bytes the node's store keeps
//! under the requested digest and decides with [`manifest_kind`].

use std::fmt;

use base64::Engine as _;
use sha2::{Digest as _, Sha256};

/// The platform property that names an action's image.
pub const PROPERTY: &str = "container-image";

const SCHEME: &str = "docker://";
const SHA256: &str = "sha256:";

/// An image named by repository and manifest digest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageRef {
    repository: String,
    digest: String,
}

/// Why a `container-image` value is not an image by digest.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ImageError {
    #[error("container-image {0:?} does not start with {SCHEME}")]
    Scheme(String),
    #[error("container-image {0:?} names no digest; name an image as <repo>@sha256:<digest>")]
    NoDigest(String),
    #[error("container-image {0:?} names a tag; name an image by digest only")]
    Tag(String),
    #[error("container-image {0:?} has no repository")]
    NoRepository(String),
    #[error("container-image {0:?}: the digest is not sha256 and 64 lowercase hex digits")]
    BadDigest(String),
}

impl ImageRef {
    /// Parses a `container-image` value.
    pub fn parse(value: &str) -> Result<Self, ImageError> {
        let rest = value
            .strip_prefix(SCHEME)
            .ok_or_else(|| ImageError::Scheme(value.to_owned()))?;
        let (repository, digest) = rest
            .split_once('@')
            .ok_or_else(|| ImageError::NoDigest(value.to_owned()))?;
        if repository.is_empty() {
            return Err(ImageError::NoRepository(value.to_owned()));
        }
        // A colon in the last path component is a tag (`repo:tag@sha256:...`); a colon
        // before the first slash is a registry port (`host:5000/repo`).
        let last = repository.rsplit('/').next().unwrap_or(repository);
        if last.contains(':') {
            return Err(ImageError::Tag(value.to_owned()));
        }
        let hex = digest
            .strip_prefix(SHA256)
            .ok_or_else(|| ImageError::BadDigest(value.to_owned()))?;
        if hex.len() != 64 || !hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            return Err(ImageError::BadDigest(value.to_owned()));
        }
        Ok(Self {
            repository: repository.to_owned(),
            digest: digest.to_owned(),
        })
    }

    /// The manifest digest, `sha256:<hex>`.
    #[must_use]
    pub fn digest(&self) -> &str {
        &self.digest
    }
}

/// The reference Podman resolves: `<repo>@sha256:<hex>`.
impl fmt::Display for ImageRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@{}", self.repository, self.digest)
    }
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

    const HEX: &str = "bdf57e528e45e4433820e045b29b4597825a1c9e38353532d90a01445013f82e";

    fn image(rest: &str) -> String {
        format!("{SCHEME}{rest}")
    }

    /// Catches a digest reference being mangled on its way to Podman.
    #[test]
    fn a_digest_reference_parses() {
        let value = image(&format!("docker.io/library/busybox@sha256:{HEX}"));
        let parsed = ImageRef::parse(&value).expect("parses");
        assert_eq!(
            parsed.to_string(),
            format!("docker.io/library/busybox@sha256:{HEX}")
        );
        assert_eq!(parsed.digest(), format!("sha256:{HEX}"));
    }

    /// Catches a registry port being taken for a tag.
    #[test]
    fn a_registry_port_is_not_a_tag() {
        let value = image(&format!("registry.example:5000/team/tool@sha256:{HEX}"));
        assert!(ImageRef::parse(&value).is_ok());
    }

    /// Catches a tag being accepted: a tag can move, so one action digest could mean
    /// two images (the "tag instead of a digest" mutant).
    #[test]
    fn a_tag_is_refused() {
        let tag_only = image("docker.io/library/busybox:1.37");
        assert_eq!(
            ImageRef::parse(&tag_only),
            Err(ImageError::NoDigest(tag_only.clone()))
        );
        let tag_and_digest = image(&format!("docker.io/library/busybox:1.37@sha256:{HEX}"));
        assert_eq!(
            ImageRef::parse(&tag_and_digest),
            Err(ImageError::Tag(tag_and_digest.clone()))
        );
        let bare_tag = image(&format!("busybox:latest@sha256:{HEX}"));
        assert_eq!(
            ImageRef::parse(&bare_tag),
            Err(ImageError::Tag(bare_tag.clone()))
        );
    }

    /// Catches malformed references reaching Podman, which might resolve them some
    /// other way (a short name searched in registries, a different digest algorithm).
    #[test]
    fn malformed_references_are_refused() {
        let cases = [
            (
                format!("busybox@sha256:{HEX}"),
                ImageError::Scheme(format!("busybox@sha256:{HEX}")),
            ),
            (
                image(&format!("@sha256:{HEX}")),
                ImageError::NoRepository(image(&format!("@sha256:{HEX}"))),
            ),
            (
                image(&format!("busybox@sha512:{HEX}")),
                ImageError::BadDigest(image(&format!("busybox@sha512:{HEX}"))),
            ),
            (
                image("busybox@sha256:abc"),
                ImageError::BadDigest(image("busybox@sha256:abc")),
            ),
            (
                image(&format!("busybox@sha256:{}", HEX.to_uppercase())),
                ImageError::BadDigest(image(&format!("busybox@sha256:{}", HEX.to_uppercase()))),
            ),
        ];
        for (value, want) in cases {
            assert_eq!(ImageRef::parse(&value), Err(want), "{value}");
        }
    }

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
