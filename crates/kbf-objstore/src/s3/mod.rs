//! An [`ObjectStore`] over the S3 REST API: path-style addressing, SigV4 signing,
//! plain HTTP.
//!
//! Written against S3 as MinIO and RustFS serve it. It signs every payload (the store
//! checks the body against the signed hash), refuses a ranged read answered with
//! anything but the exact range asked for, and deletes the exact object version on a
//! store with Object Lock, so a retained object is refused rather than hidden behind a
//! delete marker.
//!
//! [`ObjectStore::put_stream`] of an object over [`MULTIPART_THRESHOLD`] is a multipart
//! upload (`multipart.rs`): parts of [`part_size`] sent one at a time, each held in
//! memory only while it is sent, and the upload aborted on any failure.
//!
//! Not built yet: TLS (an `https` endpoint is refused).

#[cfg(feature = "testkit")]
pub mod fake;
mod multipart;
mod sigv4;
mod xml;

pub use multipart::{
    MAX_PART_BYTES, MAX_PARTS, MIN_PART_BYTES, MULTIPART_THRESHOLD, PART_BYTES, part_size,
};

use std::fmt;
use std::num::NonZeroU64;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use base64::Engine as _;
use bytes::Bytes;
use md5::Md5;
use reqwest::header::{AUTHORIZATION, CONTENT_RANGE};
use reqwest::{Method, Response, StatusCode, Url};
use sha2::{Digest, Sha256};

use crate::types::Token;
use crate::{
    BodyStream, ByteRange, Capabilities, KeyPrefix, ListPage, ListToken, ObjectKey, ObjectStore,
    ObjectStoreError, PageSize,
};
use sigv4::{EMPTY_SHA256, Request, Signer, Utc};

/// How long [`S3Store::wait_answering`] waits between attempts.
pub const WAIT_RETRY: Duration = Duration::from_millis(250);

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
    /// Limit on a whole request, body included, for a request with no body or a small one.
    pub request_timeout: Duration,
    /// The slowest rate, in bytes a second, at which a request body may reach the
    /// store: a request with a body may take `request_timeout` plus the body's length
    /// at this rate. `None` gives every request `request_timeout` however large its
    /// body. A store shared with other tenants can take minutes over one large PUT; a
    /// flat limit cuts it however steadily it progresses.
    pub min_body_rate: Option<NonZeroU64>,
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
    /// Shared with a multipart upload's abort, which may run after its write was
    /// dropped.
    conn: Arc<Conn>,
}

/// What every request to the bucket needs: the client, the address, the signing key.
struct Conn {
    client: reqwest::Client,
    base: Url,
    /// `host[:port]` as the `Host` header carries it.
    host: String,
    bucket: String,
    region: String,
    credentials: Credentials,
    capabilities: Capabilities,
    request_timeout: Duration,
    min_body_rate: Option<NonZeroU64>,
}

impl fmt::Debug for S3Store {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("S3Store")
            .field("base", &self.conn.base.as_str())
            .field("bucket", &self.conn.bucket)
            .field("capabilities", &self.conn.capabilities)
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
        let conn = Conn {
            client,
            base,
            host,
            bucket: config.bucket,
            region: config.region,
            credentials: config.credentials,
            capabilities: config.capabilities,
            request_timeout: config.request_timeout,
            min_body_rate: config.min_body_rate,
        };
        Ok(Self {
            conn: Arc::new(conn),
        })
    }
}

impl Conn {
    /// The limit on a whole request with a `body_len`-byte body.
    fn timeout_for(&self, body_len: usize) -> Duration {
        let Some(rate) = self.min_body_rate else {
            return self.request_timeout;
        };
        let nanos = (body_len as u128).saturating_mul(1_000_000_000) / u128::from(rate.get());
        self.request_timeout.saturating_add(Duration::from_nanos(
            u64::try_from(nanos).unwrap_or(u64::MAX),
        ))
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
        req = req.timeout(self.timeout_for(body.len()));
        if !body.is_empty() {
            req = req.body(body);
        }
        req.send().await.map_err(transport)
    }
}

impl S3Store {
    /// Creates the bucket, with Object Lock if the store claims it. Succeeds if this
    /// key pair already owns the bucket. For setup and tests; kbf itself never calls it.
    pub async fn create_bucket(&self) -> Result<(), ObjectStoreError> {
        let mut headers = Vec::new();
        if self.conn.capabilities.object_lock {
            headers.push(("x-amz-bucket-object-lock-enabled", "true".to_owned()));
        }
        let resp = self
            .conn
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

    /// Waits, for at most `within`, until the service answers an HTTP request. Any
    /// answer counts, whatever its status: a running S3 service answers this unsigned
    /// `GET /` with an error such as 403. Only a request that fails to complete (refused,
    /// reset, timed out) is retried, every [`WAIT_RETRY`]. On the deadline it returns
    /// the last attempt's error. For setup and tests, to wait for a store that is
    /// starting; kbf itself never calls it.
    pub async fn wait_answering(&self, within: Duration) -> Result<(), ObjectStoreError> {
        let deadline = Instant::now() + within;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self
                .conn
                .client
                .get(self.conn.base.clone())
                .timeout(left)
                .send()
                .await
            {
                Ok(_) => return Ok(()),
                Err(e) => {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return Err(transport(e));
                    }
                    tokio::time::sleep(left.min(WAIT_RETRY)).await;
                }
            }
        }
    }
}

