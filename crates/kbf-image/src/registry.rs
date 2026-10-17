//! Fetching an image from its registry into a directory of verified files.

use std::path::{Path, PathBuf};

use hyper::header::{ACCEPT, AUTHORIZATION, HeaderName, WWW_AUTHENTICATE};
use kbf_types::ImageRef;
use rustls::pki_types::CertificateDer;
use serde::Deserialize;
use sha2::{Digest as _, Sha256};
use tokio::io::AsyncWriteExt as _;

pub use crate::http::Timeouts;
use crate::http::{Http, Roots};
use crate::manifest::{self, Bounds, Compression, Manifest};
use crate::{Error, Location};

/// What one fetch may write or read, so a wrong pin or a hostile registry cannot fill
/// the disk or the memory of a node.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// The manifest, in bytes.
    pub manifest: u64,
    /// The config blob, in bytes.
    pub config: u64,
    /// The layers of an image.
    pub layers: usize,
    /// The sum of an image's blob sizes, as its manifest declares them.
    pub image_bytes: u64,
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            manifest: 4 << 20,
            config: 1 << 20,
            layers: 128,
            image_bytes: 16 << 30,
        }
    }
}

/// The platform of this node, as image configs name it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Platform {
    pub os: &'static str,
    pub architecture: &'static str,
}

impl Platform {
    /// The Linux platform of Rust architecture `arch` (`std::env::consts::ARCH`).
    ///
    /// # Errors
    /// The architecture has no image platform name here.
    pub fn of(arch: &str) -> Result<Self, Error> {
        let architecture = match arch {
            "x86_64" => "amd64",
            "aarch64" => "arm64",
            other => return Err(Error::Arch(other.to_owned())),
        };
        Ok(Self {
            os: "linux",
            architecture,
        })
    }
}

/// A verified layer blob in the directory a fetch wrote.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LayerFile {
    /// The file's name in the directory: `<index>.tar` or `<index>.tar.gz`.
    pub name: String,
    pub compression: Compression,
}

/// A registry client.
#[derive(Debug)]
pub struct Registry {
    http: Http,
    limits: Limits,
    /// The Rust name of the architecture images must be for.
    arch: String,
}

impl Registry {
    /// A client that trusts the host's system roots and speaks https only, for images
    /// of this machine's architecture.
    #[must_use]
    pub fn new() -> Self {
        Self::build(Roots::System)
    }

    /// A client that trusts exactly `roots`, for a registry with a private authority.
    #[must_use]
    pub fn with_roots(roots: Vec<CertificateDer<'static>>) -> Self {
        Self::build(Roots::Given(roots))
    }

    fn build(roots: Roots) -> Self {
        Self {
            http: Http::new(roots),
            limits: Limits::default(),
            arch: std::env::consts::ARCH.to_owned(),
        }
    }

    /// Also speaks plain HTTP to the registry `authority` (`host:port`). Only for a
    /// registry on a loopback address or a test: the daemon never calls this.
    #[must_use]
    pub fn allow_plain_http(mut self, authority: &str) -> Self {
        self.http.allow_plain(authority.to_owned());
        self
    }

    #[must_use]
    pub fn with_limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    #[must_use]
    pub fn with_timeouts(mut self, timeouts: Timeouts) -> Self {
        self.http.timeouts = timeouts;
        self
    }

    /// Fetches images for the architecture Rust names `arch` (`aarch64`, `x86_64`), not
    /// this machine's.
    #[must_use]
    pub fn with_arch(mut self, arch: &str) -> Self {
        arch.clone_into(&mut self.arch);
        self
    }

