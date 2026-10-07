//! An [`ObjectStore`] over the S3 REST API: path-style addressing, SigV4 signing,
//! plain HTTP.
//!
//! Written against S3 as MinIO and RustFS serve it. It signs every payload (the store
//! checks the body against the signed hash), refuses a ranged read answered with
//! anything but the exact range asked for, and deletes the exact object version on a
//! store with Object Lock, so a retained object is refused rather than hidden behind a
//! delete marker.
//!
//! Not built yet: TLS (an `https` endpoint is refused) and multipart upload (a single
//! PUT carries up to 5 GiB; kbf segments are at most 128 MiB).

mod sigv4;
mod xml;

use std::fmt;
use std::time::{Duration, SystemTime};

use base64::Engine as _;
use bytes::Bytes;
use md5::Md5;
use reqwest::header::{AUTHORIZATION, CONTENT_RANGE};
use reqwest::{Method, Response, StatusCode, Url};
use sha2::{Digest, Sha256};

use crate::{
    ByteRange, Capabilities, KeyPrefix, ListPage, ListToken, ObjectKey, ObjectStore,
    ObjectStoreError, PageSize,
};
use sigv4::{EMPTY_SHA256, Request, Signer, Utc};

/// An access key pair. `Debug` never shows the secret.
#[derive(Clone)]
pub struct Credentials {
    access_key_id: String,
    secret_access_key: String,
}

impl Credentials {
    /// A key pair.
    #[must_use]
    pub fn new(access_key_id: impl Into<String>, secret_access_key: impl Into<String>) -> Self {
        Self {
            access_key_id: access_key_id.into(),
            secret_access_key: secret_access_key.into(),
        }
    }
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credentials")
            .field("access_key_id", &self.access_key_id)
            .field("secret_access_key", &"<redacted>")
            .finish()
    }
}

/// Where and how to reach one bucket.
#[derive(Clone, Debug)]
pub struct S3Config {
    /// `http://host[:port]`, with no path.
    pub endpoint: String,
    /// The signing region (MinIO and RustFS accept `us-east-1` unless configured).
    pub region: String,
    /// The bucket this store addresses.
    pub bucket: String,
    /// The key pair requests are signed with.
    pub credentials: Credentials,
    /// What the bucket supports, as the operator declares it; the conformance suite
    /// checks the declaration. `object_lock` needs a bucket created with Object Lock.
    pub capabilities: Capabilities,
    /// Limit on opening a connection.
    pub connect_timeout: Duration,
    /// Limit on a whole request, body included.
    pub request_timeout: Duration,
}

/// Why an [`S3Config`] was refused.
#[derive(Debug, thiserror::Error)]
pub enum S3ConfigError {
    /// The endpoint is not `http://host[:port]`.
    #[error("endpoint {endpoint:?}: {why}")]
    Endpoint {
        /// The refused endpoint.
        endpoint: String,
        /// What is wrong with it.
        why: &'static str,
    },
    /// The bucket name is not a valid S3 bucket name.
    #[error(
        "bucket name {0:?} must be 3 to 63 of a-z 0-9 . - and start and end with a letter or digit"
    )]
    Bucket(String),
    /// The HTTP client could not be built.
    #[error("building the HTTP client")]
    Client(#[source] reqwest::Error),
}

/// One S3 bucket.
pub struct S3Store {
    client: reqwest::Client,
    base: Url,
    /// `host[:port]` as the `Host` header carries it.
    host: String,
    bucket: String,
    region: String,
    credentials: Credentials,
    capabilities: Capabilities,
}

impl fmt::Debug for S3Store {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("S3Store")
            .field("base", &self.base.as_str())
            .field("bucket", &self.bucket)
            .field("capabilities", &self.capabilities)
            .finish_non_exhaustive()
    }
}

