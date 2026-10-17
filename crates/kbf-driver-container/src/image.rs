//! The `container-image` platform property: `docker://<repo>@sha256:<digest>`.
//!
//! One action digest must never mean two sets of bytes, so an image is named
//! only by a per-architecture manifest digest. A tag is refused by its spelling, in
//! [`ImageRef::parse`] (in `kbf-types`, so the deploy tool checks `--image` with it too).
//! An image index digest (a multi-architecture list) cannot be told from a manifest
//! digest by spelling: the node's image store fetches the manifest by the digest and
//! refuses an index (`kbf-image`).

/// The platform property that names an action's image.
pub const PROPERTY: &str = "container-image";

pub use kbf_types::{ImageError, ImageRef};

/// Why this node cannot run an image named by digest.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ImageCheckError {
    /// Its digest names an image index (a multi-architecture list).
    #[error("container-image {0} names an image index; name the per-architecture manifest digest")]
    Index(ImageRef),
    /// The image could not be fetched or unpacked.
    #[error("{0}")]
    Failed(String),
}