    /// Fetches `image` into `dir` (which exists and is empty or private to the caller):
    /// each layer as `<index>.tar[.gz]`, written only once its bytes hash to the
    /// manifest's digest and match its size. Returns the layers, lowest first.
    ///
    /// # Errors
    /// The registry cannot be reached or refuses, the manifest is not the pinned one or
    /// is not one image, a blob fails its digest or size, the image is for another
    /// platform, a limit is passed, or a file cannot be written. Files written before
    /// the failure are removed.
    pub async fn fetch(&self, image: &ImageRef, dir: &Path) -> Result<Vec<LayerFile>, Error> {
        let platform = Platform::of(&self.arch)?;
        let location = Location::of(image)?;
        let mut session = Session {
            http: &self.http,
            scheme: if self.http.is_plain(&location.endpoint) {
                "http"
            } else {
                "https"
            },
            location,
            token: None,
        };
        let bounds = Bounds {
            layers: self.limits.layers,
            bytes: self.limits.image_bytes,
        };
        let manifest = session
            .manifest(image.digest(), self.limits.manifest, bounds)
            .await?;
        let config = session
            .blob_bytes(&manifest.config.0, manifest.config.1, self.limits.config)
            .await?;
        check_platform(&platform, &config)?;
        let mut files: Vec<LayerFile> = Vec::new();
        for (index, layer) in manifest.layers.iter().enumerate() {
            let suffix = match layer.compression {
                Compression::None => "tar",
                Compression::Gzip => "tar.gz",
            };
            let name = format!("{index}.{suffix}");
            let fetched = session
                .blob_file(&layer.digest, layer.size, &dir.join(&name))
                .await;
            if let Err(e) = fetched {
                for written in &files {
                    let _ = tokio::fs::remove_file(dir.join(&written.name)).await;
                }
                return Err(e);
            }
            files.push(LayerFile {
                name,
                compression: layer.compression,
            });
        }
        Ok(files)
    }
}

/// Refuses a config whose stated platform is not `platform`. A config that states none is
/// accepted: the digest is what pins the image.
fn check_platform(platform: &Platform, config: &[u8]) -> Result<(), Error> {
    #[derive(Deserialize)]
    struct Config {
        os: Option<String>,
        architecture: Option<String>,
    }
    let config: Config = serde_json::from_slice(config)
        .map_err(|e| Error::Malformed(format!("the image config is not JSON: {e}")))?;
    let os = config.os.as_deref().unwrap_or(platform.os);
    let arch = config
        .architecture
        .as_deref()
        .unwrap_or(platform.architecture);
    if os != platform.os || arch != platform.architecture {
        return Err(Error::Platform {
            want: format!("{}/{}", platform.os, platform.architecture),
            have: format!("{os}/{arch}"),
        });
    }
    Ok(())
}

/// One image's requests: its registry address and the token the registry gave.
struct Session<'a> {
    http: &'a Http,
    scheme: &'static str,
    location: Location,
    token: Option<String>,
}