impl S3Store {
    /// A store for `config.bucket`. Does no I/O.
    pub fn new(config: S3Config) -> Result<Self, S3ConfigError> {
        let bad = |why| S3ConfigError::Endpoint {
            endpoint: config.endpoint.clone(),
            why,
        };
        let base = Url::parse(&config.endpoint).map_err(|_| bad("not a URL"))?;
        match base.scheme() {
            "http" => {}
            "https" => return Err(bad("TLS is not built yet; use http")),
            _ => return Err(bad("the scheme must be http")),
        }
        if !base.username().is_empty() || base.password().is_some() {
            return Err(bad("credentials belong in `credentials`, not the URL"));
        }
        if base.path() != "/" || base.query().is_some() || base.fragment().is_some() {
            return Err(bad("path-style addressing needs an endpoint with no path"));
        }
        let host = match (base.host_str(), base.port()) {
            (Some(h), Some(p)) => format!("{h}:{p}"),
            (Some(h), None) => h.to_owned(),
            (None, _) => return Err(bad("no host")),
        };
        if !valid_bucket(&config.bucket) {
            return Err(S3ConfigError::Bucket(config.bucket));
        }
        let client = reqwest::Client::builder()
            .connect_timeout(config.connect_timeout)
            .timeout(config.request_timeout)
            .build()
            .map_err(S3ConfigError::Client)?;
        Ok(Self {
            client,
            base,
            host,
            bucket: config.bucket,
            region: config.region,
            credentials: config.credentials,
            capabilities: config.capabilities,
        })
    }

    /// Creates the bucket, with Object Lock if the store claims it. Succeeds if this
    /// key pair already owns the bucket. For setup and tests; kbf itself never calls it.
    pub async fn create_bucket(&self) -> Result<(), ObjectStoreError> {
        let mut headers = Vec::new();
        if self.capabilities.object_lock {
            headers.push(("x-amz-bucket-object-lock-enabled", "true".to_owned()));
        }
        let resp = self
            .send(Method::PUT, None, &[], headers, Bytes::new())
            .await?;
        match resp.status() {
            StatusCode::OK => Ok(()),
            _ => match failure(resp).await {
                ObjectStoreError::Service { code, .. } if code == "BucketAlreadyOwnedByYou" => {
                    Ok(())
                }
                e => Err(e),
            },
        }
    }

    /// Signs and sends one request. `key` of `None` addresses the bucket.
    async fn send(
        &self,
        method: Method,
        key: Option<&ObjectKey>,
        query: &[(&str, String)],
        mut headers: Vec<(&'static str, String)>,
        body: Bytes,
    ) -> Result<Response, ObjectStoreError> {
        // `ObjectKey` holds only unreserved characters and `/`, so the path needs no
        // encoding and is signed exactly as sent.
        let path = match key {
            Some(k) => format!("/{}/{}", self.bucket, k.as_str()),
            None => format!("/{}", self.bucket),
        };
        // PERF: hashing a 128 MiB segment takes a fraction of a second on this task's
        // thread; move it off the runtime if upload latency shows it.
        let payload = if body.is_empty() {
            EMPTY_SHA256.to_owned()
        } else {
            hex::encode(Sha256::digest(&body))
        };
        let amz_date = Utc::of(SystemTime::now(), false).amz();
        headers.push(("x-amz-content-sha256", payload.clone()));
        headers.push(("x-amz-date", amz_date.clone()));
        let authorization = Signer {
            access_key_id: &self.credentials.access_key_id,
            secret_access_key: &self.credentials.secret_access_key,
            region: &self.region,
        }
        .authorization(
            &Request {
                method: method.as_str(),
                host: &self.host,
                path: &path,
                query,
                headers: &headers,
                payload_sha256: &payload,
            },
            &amz_date,
        );

        let mut url = self.base.clone();
        url.set_path(&path);
        let q = sigv4::canonical_query(query);
        url.set_query((!q.is_empty()).then_some(q.as_str()));
        let mut req = self
            .client
            .request(method, url)
            .header(AUTHORIZATION, authorization);
        for (name, value) in headers {
            req = req.header(name, value);
        }
        if !body.is_empty() {
            req = req.body(body);
        }
        req.send().await.map_err(transport)
    }
}

impl ObjectStore for S3Store {
    fn capabilities(&self) -> Capabilities {
        self.capabilities
    }

