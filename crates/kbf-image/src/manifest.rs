//! Reading an image manifest: checked against the digest the image is pinned by, and
//! reduced to the blobs a node fetches (the config and the layers, each with its digest
//! and size).

use serde::Deserialize;
use sha2::{Digest as _, Sha256};

use crate::Error;

const OCI_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
const DOCKER_MANIFEST: &str = "application/vnd.docker.distribution.manifest.v2+json";
const OCI_INDEX: &str = "application/vnd.oci.image.index.v1+json";
const DOCKER_LIST: &str = "application/vnd.docker.distribution.manifest.list.v2+json";

/// The `Accept` header of a manifest request: both single-image types, and both index
/// types, which a registry then answers with (so the refusal can say index) rather than
/// with a 404.
pub(crate) const ACCEPT: &str = "application/vnd.oci.image.manifest.v1+json, \
    application/vnd.docker.distribution.manifest.v2+json, \
    application/vnd.oci.image.index.v1+json, \
    application/vnd.docker.distribution.manifest.list.v2+json";

const OCI_CONFIG: &str = "application/vnd.oci.image.config.v1+json";
const DOCKER_CONFIG: &str = "application/vnd.docker.container.image.v1+json";

/// How a layer blob is compressed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Compression {
    /// A plain tar.
    None,
    /// A gzip stream of a tar.
    Gzip,
}

/// One layer to fetch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Layer {
    /// `sha256:<hex>` of the blob.
    pub digest: String,
    /// The blob's length in bytes.
    pub size: u64,
    pub compression: Compression,
}

/// What a node needs of an image manifest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Manifest {
    /// The config blob's digest and size.
    pub config: (String, u64),
    /// The layers, lowest first.
    pub layers: Vec<Layer>,
}

#[derive(Deserialize)]
struct Document {
    #[serde(rename = "schemaVersion")]
    schema_version: Option<u64>,
    #[serde(rename = "mediaType")]
    media_type: Option<String>,
    manifests: Option<serde_json::Value>,
    config: Option<Descriptor>,
    layers: Option<Vec<Descriptor>>,
}

#[derive(Deserialize)]
struct Descriptor {
    #[serde(rename = "mediaType")]
    media_type: String,
    digest: String,
    size: u64,
    #[serde(default)]
    urls: Vec<String>,
}

/// `sha256:<hex>` of `bytes`.
#[must_use]
pub fn sha256_of(bytes: &[u8]) -> String {
    format!("sha256:{}", hex::encode(Sha256::digest(bytes)))
}

/// Whether `digest` is `sha256:` and 64 lowercase hex digits, so it is safe as a path
/// component and a URL part.
fn well_formed(digest: &str) -> bool {
    digest.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    })
}

/// The limits [`parse`] applies to a manifest.
#[derive(Clone, Copy, Debug)]
pub struct Bounds {
    /// How many layers an image may have.
    pub layers: usize,
    /// The sum of the layers' sizes.
    pub bytes: u64,
}

/// Reads `bytes`, the manifest the image is pinned by `pinned`.
///
/// # Errors
/// The bytes do not hash to `pinned`, are an image index, use a schema, media type or
/// layer kind this store does not take (Docker schema 1, zstd layers, foreign layers),
/// are not a manifest, or are beyond `bounds`.
pub fn parse(bytes: &[u8], pinned: &str, bounds: Bounds) -> Result<Manifest, Error> {
    let actual = sha256_of(bytes);
    if actual != pinned {
        return Err(Error::ManifestDigest {
            pinned: pinned.to_owned(),
            actual,
        });
    }
    let doc: Document = serde_json::from_slice(bytes)
        .map_err(|e| Error::Malformed(format!("the manifest {pinned} is not a manifest: {e}")))?;
    let media = doc.media_type.as_deref();
    if doc.manifests.is_some() || matches!(media, Some(OCI_INDEX | DOCKER_LIST)) {
        return Err(Error::Index(pinned.to_owned()));
    }
    if doc.schema_version != Some(2) {
        return Err(Error::Unsupported(format!(
            "the manifest {pinned} has schema version {:?}; only version 2 is supported",
            doc.schema_version
        )));
    }
    if !matches!(media, None | Some(OCI_MANIFEST | DOCKER_MANIFEST)) {
        return Err(Error::Unsupported(format!(
            "the manifest {pinned} has media type {media:?}"
        )));
    }
    let config = doc
        .config
        .ok_or_else(|| Error::Malformed(format!("the manifest {pinned} has no config")))?;
    if !matches!(config.media_type.as_str(), OCI_CONFIG | DOCKER_CONFIG) {
        return Err(Error::Unsupported(format!(
            "the config has media type {}",
            config.media_type
        )));
    }
    let layers = doc
        .layers
        .ok_or_else(|| Error::Malformed(format!("the manifest {pinned} has no layers")))?;
    if layers.len() > bounds.layers {
        return Err(Error::TooLarge {
            what: format!("the manifest's {} layers", layers.len()),
            limit: bounds.layers as u64,
        });
    }
    let mut total = config.size;
    let mut out = Vec::with_capacity(layers.len());
    for layer in layers {
        total = total.saturating_add(layer.size);
        out.push(Layer {
            compression: compression(&layer)?,
            digest: layer.digest,
            size: layer.size,
        });
    }
    if total > bounds.bytes {
        return Err(Error::TooLarge {
            what: "the image's blobs".to_owned(),
            limit: bounds.bytes,
        });
    }
    for digest in std::iter::once(&config.digest).chain(out.iter().map(|l| &l.digest)) {
        if !well_formed(digest) {
            return Err(Error::Malformed(format!(
                "the manifest names the blob {digest:?}, which is not sha256 and 64 lowercase hex digits"
            )));
        }
    }
    Ok(Manifest {
        config: (config.digest, config.size),
        layers: out,
    })
}