impl Session<'_> {
    fn url(&self, kind: &str, reference: &str) -> String {
        format!(
            "{}://{}/v2/{}/{kind}/{reference}",
            self.scheme, self.location.endpoint, self.location.repository
        )
    }

    /// The manifest pinned by `digest`, verified and read.
    async fn manifest(
        &mut self,
        digest: &str,
        limit: u64,
        bounds: Bounds,
    ) -> Result<Manifest, Error> {
        let url = self.url("manifests", digest);
        let response = self.get(&url, Some(manifest::ACCEPT)).await?;
        let bytes = self.http.read_limited(&url, response, limit).await?;
        manifest::parse(&bytes, digest, bounds)
    }

    /// The blob `digest` in memory, at most `limit` bytes, checked against `digest` and
    /// the manifest's `size`.
    async fn blob_bytes(&mut self, digest: &str, size: u64, limit: u64) -> Result<Vec<u8>, Error> {
        let url = self.url("blobs", digest);
        let response = self.get(&url, None).await?;
        let bytes = self
            .http
            .read_limited(&url, response, limit.min(size))
            .await?;
        let got = bytes.len() as u64;
        if got != size {
            return Err(Error::BlobSize {
                digest: digest.to_owned(),
                declared: size,
                got,
            });
        }
        let actual = manifest::sha256_of(&bytes);
        if actual != digest {
            return Err(Error::BlobDigest {
                digest: digest.to_owned(),
                actual,
            });
        }
        Ok(bytes)
    }

    /// The blob `digest` into the file `to`, hashed as it is written and removed again
    /// if it is not `size` bytes or does not hash to `digest`.
    async fn blob_file(&mut self, digest: &str, size: u64, to: &Path) -> Result<(), Error> {
        let result = self.blob_to(digest, size, to).await;
        if result.is_err() {
            let _ = tokio::fs::remove_file(to).await;
        }
        result
    }

    async fn blob_to(&mut self, digest: &str, size: u64, to: &Path) -> Result<(), Error> {
        let io = |path: &Path| {
            let path = PathBuf::from(path);
            move |source| Error::Io { path, source }
        };
        let url = self.url("blobs", digest);
        let response = self.get(&url, None).await?;
        let mut body = response.into_body();
        let mut file = tokio::fs::File::create(to).await.map_err(io(to))?;
        let mut hasher = Sha256::new();
        let mut got = 0u64;
        while let Some(data) = self.http.chunk(&url, &mut body).await? {
            got += data.len() as u64;
            if got > size {
                return Err(Error::BlobSize {
                    digest: digest.to_owned(),
                    declared: size,
                    got,
                });
            }
            hasher.update(&data);
            file.write_all(&data).await.map_err(io(to))?;
        }
        file.flush().await.map_err(io(to))?;
        if got != size {
            return Err(Error::BlobSize {
                digest: digest.to_owned(),
                declared: size,
                got,
            });
        }
        let actual = format!("sha256:{}", hex::encode(hasher.finalize()));
        if actual != digest {
            return Err(Error::BlobDigest {
                digest: digest.to_owned(),
                actual,
            });
        }
        Ok(())
    }

    /// GETs `url`, answering a Bearer challenge once, and returns a 2xx response.
    async fn get(
        &mut self,
        url: &str,
        accept: Option<&str>,
    ) -> Result<hyper::Response<hyper::body::Incoming>, Error> {
        let mut challenged = false;
        loop {
            let mut headers: Vec<(HeaderName, String)> = Vec::new();
            if let Some(accept) = accept {
                headers.push((ACCEPT, accept.to_owned()));
            }
            if let Some(token) = &self.token {
                headers.push((AUTHORIZATION, format!("Bearer {token}")));
            }
            let response = self.http.get(url, &headers).await?;
            let status = response.status();
            if status.is_success() {
                return Ok(response);
            }
            if status != hyper::StatusCode::UNAUTHORIZED {
                return Err(Error::Status {
                    url: url.to_owned(),
                    status: status.as_u16(),
                });
            }
            if challenged {
                return Err(Error::Auth(format!(
                    "{url}: the registry refused the token it issued"
                )));
            }
            challenged = true;
            let challenge = response
                .headers()
                .get(WWW_AUTHENTICATE)
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| Error::Auth(format!("{url}: 401 with no WWW-Authenticate")))?
                .to_owned();
            self.token = Some(self.fetch_token(&challenge).await?);
        }
    }

    /// The anonymous pull token a `Bearer` challenge asks for.
    async fn fetch_token(&self, challenge: &str) -> Result<String, Error> {
        let params = Challenge::parse(challenge)?;
        let scope = params
            .scope
            .unwrap_or_else(|| format!("repository:{}:pull", self.location.repository));
        let mut url = params.realm;
        url.push(if url.contains('?') { '&' } else { '?' });
        if let Some(service) = params.service {
            url.push_str(&format!("service={}&", query_escape(&service)));
        }
        url.push_str(&format!("scope={}", query_escape(&scope)));
        let response = self.http.get(&url, &[]).await?;
        if !response.status().is_success() {
            return Err(Error::Status {
                url,
                status: response.status().as_u16(),
            });
        }
        let body = self.http.read_limited(&url, response, 64 << 10).await?;
        #[derive(Deserialize)]
        struct Token {
            token: Option<String>,
            access_token: Option<String>,
        }
        let token: Token = serde_json::from_slice(&body)
            .map_err(|e| Error::Auth(format!("{url}: the token answer is not JSON: {e}")))?;
        token
            .token
            .or(token.access_token)
            .filter(|t| !t.is_empty())
            .ok_or_else(|| Error::Auth(format!("{url}: the answer holds no token")))
    }
}