    async fn put_new(
        &self,
        key: &ObjectKey,
        body: Bytes,
        retain_until: Option<SystemTime>,
    ) -> Result<(), ObjectStoreError> {
        let mut headers = Vec::new();
        if self.capabilities.conditional_put {
            headers.push(("if-none-match", "*".to_owned()));
        }
        if let Some(until) = retain_until {
            if !self.capabilities.object_lock {
                return Err(ObjectStoreError::Unsupported("Object Lock retention"));
            }
            headers.push(("x-amz-object-lock-mode", "COMPLIANCE".to_owned()));
            headers.push((
                "x-amz-object-lock-retain-until-date",
                Utc::of(until, true).iso8601(),
            ));
            // S3 requires a body checksum on a PUT that sets retention.
            headers.push((
                "content-md5",
                base64::engine::general_purpose::STANDARD.encode(Md5::digest(&body)),
            ));
        }
        let resp = self
            .send(Method::PUT, Some(key), &[], headers, body)
            .await?;
        match resp.status() {
            StatusCode::OK => Ok(()),
            StatusCode::PRECONDITION_FAILED if self.capabilities.conditional_put => {
                Err(ObjectStoreError::AlreadyExists(key.clone()))
            }
            _ => Err(failure(resp).await),
        }
    }

    async fn get_range(
        &self,
        key: &ObjectKey,
        range: ByteRange,
    ) -> Result<Bytes, ObjectStoreError> {
        let header = format!("bytes={}-{}", range.offset(), range.last());
        let resp = self
            .send(
                Method::GET,
                Some(key),
                &[],
                vec![("range", header)],
                Bytes::new(),
            )
            .await?;
        match resp.status() {
            StatusCode::PARTIAL_CONTENT => {}
            StatusCode::OK => {
                return Err(ObjectStoreError::Protocol(format!(
                    "ranged read of {key} {range:?} answered 200: the store ignored Range"
                )));
            }
            StatusCode::NOT_FOUND => return Err(ObjectStoreError::NotFound(key.clone())),
            StatusCode::RANGE_NOT_SATISFIABLE => {
                return Err(ObjectStoreError::InvalidRange {
                    key: key.clone(),
                    range,
                });
            }
            _ => return Err(failure(resp).await),
        }
        let content_range = resp
            .headers()
            .get(CONTENT_RANGE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let body = resp.bytes().await.map_err(transport)?;
        check_partial(content_range.as_deref(), range, body.len())
            .map_err(|why| ObjectStoreError::Protocol(format!("ranged read of {key}: {why}")))?;
        Ok(body)
    }

    async fn delete(&self, key: &ObjectKey) -> Result<(), ObjectStoreError> {
        let mut query = Vec::new();
        if self.capabilities.object_lock {
            // A bucket with Object Lock is versioned: a plain DELETE would succeed by
            // adding a delete marker and keep the bytes. Delete the current version
            // itself, which the store refuses while it is retained.
            let head = self
                .send(Method::HEAD, Some(key), &[], Vec::new(), Bytes::new())
                .await?;
            match head.status() {
                StatusCode::OK => {}
                StatusCode::NOT_FOUND => return Ok(()),
                _ => return Err(failure(head).await),
            }
            if let Some(v) = head
                .headers()
                .get("x-amz-version-id")
                .and_then(|v| v.to_str().ok())
                .filter(|v| !v.is_empty() && *v != "null")
            {
                query.push(("versionId", v.to_owned()));
            }
        }
        let resp = self
            .send(Method::DELETE, Some(key), &query, Vec::new(), Bytes::new())
            .await?;
        match resp.status() {
            StatusCode::NO_CONTENT | StatusCode::OK | StatusCode::NOT_FOUND => Ok(()),
            _ => Err(failure(resp).await),
        }
    }

    async fn list(
        &self,
        prefix: &KeyPrefix,
        after: Option<&ListToken>,
        max_keys: PageSize,
    ) -> Result<ListPage, ObjectStoreError> {
        let mut query = vec![
            ("list-type", "2".to_owned()),
            ("prefix", prefix.as_str().to_owned()),
            ("max-keys", max_keys.get().to_string()),
        ];
        if let Some(t) = after {
            query.push(("continuation-token", t.0.clone()));
        }
        let resp = self
            .send(Method::GET, None, &query, Vec::new(), Bytes::new())
            .await?;
        if resp.status() != StatusCode::OK {
            return Err(failure(resp).await);
        }
        let body = resp.bytes().await.map_err(transport)?;
        xml::list_page(&body)
    }
}

fn transport(e: reqwest::Error) -> ObjectStoreError {
    ObjectStoreError::Transport(Box::new(e))
}

/// The error a non-success answer carries.
async fn failure(resp: Response) -> ObjectStoreError {
    let status = resp.status().as_u16();
    let body = resp.bytes().await.unwrap_or_default();
    let (code, message) = xml::error_code_message(&body);
    ObjectStoreError::Service {
        status,
        code,
        message,
    }
}

/// Checks a 206 answer: `Content-Range` must name exactly the bytes asked for (cut at
/// the end of the object) and the body must be that long.
fn check_partial(
    content_range: Option<&str>,
    range: ByteRange,
    body_len: usize,
) -> Result<(), String> {
    let cr = content_range.ok_or("206 without Content-Range")?;
    let parsed = cr.strip_prefix("bytes ").and_then(|rest| {
        let (span, total) = rest.split_once('/')?;
        let (first, last) = span.split_once('-')?;
        Some((
            first.parse::<u64>().ok()?,
            last.parse::<u64>().ok()?,
            total.parse::<u64>().ok()?,
        ))
    });
    let Some((first, last, total)) = parsed else {
        return Err(format!("Content-Range {cr:?} does not parse"));
    };
    let want_last = range.last().min(total.saturating_sub(1));
    if first != range.offset() || last != want_last || last < first {
        return Err(format!(
            "asked for bytes {}-{} and got Content-Range {cr:?}",
            range.offset(),
            range.last()
        ));
    }
    if body_len as u64 != last - first + 1 {
        return Err(format!(
            "Content-Range {cr:?} but the body has {body_len} bytes"
        ));
    }
    Ok(())
}

fn valid_bucket(b: &str) -> bool {
    let edge_ok = |c: Option<char>| c.is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    (3..=63).contains(&b.len())
        && b.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '-')
        && edge_ok(b.chars().next())
        && edge_ok(b.chars().last())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(offset: u64, len: u64) -> ByteRange {
        ByteRange::new(offset, len).unwrap()
    }