/// How the layer `layer` is compressed, if it is a kind a node unpacks.
fn compression(layer: &Descriptor) -> Result<Compression, Error> {
    if !layer.urls.is_empty() || layer.media_type.contains("foreign") {
        return Err(Error::Unsupported(format!(
            "the layer {} is foreign (served from elsewhere than the registry)",
            layer.digest
        )));
    }
    match layer.media_type.as_str() {
        "application/vnd.oci.image.layer.v1.tar" => Ok(Compression::None),
        "application/vnd.oci.image.layer.v1.tar+gzip"
        | "application/vnd.docker.image.rootfs.diff.tar.gzip" => Ok(Compression::Gzip),
        other if other.ends_with("+zstd") => Err(Error::Unsupported(format!(
            "the layer {} is zstd-compressed ({other}); this store reads tar and tar+gzip layers",
            layer.digest
        ))),
        other => Err(Error::Unsupported(format!(
            "the layer {} has media type {other}",
            layer.digest
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOUNDS: Bounds = Bounds {
        layers: 4,
        bytes: 1000,
    };

    fn hex(n: u8) -> String {
        format!("sha256:{}", format!("{n:02x}").repeat(32))
    }

    fn manifest(extra: &str) -> Vec<u8> {
        format!(
            r#"{{"schemaVersion":2,"mediaType":"{OCI_MANIFEST}",
            "config":{{"mediaType":"{OCI_CONFIG}","digest":"{}","size":10}},
            "layers":[{{"mediaType":"application/vnd.oci.image.layer.v1.tar+gzip","digest":"{}","size":20}},
            {{"mediaType":"application/vnd.docker.image.rootfs.diff.tar.gzip","digest":"{}","size":30}},
            {{"mediaType":"application/vnd.oci.image.layer.v1.tar","digest":"{}","size":40}}]{extra}}}"#,
            hex(1),
            hex(2),
            hex(3),
            hex(4)
        )
        .into_bytes()
    }

    fn parsed(bytes: &[u8]) -> Result<Manifest, Error> {
        parse(bytes, &sha256_of(bytes), BOUNDS)
    }

    fn refused(bytes: &[u8]) -> String {
        parsed(bytes).expect_err("refused").to_string()
    }

    /// Catches a manifest read wrongly: the three layer types, their order and sizes,
    /// and the config.
    #[test]
    fn a_manifest_gives_its_config_and_layers_in_order() {
        let m = parsed(&manifest("")).expect("parses");
        assert_eq!(m.config, (hex(1), 10));
        let kinds: Vec<_> = m
            .layers
            .iter()
            .map(|l| (l.digest.clone(), l.size, l.compression))
            .collect();
        assert_eq!(
            kinds,
            [
                (hex(2), 20, Compression::Gzip),
                (hex(3), 30, Compression::Gzip),
                (hex(4), 40, Compression::None)
            ]
        );
    }

    /// Catches bytes that are not the pinned manifest being believed (the "wrong
    /// manifest digest accepted" mutant).
    #[test]
    fn bytes_that_do_not_hash_to_the_pin_are_refused() {
        let bytes = manifest("");
        let other = hex(9);
        let why = parse(&bytes, &other, BOUNDS)
            .expect_err("refused")
            .to_string();
        assert!(why.contains(&other), "{why}");
        assert!(why.contains(&sha256_of(&bytes)), "{why}");
    }

    /// Catches an index being taken for an image: by its media type, by its list of
    /// manifests, and the Docker spelling.
    #[test]
    fn an_index_is_refused() {
        for text in [
            r#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.index.v1+json","manifests":[]}"#,
            r#"{"schemaVersion":2,"manifests":[]}"#,
            r#"{"schemaVersion":2,"mediaType":"application/vnd.docker.distribution.manifest.list.v2+json"}"#,
        ] {
            let bytes = text.as_bytes();
            let err = parsed(bytes).expect_err("refused");
            assert!(matches!(err, Error::Index(_)), "{text}: {err}");
        }
    }

    /// Catches manifests this store cannot take being accepted or described wrongly.
    #[test]
    fn unsupported_manifests_say_what_is_unsupported() {
        let layer = |media: &str, extra: &str| {
            format!(
                r#"{{"schemaVersion":2,"config":{{"mediaType":"{OCI_CONFIG}","digest":"{}","size":1}},
                "layers":[{{"mediaType":"{media}","digest":"{}","size":1{extra}}}]}}"#,
                hex(1),
                hex(2)
            )
        };
        let zstd = refused(layer("application/vnd.oci.image.layer.v1.tar+zstd", "").as_bytes());
        assert!(zstd.contains("zstd"), "{zstd}");
        let foreign = refused(
            layer(
                "application/vnd.docker.image.rootfs.foreign.diff.tar.gzip",
                "",
            )
            .as_bytes(),
        );
        assert!(foreign.contains("foreign"), "{foreign}");
        let urls = refused(
            layer(
                "application/vnd.oci.image.layer.v1.tar+gzip",
                r#","urls":["https://elsewhere.invalid/blob"]"#,
            )
            .as_bytes(),
        );
        assert!(urls.contains("foreign"), "{urls}");
        let other = refused(layer("text/plain", "").as_bytes());
        assert!(other.contains("text/plain"), "{other}");
        let v1 = refused(br#"{"schemaVersion":1,"layers":[]}"#);
        assert!(v1.contains("schema version"), "{v1}");
        let media = refused(br#"{"schemaVersion":2,"mediaType":"text/plain"}"#);
        assert!(media.contains("media type"), "{media}");
        let config = refused(
            format!(
                r#"{{"schemaVersion":2,"config":{{"mediaType":"text/plain","digest":"{}","size":1}},"layers":[]}}"#,
                hex(1)
            )
            .as_bytes(),
        );
        assert!(config.contains("config has media type"), "{config}");
    }

    /// Catches a malformed manifest passing: not JSON, no config, no layers, a digest
    /// that is not safe as a file name.
    #[test]
    fn malformed_manifests_are_refused() {
        assert!(refused(b"not json").contains("not a manifest"));
        assert!(refused(br#"{"schemaVersion":2,"layers":[]}"#).contains("no config"));
        let config = format!(
            r#"{{"schemaVersion":2,"config":{{"mediaType":"{OCI_CONFIG}","digest":"{}","size":1}}"#,
            hex(1)
        );
        assert!(refused(format!("{config}}}").as_bytes()).contains("no layers"));
        let traversal = format!(
            r#"{config},"layers":[{{"mediaType":"application/vnd.oci.image.layer.v1.tar","digest":"sha256:../x","size":1}}]}}"#
        );
        assert!(refused(traversal.as_bytes()).contains("not sha256"));
        let upper = format!(
            r#"{config},"layers":[{{"mediaType":"application/vnd.oci.image.layer.v1.tar","digest":"sha256:{}","size":1}}]}}"#,
            "A".repeat(64)
        );
        assert!(refused(upper.as_bytes()).contains("not sha256"));
    }

    /// Catches the bounds not holding: too many layers, too many bytes.
    #[test]
    fn a_manifest_beyond_the_bounds_is_refused() {
        let bytes = manifest("");
        let few = Bounds {
            layers: 2,
            bytes: 1000,
        };
        let why = parse(&bytes, &sha256_of(&bytes), few)
            .expect_err("layers")
            .to_string();
        assert!(why.contains("3 layers"), "{why}");
        let small = Bounds {
            layers: 4,
            bytes: 99,
        };
        let why = parse(&bytes, &sha256_of(&bytes), small)
            .expect_err("bytes")
            .to_string();
        assert!(why.contains("99"), "{why}");
        // The sum is what counts: 10 + 20 + 30 + 40.
        let exact = Bounds {
            layers: 4,
            bytes: 100,
        };
        assert!(parse(&bytes, &sha256_of(&bytes), exact).is_ok());
    }
}