/// The parameters of a `WWW-Authenticate: Bearer` challenge.
#[derive(Debug, PartialEq, Eq)]
struct Challenge {
    realm: String,
    service: Option<String>,
    scope: Option<String>,
}

impl Challenge {
    fn parse(header: &str) -> Result<Self, Error> {
        let (scheme, rest) = header.split_once(' ').unwrap_or((header, ""));
        if !scheme.eq_ignore_ascii_case("bearer") {
            return Err(Error::Auth(format!(
                "the registry asks for {scheme:?} authentication; only anonymous Bearer tokens are supported"
            )));
        }
        let (mut realm, mut service, mut scope) = (None, None, None);
        for (key, value) in parameters(rest) {
            match key.to_ascii_lowercase().as_str() {
                "realm" => realm = Some(value),
                "service" => service = Some(value),
                "scope" => scope = Some(value),
                _ => {}
            }
        }
        let realm = realm.ok_or_else(|| Error::Auth("the challenge has no realm".to_owned()))?;
        Ok(Self {
            realm,
            service,
            scope,
        })
    }
}

/// The `key=value` and `key="quoted, value"` pairs of a challenge's parameters.
fn parameters(text: &str) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    let mut rest = text.trim_start();
    while let Some((key, after)) = rest.split_once('=') {
        let key = key.trim().trim_start_matches(',').trim();
        let (value, tail) = if let Some(quoted) = after.strip_prefix('"') {
            quoted.split_once('"').unwrap_or((quoted, ""))
        } else {
            after.split_once(',').unwrap_or((after, ""))
        };
        pairs.push((key.to_owned(), value.trim().to_owned()));
        rest = tail.trim_start().trim_start_matches(',').trim_start();
    }
    pairs
}

/// `value` as a URL query component: everything but unreserved characters and `:` `/`
/// percent-encoded.
fn query_escape(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b':' | b'/' => {
                out.push(char::from(byte));
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches a challenge read wrongly: quoted values with commas and spaces,
    /// unquoted values, any case of the scheme and keys, and what is not Bearer.
    #[test]
    fn a_bearer_challenge_is_read() {
        let hub = Challenge::parse(
            r#"Bearer realm="https://auth.example/token",service="registry.example",scope="repository:library/x:pull,push""#,
        )
        .unwrap();
        assert_eq!(hub.realm, "https://auth.example/token");
        assert_eq!(hub.service.as_deref(), Some("registry.example"));
        assert_eq!(hub.scope.as_deref(), Some("repository:library/x:pull,push"));
        let loose = Challenge::parse("bearer Realm=https://a/t, service=s").unwrap();
        assert_eq!(
            loose,
            Challenge {
                realm: "https://a/t".to_owned(),
                service: Some("s".to_owned()),
                scope: None
            }
        );
        let extra = Challenge::parse(r#"Bearer realm="r",error="invalid_token""#).unwrap();
        assert_eq!(extra.realm, "r");
        assert!(
            Challenge::parse("Basic realm=\"x\"")
                .unwrap_err()
                .to_string()
                .contains("Basic")
        );
        assert!(
            Challenge::parse("Bearer service=\"x\"")
                .unwrap_err()
                .to_string()
                .contains("realm")
        );
        assert!(
            Challenge::parse("Bearer")
                .unwrap_err()
                .to_string()
                .contains("realm")
        );
    }

    /// Catches a scope or service escaped wrongly in the token request.
    #[test]
    fn query_values_are_escaped() {
        assert_eq!(
            query_escape("repository:library/busybox:pull"),
            "repository:library/busybox:pull"
        );
        assert_eq!(query_escape("a b&c=d,é"), "a%20b%26c%3Dd%2C%C3%A9");
    }

    /// Catches a platform name wrong for a Rust architecture.
    #[test]
    fn platforms_are_named_as_images_name_them() {
        assert_eq!(Platform::of("x86_64").unwrap().architecture, "amd64");
        assert_eq!(Platform::of("aarch64").unwrap().architecture, "arm64");
        assert_eq!(Platform::of("x86_64").unwrap().os, "linux");
        assert!(matches!(Platform::of("riscv64"), Err(Error::Arch(_))));
    }
}