    /// Catches: accepting a 206 for different bytes than asked (a store that rounds a
    /// range to its own block size), a body shorter than its header says, or a range cut
    /// at the end refused.
    #[test]
    fn partial_answers_must_match_the_range() {
        assert!(check_partial(Some("bytes 10-19/100"), r(10, 10), 10).is_ok());
        assert!(check_partial(Some("bytes 95-99/100"), r(95, 10), 5).is_ok());
        assert!(check_partial(Some("bytes 0-19/100"), r(10, 10), 20).is_err());
        assert!(check_partial(Some("bytes 10-19/100"), r(10, 10), 9).is_err());
        assert!(check_partial(Some("bytes 10-29/100"), r(10, 10), 20).is_err());
        assert!(check_partial(Some("bytes */100"), r(10, 10), 0).is_err());
        assert!(check_partial(None, r(0, 1), 1).is_err());
    }

    fn config(endpoint: &str, bucket: &str) -> S3Config {
        S3Config {
            endpoint: endpoint.to_owned(),
            region: "us-east-1".to_owned(),
            bucket: bucket.to_owned(),
            credentials: Credentials::new("a", "s"),
            capabilities: Capabilities::default(),
            connect_timeout: Duration::from_secs(1),
            request_timeout: Duration::from_secs(1),
        }
    }

    /// Catches: an endpoint whose path, scheme or credentials would make the signed
    /// request differ from the one sent, and bucket names S3 refuses.
    #[test]
    fn config_is_checked() {
        assert!(S3Store::new(config("http://127.0.0.1:9000", "kbf-cas")).is_ok());
        for bad in [
            "https://127.0.0.1:9000",
            "ftp://127.0.0.1",
            "http://127.0.0.1:9000/base",
            "http://u:p@127.0.0.1:9000",
            "http://127.0.0.1:9000/?x=1",
            "127.0.0.1:9000",
        ] {
            assert!(
                matches!(
                    S3Store::new(config(bad, "kbf-cas")),
                    Err(S3ConfigError::Endpoint { .. })
                ),
                "{bad}"
            );
        }
        for bad in ["ab", "Kbf", "-kbf", "kbf-", "kbf_cas", &"a".repeat(64)] {
            assert!(
                matches!(
                    S3Store::new(config("http://127.0.0.1", bad)),
                    Err(S3ConfigError::Bucket(_))
                ),
                "{bad}"
            );
        }
    }

    /// Catches: a secret printed by `Debug` into a log.
    #[test]
    fn debug_hides_the_secret() {
        let text = format!("{:?}", config("http://127.0.0.1", "kbf-cas"));
        assert!(
            text.contains("<redacted>") && !text.contains("\"s\""),
            "{text}"
        );
    }
}
