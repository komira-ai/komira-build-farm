//! What can go wrong fetching an image or unpacking its layers.

use std::path::PathBuf;

/// Why an image could not be fetched.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The repository names no registry path.
    #[error("repository {0:?} names no image path")]
    Reference(String),
    /// A plain-HTTP address that was not allowed.
    #[error("{0} is not https, and not a registry allowed to use plain http")]
    Insecure(String),
    /// Connecting, the TLS handshake, the request or a body failed.
    #[error("{what}: {why}")]
    Transport { what: String, why: String },
    /// A request was answered with a status that is not success.
    #[error("{url}: the registry answered {status}")]
    Status { url: String, status: u16 },
    /// The registry's authentication challenge could not be answered.
    #[error("registry authentication: {0}")]
    Auth(String),
    /// A response or file is larger than its limit.
    #[error("{what} is larger than {limit} bytes")]
    TooLarge { what: String, limit: u64 },
    /// The manifest's bytes do not hash to the digest the image is pinned by.
    #[error("the manifest hashes to {actual}, not the pinned {pinned}")]
    ManifestDigest { pinned: String, actual: String },
    /// The digest names an image index or manifest list.
    #[error("{0} names an image index; pin the manifest of the node's platform")]
    Index(String),
    /// A manifest or config feature this store does not take.
    #[error("{0}")]
    Unsupported(String),
    /// A manifest, config or token that is not what its type says.
    #[error("{0}")]
    Malformed(String),
    /// A blob's bytes do not hash to its digest.
    #[error("the blob {digest} hashes to {actual}")]
    BlobDigest { digest: String, actual: String },
    /// A blob's length is not the manifest's.
    #[error("the blob {digest} is {got} bytes, the manifest says {declared}")]
    BlobSize {
        digest: String,
        declared: u64,
        got: u64,
    },
    /// The image is for another platform than the node's.
    #[error("the image is for {have}, this node is {want}")]
    Platform { want: String, have: String },
    /// The node's architecture is not one images are made for here.
    #[error("this node's architecture {0} has no image platform name")]
    Arch(String),
    /// A file of the store could not be written.
    #[error("{}: {source}", path.display())]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
}

/// Why layers could not be unpacked.
#[derive(Debug, thiserror::Error)]
pub enum UnpackError {
    /// A file operation failed.
    #[error("{}: {source}", path.display())]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    /// The tar could not be read.
    #[error("layer {}: {why}", layer.display())]
    Archive { layer: PathBuf, why: String },
    /// An entry's path leaves the root.
    #[error("the entry {0:?} leaves the root filesystem")]
    Unsafe(PathBuf),
    /// An owner above the ids the container maps.
    #[error("{}: owner {id} is above the {max} ids of the container's user namespace", path.display())]
    IdRange { path: PathBuf, id: u64, max: u32 },
    /// Symlinks pointing at each other.
    #[error("{}: too many levels of symbolic links", .0.display())]
    SymlinkLoop(PathBuf),
    /// More bytes of files than the limit.
    #[error("the layers hold more than {0} bytes of files")]
    TooBig(u64),
}
