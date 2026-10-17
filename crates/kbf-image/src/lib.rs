//! The node's image source: an image named by manifest digest is fetched from its
//! registry, every byte checked against a digest on the way in, and its layers
//! unpacked into a root filesystem. No container tool takes part: the OCI backend of
//! the container driver reads images from here and nowhere else.
//!
//! What is verified, and when:
//! - the manifest's bytes hash to the digest the image is pinned by, before anything in
//!   them is believed ([`manifest::parse`]); a manifest that is an image index (or
//!   Docker manifest list) is refused, since an index digest names a different image
//!   on each architecture: pins name the per-architecture manifest;
//! - the config blob and every layer blob are hashed while they are written, and
//!   compared with the manifest's digest (and size) before anyone uses them
//!   ([`Registry::fetch`]); a file that fails the check is removed;
//! - the image's own platform, when its config states one, is this node's.
//!
//! Layers are applied, in order, by [`unpack::Unpack`]: whiteouts delete what lower
//! layers made, an opaque marker empties a directory of what lower layers made, links
//! and paths are resolved inside the root and never outside it, and each file gets
//! the owner and mode of its tar header (owner first: `chown` clears setuid bits).
//!
//! Limits bound what a wrong pin or a hostile registry can make a node write
//! ([`Limits`]). Only anonymous pulls are supported.

mod error;
mod http;
pub mod manifest;
mod reference;
mod registry;
pub mod unpack;

pub use error::{Error, UnpackError};
pub use manifest::{Compression, Layer, Manifest};
pub use reference::Location;
pub use registry::{LayerFile, Limits, Platform, Registry, Timeouts};
pub use unpack::{Ownership, Unpack, Unpacked, ensure_paths};