impl ObjectStore for S3Store {
    fn capabilities(&self) -> Capabilities {
        self.conn.capabilities
    }

    async fn put_new(
        &self,
        key: &ObjectKey,
        body: Bytes,
        retain_until: Option<SystemTime>,
    ) -> Result<(), ObjectStoreError> {
        let mut headers = Vec::new();
        if self.conn.capabilities.conditional_put {
            headers.push(("if-none-match", "*".to_owned()));
        }
        if let Some(until) = retain_until {
            if !self.conn.capabilities.object_lock {
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
            .conn
            .send(Method::PUT, Some(key), &[], headers, body)
            .await?;
        match resp.status() {
            StatusCode::OK => Ok(()),
            StatusCode::PRECONDITION_FAILED if self.conn.capabilities.conditional_put => {
                Err(ObjectStoreError::AlreadyExists(key.clone()))
            }
            _ => Err(failure(resp).await),
        }
    }

    /// One PUT for a body of at most [`MULTIPART_THRESHOLD`] bytes, gathered first; a
    /// multipart upload for a larger one, aborted on any failure.
    async fn put_stream(
        &self,
        key: &ObjectKey,
        len: u64,
        body: BodyStream,
    ) -> Result<(), ObjectStoreError> {
        if len <= MULTIPART_THRESHOLD {
            let bytes = crate::gather(body, len).await?;
            return self.put_new(key, bytes, None).await;
        }
        multipart::put(&self.conn, key, len, body).await
    }

    async fn get_range(
        &self,
        key: &ObjectKey,
        range: ByteRange,
    ) -> Result<Bytes, ObjectStoreError> {
        let header = format!("bytes={}-{}", range.offset(), range.last());
        let resp = self
            .conn
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
        if self.conn.capabilities.object_lock {
            // A bucket with Object Lock is versioned: a plain DELETE would succeed by
            // adding a delete marker and keep the bytes. Delete the current version
            // itself, which the store refuses while it is retained.
            let head = self
                .conn
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
            .conn
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
        match after.map(|t| &t.0) {
            Some(Token::Continue(t)) => query.push(("continuation-token", t.clone())),
            Some(Token::After(key) | Token::Kept { after: key, .. }) => {
                query.push(("start-after", key.as_str().to_owned()))
            }
            None => {}
        }
        let resp = self
            .conn
            .send(Method::GET, None, &query, Vec::new(), Bytes::new())
            .await?;
        if resp.status() != StatusCode::OK {
            return Err(failure(resp).await);
        }
        let body = resp.bytes().await.map_err(transport)?;
        xml::list_page(&body)
    }
}

/// A transport failure, without the request URL: the text reaches REAPI clients (a CAS
/// `Unavailable` status), and the URL names the store's address, bucket and key. The
/// kind and causes ("operation timed out", "connection reset") stay.
fn transport(e: reqwest::Error) -> ObjectStoreError {
    ObjectStoreError::Transport(std::sync::Arc::new(e.without_url()))
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

    /// Catches: a timeout that ignores the body (the flat 60 s that cut a 128 MiB PUT
    /// on a slow store), one that scales a body-less request, or arithmetic that
    /// overflows on a huge body or a rate of one byte a second.
    #[test]
    fn the_timeout_grows_with_the_body_at_the_floor_rate() {
        let mut c = config("http://127.0.0.1:9000", "kbf-test");
        c.request_timeout = Duration::from_secs(60);
        let flat = S3Store::new(c.clone()).unwrap();
        assert_eq!(flat.conn.timeout_for(128 << 20), Duration::from_secs(60));
        c.min_body_rate = NonZeroU64::new(1 << 20);
        let scaled = S3Store::new(c.clone()).unwrap();
        assert_eq!(scaled.conn.timeout_for(0), Duration::from_secs(60));
        assert_eq!(scaled.conn.timeout_for(128 << 20), Duration::from_secs(188));
        assert_eq!(
            scaled.conn.timeout_for(512 << 10),
            Duration::from_millis(60_500)
        );
        c.min_body_rate = NonZeroU64::new(1);
        let slowest = S3Store::new(c).unwrap();
        let saturated = Duration::from_secs(60) + Duration::from_nanos(u64::MAX);
        assert_eq!(slowest.conn.timeout_for(usize::MAX), saturated);
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
            min_body_rate: None,
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
