//! `GET /v1/blobs/{digest}` on the operator API listener over a real listener: the
//! bytes, the headers every answer carries, and the policy that turns the route on
//! (`docs/api.md`, `docs/design/invocations.md` section 4.6).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use kbf_auth::Policy;
use kbf_front::{Cache, MemoryMetaLog, MetaLog, VerifiedBlob};
use kbf_meta::BlobAnswer;
use kbf_objstore::{ByteRange, MemoryStore, ObjectStore};
use kbf_proto::google::bytestream::ReadRequest;
use kbf_proto::google::bytestream::byte_stream_client::ByteStreamClient;
use kbf_server::http_blobs::INLINE_MAX_BYTES;
use kbf_server::{Api, Listeners, bind_server_with_policy};
use kbf_types::{Digest, FarmTime};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

type MemoryCache = Cache<MemoryMetaLog, MemoryStore>;

fn loopback() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 0))
}

/// A policy that accepts every caller and reads blobs on every instance.
const OPEN: &str = r#"{"authenticationPolicy": {"allow": {}},
    "invocations": {"getAuthorizer": {"allow": {}}}, "blobs": {"getAuthorizer": {"allow": {}}}}"#;

/// A server with the operator API in this process, over a cache the test holds.
struct Server {
    reapi: SocketAddr,
    api: SocketAddr,
    cache: Arc<MemoryCache>,
    stop: tokio::sync::oneshot::Sender<()>,
    serving: tokio::task::JoinHandle<()>,
}

impl Server {
    /// A server running `policy`.
    fn start(policy: Policy) -> Self {
        let cache: Arc<MemoryCache> = Arc::new(Cache::memory());
        let listeners = Listeners {
            reapi: loopback(),
            worker: loopback(),
            reapi_tls: None,
            worker_tls: None,
            heartbeat_interval: Duration::from_millis(200),
            hello_wait: Duration::from_secs(5),
            tick: Duration::from_millis(50),
            unservable_wait: kbf_sched::UNSERVABLE_WAIT,
            memory_ladder: kbf_server::MemoryLadder::default(),
            finished_retention: kbf_sched::FINISHED_RETENTION,
            shutdown_timeout: Duration::from_secs(10),
            streams: kbf_server::StreamTiming::default(),
            store_probe_timeout: kbf_server::health::STORE_PROBE_TIMEOUT,
            invocation_memory: kbf_front::DEFAULT_INVOCATION_MEMORY,
            segments: kbf_front::SegmentsConfig::DEFAULT,
            stream_limits: kbf_server::StreamLimits::DEFAULT,
        };
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let shutdown = async move {
            let _ = stopped.await;
        };
        let api = Api {
            listen: loopback(),
            token: None,
            local_cache: None,
            releases: None,
        };
        let bound =
            bind_server_with_policy(Arc::clone(&cache), listeners, Some(api), policy, shutdown)
                .expect("bind");
        let (reapi, api) = (bound.reapi, bound.api.expect("an API address"));
        let serving = tokio::spawn(async move { bound.serving.await.expect("serve") });
        Self {
            reapi,
            api,
            cache,
            stop,
            serving,
        }
    }

    /// A server running the policy file `json`.
    fn with(json: &str) -> Self {
        Self::start(Policy::from_json(json).expect("a policy"))
    }

    /// Shuts the server down and waits until it has stopped.
    async fn stop(self) {
        self.stop.send(()).expect("the server is running");
        self.serving.await.expect("the server stops cleanly");
    }

    /// Stores `bytes` in the cache; their digest.
    async fn store(&self, bytes: Vec<u8>) -> Digest {
        let blob = VerifiedBlob::hashed(Bytes::from(bytes));
        let digest = blob.digest();
        self.cache.store_blobs(vec![blob]).await.expect("store");
        digest
    }

    /// One `GET` of `path` with `headers` (each line ending `\r\n`).
    async fn get(&self, path: &str, headers: &str) -> Answer {
        let mut stream = TcpStream::connect(self.api).await.expect("connect");
        let request =
            format!("GET {path} HTTP/1.1\r\nHost: kbf\r\n{headers}Connection: close\r\n\r\n");
        stream.write_all(request.as_bytes()).await.expect("send");
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.expect("read");
        let end = response
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .expect("a head");
        let head = String::from_utf8(response[..end].to_vec()).expect("an ASCII head");
        let body = response[end + 4..].to_vec();
        let code = head
            .split_whitespace()
            .nth(1)
            .and_then(|c| c.parse().ok())
            .expect("a status code");
        Answer { code, head, body }
    }

    /// The blob `digest` read over ByteStream on the REAPI listener.
    async fn byte_stream(&self, digest: &Digest) -> Vec<u8> {
        let mut client = ByteStreamClient::connect(format!("http://{}", self.reapi))
            .await
            .expect("connect")
            .max_decoding_message_size(kbf_front::MAX_MESSAGE_BYTES);
        let request = ReadRequest {
            resource_name: format!("blobs/{}/{}", digest.hash_hex(), digest.size_bytes),
            ..ReadRequest::default()
        };
        let mut stream = client.read(request).await.expect("Read").into_inner();
        let mut bytes = Vec::new();
        while let Some(message) = stream.message().await.expect("a message") {
            bytes.extend_from_slice(&message.data);
        }
        bytes
    }
}

/// An HTTP answer.
struct Answer {
    code: u16,
    head: String,
    body: Vec<u8>,
}

impl Answer {
    /// The value of the one header `name` (any case), or `None`.
    fn header(&self, name: &str) -> Option<&str> {
        let mut found = self.head.lines().skip(1).filter_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.eq_ignore_ascii_case(name).then_some(value.trim())
        });
        let first = found.next();
        assert!(found.next().is_none(), "{name} twice in {}", self.head);
        first
    }
}

/// The route's path for `digest`.
fn path(digest: &Digest) -> String {
    format!("/v1/blobs/{}-{}", digest.hash_hex(), digest.size_bytes)
}

/// `n` bytes that are not all alike, so a slice in the wrong place shows.
fn pattern(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i * 7 + i / 251) as u8).collect()
}

/// Catches: the route serving other bytes than ByteStream Read for the same digest
/// (another blob, a truncated body, a body cut at a chunk size), or serving them as
/// anything but an `application/octet-stream` attachment.
#[tokio::test]
async fn the_bytes_are_those_bytestream_reads() {
    let server = Server::with(OPEN);
    let bytes = pattern(3 * 1024 * 1024 + 17);
    let digest = server.store(bytes.clone()).await;
    let answer = server.get(&path(&digest), "").await;
    assert_eq!(answer.code, 200, "{}", answer.head);
    assert_eq!(
        answer.header("content-type"),
        Some("application/octet-stream")
    );
    assert_eq!(answer.header("content-disposition"), Some("attachment"));
    assert_eq!(answer.header("accept-ranges"), Some("bytes"));
    let length = bytes.len().to_string();
    assert_eq!(answer.header("content-length"), Some(length.as_str()));
    let streamed = server.byte_stream(&digest).await;
    assert_eq!(streamed, bytes);
    assert!(answer.body == streamed, "the bodies differ");
    server.stop().await;
}

/// `digest`'s object with the first byte of its record flipped: the length and footer
/// are unchanged, so only a read that hashes what it got can tell.
async fn corrupt(cache: &MemoryCache, digest: Digest) {
    let answer = cache
        .meta()
        .query(move |s| s.blob(&digest))
        .await
        .expect("query");
    let BlobAnswer::Present { location, .. } = answer else {
        panic!("{digest} is not present: {answer:?}");
    };
    let key = cache.object_key(location.object).expect("key");
    let objects = cache.objects();
    let whole = ByteRange::new(0, u64::from(u32::MAX)).expect("range");
    let mut bytes = objects.get_range(&key, whole).await.expect("get").to_vec();
    let at = usize::try_from(location.offset).expect("offset");
    bytes[at] ^= 0x01;
    objects.delete(&key).await.expect("delete");
    let bytes = Bytes::from(bytes);
    objects.put_new(&key, bytes, None).await.expect("put");
}

/// Catches (mutant "skip verification": the cache's read serves the store's bytes
/// without hashing them): a store returning corrupt bytes answered `200` with those
/// bytes, here or on a second read.
#[tokio::test]
async fn corrupt_bytes_are_an_error_not_a_body() {
    let server = Server::with(OPEN);
    let bytes = b"stdout of an action, then a flipped bit".to_vec();
    let digest = server.store(bytes.clone()).await;
    corrupt(&server.cache, digest).await;
    for attempt in ["first", "second"] {
        let answer = server.get(&path(&digest), "").await;
        assert_eq!(answer.code, 503, "{attempt}: {}", answer.head);
        assert_eq!(answer.header("content-type"), Some("application/json"));
        let body = String::from_utf8_lossy(&answer.body);
        assert!(body.contains("cannot be read"), "{attempt}: {body}");
        assert!(!body.contains("flipped bit"), "{attempt}: {body}");
        assert!(
            !body.contains("busy"),
            "{attempt}: not the busy answer: {body}"
        );
        assert_eq!(answer.header("retry-after"), None, "{attempt}");
    }
    server.stop().await;
}

/// Catches: `Range` ignored (`200` with the whole blob), an end that excludes the
/// last byte asked for, a `Content-Range` that does not say what was sent, a start
/// past the end served instead of `416 bytes */<len>`, and several ranges answered
/// with anything but the whole blob.
#[tokio::test]
async fn a_byte_range_is_honoured() {
    let server = Server::with(OPEN);
    let bytes = pattern(1000);
    let digest = server.store(bytes.clone()).await;
    let blob = path(&digest);
    for (range, first, last) in [
        ("bytes=10-19", 10, 19),
        ("bytes=-5", 995, 999),
        ("bytes=990-", 990, 999),
    ] {
        let answer = server.get(&blob, &format!("Range: {range}\r\n")).await;
        assert_eq!(answer.code, 206, "{range}: {}", answer.head);
        let want = format!("bytes {first}-{last}/1000");
        assert_eq!(
            answer.header("content-range"),
            Some(want.as_str()),
            "{range}"
        );
        assert_eq!(answer.body, bytes[first..=last], "{range}");
    }
    let past = server.get(&blob, "Range: bytes=1000-\r\n").await;
    assert_eq!(past.code, 416, "{}", past.head);
    assert_eq!(past.header("content-range"), Some("bytes */1000"));
    assert!(past.body.is_empty());
    let several = server.get(&blob, "Range: bytes=0-1,5-6\r\n").await;
    assert_eq!(several.code, 200, "{}", several.head);
    assert_eq!(several.header("content-range"), None);
    assert!(several.body == bytes, "the whole blob");
    server.stop().await;
}

/// Catches (mutant "splice the name raw"): a `name` with a quote, a `;` and
/// non-ASCII sent other than percent-encoded in `filename*`, so it could end the
/// value or add a parameter; and `name` lost between the query and the header.
#[tokio::test]
async fn a_name_comes_back_as_a_well_formed_filename_star() {
    let server = Server::with(OPEN);
    let digest = server.store(b"an artifact".to_vec()).await;
    // `a "b";c é.txt`, as a browser encodes it in a query.
    let query = "?name=a+%22b%22%3Bc+%C3%A9.txt";
    let answer = server.get(&format!("{}{query}", path(&digest)), "").await;
    assert_eq!(answer.code, 200, "{}", answer.head);
    assert_eq!(
        answer.header("content-disposition"),
        Some("attachment; filename*=UTF-8''a%20%22b%22%3Bc%20%C3%A9.txt")
    );
    server.stop().await;
}

/// The two headers every answer of the route carries, or the head that lacks them.
fn assert_never_a_page(what: &str, answer: &Answer) {
    assert_eq!(
        answer.header("x-content-type-options"),
        Some("nosniff"),
        "{what}: {}",
        answer.head
    );
    assert_eq!(
        answer.header("content-security-policy"),
        Some("sandbox"),
        "{what}: {}",
        answer.head
    );
}

/// Catches (mutants "drop nosniff", "drop the CSP", "only on attachments", "only on
/// success"): an answer of the route without `X-Content-Type-Options: nosniff` and
/// `Content-Security-Policy: sandbox`, so stdout holding HTML could be sniffed or run
/// as a page on the API's origin. Every kind of answer is asked: inline text, an
/// attachment, a range, `416`, each refusal, and the route turned off.
#[tokio::test]
async fn every_answer_carries_nosniff_and_sandbox() {
    let server = Server::with(OPEN);
    let html = b"<html><script>alert(document.domain)</script></html>".to_vec();
    let digest = server.store(html.clone()).await;
    let blob = path(&digest);
    let inline = server.get(&format!("{blob}?inline=text"), "").await;
    assert_eq!(inline.code, 200, "{}", inline.head);
    assert_eq!(
        inline.header("content-type"),
        Some("text/plain; charset=utf-8")
    );
    assert_eq!(inline.header("content-disposition"), Some("inline"));
    assert_eq!(inline.body, html);
    assert_never_a_page("inline", &inline);
    let named = server
        .get(&format!("{blob}?inline=text&name=out.html"), "")
        .await;
    assert_eq!(
        named.header("content-disposition"),
        Some("inline; filename*=UTF-8''out.html")
    );
    assert_never_a_page("inline, named", &named);
    let absent = Digest::new(kbf_types::DigestFunction::Sha256, [7; 32], 3);
    let large = server
        .store(vec![
            b'x';
            usize::try_from(INLINE_MAX_BYTES).expect("fits") + 1
        ])
        .await;
    for (what, request, headers, code) in [
        ("attachment", blob.clone(), "", 200),
        ("range", blob.clone(), "Range: bytes=0-3\r\n", 206),
        ("past the end", blob.clone(), "Range: bytes=999-\r\n", 416),
        ("bad digest", "/v1/blobs/nope".to_owned(), "", 400),
        ("bad query", format!("{blob}?inline=html"), "", 400),
        (
            "inline too large",
            format!("{}?inline=text", path(&large)),
            "",
            400,
        ),
        ("absent", path(&absent), "", 404),
    ] {
        let answer = server.get(&request, headers).await;
        assert_eq!(answer.code, code, "{what}: {}", answer.head);
        assert_never_a_page(what, &answer);
    }
    server.stop().await;
    let refusals = [
        ("off", "{\"authenticationPolicy\": {\"allow\": {}}}", 404),
        (
            "not authenticated",
            r#"{"authenticationPolicy": {"deny": "who"}, "invocations": {"getAuthorizer": {"allow": {}}}, "blobs": {"getAuthorizer": {"allow": {}}}}"#,
            401,
        ),
        (
            "denied",
            r#"{"authenticationPolicy": {"allow": {}}, "invocations": {"getAuthorizer": {"allow": {}}}, "blobs": {"getAuthorizer": {"deny": {}}}}"#,
            403,
        ),
    ];
    for (what, policy, code) in refusals {
        let server = Server::with(policy);
        let digest = server.store(html.clone()).await;
        let answer = server
            .get(&format!("{}?inline=text", path(&digest)), "")
            .await;
        assert_eq!(answer.code, code, "{what}: {}", answer.head);
        assert!(answer.body != html, "{what}: the bytes were sent");
        assert_never_a_page(what, &answer);
        server.stop().await;
    }
}

/// Catches (mutants "no inline limit" and "the limit excluded", `>=` for `>`):
/// `inline=text` serving a blob over 4 MiB, or refusing one of exactly 4 MiB; and the
/// refusal leaking into the attachment, which serves any size.
#[tokio::test]
async fn inline_is_refused_over_4_mib() {
    let server = Server::with(OPEN);
    let limit = usize::try_from(INLINE_MAX_BYTES).expect("fits");
    assert_eq!(limit, 4 * 1024 * 1024);
    let at = server.store(vec![b'a'; limit]).await;
    let over = server.store(vec![b'b'; limit + 1]).await;
    let ok = server.get(&format!("{}?inline=text", path(&at)), "").await;
    assert_eq!(ok.code, 200, "{}", ok.head);
    assert_eq!(ok.body.len(), limit);
    let refused = server
        .get(&format!("{}?inline=text", path(&over)), "")
        .await;
    assert_eq!(refused.code, 400, "{}", refused.head);
    let why = String::from_utf8_lossy(&refused.body);
    assert!(
        why.contains("inline=text serves at most 4194304 bytes"),
        "{why}"
    );
    let download = server.get(&path(&over), "").await;
    assert_eq!(download.code, 200, "{}", download.head);
    assert_eq!(download.body.len(), limit + 1);
    server.stop().await;
}

/// Catches (mutant "default allow"): the route answering without a policy file, or
/// with a policy that names no `blobs.getAuthorizer` (a missing key read as allow, as
/// every other slot is), and the route not answering once the key is named. And
/// (mutant "blobs without the pages": the `invocations` check dropped from
/// `BlobReaders::from_policy`) the route answering with `blobs` named but no
/// `invocations` key: blobs are what the pages link to, and are off with them
/// (`docs/design/invocations.md` section 4.3).
#[tokio::test]
async fn blob_reads_are_off_unless_the_policy_names_their_authorizer() {
    for (what, policy, code) in [
        ("no policy file", Policy::allow_all(), 404),
        (
            "no blobs key",
            Policy::from_json(
                r#"{"authenticationPolicy": {"allow": {}},
                    "contentAddressableStorage": {"getAuthorizer": {"allow": {}}}}"#,
            )
            .expect("a policy"),
            404,
        ),
        (
            "blobs named, no invocations key",
            Policy::from_json(
                r#"{"authenticationPolicy": {"allow": {}},
                    "blobs": {"getAuthorizer": {"allow": {}}}}"#,
            )
            .expect("a policy"),
            404,
        ),
        (
            "blobs named",
            Policy::from_json(OPEN).expect("a policy"),
            200,
        ),
    ] {
        let server = Server::start(policy);
        let digest = server.store(b"a log".to_vec()).await;
        let answer = server.get(&path(&digest), "").await;
        assert_eq!(answer.code, code, "{what}: {}", answer.head);
        if code == 404 {
            let body = String::from_utf8_lossy(&answer.body);
            assert!(
                body.contains("blob reads over HTTP are off"),
                "{what}: {body}"
            );
            assert!(body.contains(r#""off":"blobs""#), "{what}: {body}");
        }
        let nodes = server.get("/v1/nodes", "").await;
        assert_eq!(nodes.code, 200, "{what}: the rest of the API answers");
        server.stop().await;
    }
}

/// Catches (mutants "skip the authorizer" and "ask about the empty instance"): a
/// read the authorizer would refuse served anyway, and the authorizer asked about
/// another instance name than the `instance` parameter (decoded, `/` included).
#[tokio::test]
async fn the_authorizer_is_asked_about_the_instance_parameter() {
    let server = Server::with(
        r#"{"authenticationPolicy": {"allow": {}},
            "invocations": {"getAuthorizer": {"allow": {}}}, "blobs": {"getAuthorizer": {"instanceNamePrefix": {"allowedInstanceNamePrefixes": ["ci"]}}}}"#,
    );
    let digest = server.store(b"stderr".to_vec()).await;
    let blob = path(&digest);
    for (query, code) in [
        ("?instance=ci", 200),
        ("?instance=ci%2Fmain", 200),
        ("?instance=cid", 403),
        ("", 403),
        ("?instance=", 403),
    ] {
        let answer = server.get(&format!("{blob}{query}"), "").await;
        assert_eq!(answer.code, code, "{query}: {}", answer.head);
    }
    server.stop().await;
}

/// Catches: an absent blob answered other than `404`, a malformed digest other than
/// `400` (a `500`, or a read of some other blob), and an unknown parameter ignored.
#[tokio::test]
async fn absent_is_404_and_malformed_is_400() {
    let server = Server::with(OPEN);
    let digest = server.store(b"present".to_vec()).await;
    let absent = Digest::new(kbf_types::DigestFunction::Sha256, [9; 32], 7);
    let hex = digest.hash_hex();
    for (request, code) in [
        (path(&absent), 404),
        (format!("/v1/blobs/{hex}/{}", digest.size_bytes), 404),
        (
            format!("/v1/blobs/{}-{}", hex.to_uppercase(), digest.size_bytes),
            400,
        ),
        (format!("/v1/blobs/{hex}"), 400),
        (format!("/v1/blobs/{hex}-x"), 400),
        (format!("{}?inline=text&inlne=1", path(&digest)), 400),
        (path(&digest), 200),
    ] {
        let answer = server.get(&request, "").await;
        assert_eq!(answer.code, code, "{request}: {}", answer.head);
    }
    server.stop().await;
}

/// Farm time `n` days after the epoch.
fn days(n: u64) -> FarmTime {
    FarmTime::from_millis(n * 24 * 60 * 60 * 1000)
}

/// Catches (mutant "read without the touch"): a read over HTTP that does not count
/// as a use, so a log a person is reading is collected at the end of its TTL. A blob
/// read on day 5 outlives a collection on day 10; its unread neighbour, stored with
/// it, does not.
#[tokio::test]
async fn each_read_is_a_touch() {
    let server = Server::with(OPEN);
    let read = server.store(b"read on day 5".to_vec()).await;
    let unread = server.store(b"never read".to_vec()).await;
    server.cache.tick(days(5)).await.expect("tick");
    let answer = server.get(&path(&read), "").await;
    assert_eq!(answer.code, 200, "{}", answer.head);
    server.cache.tick(days(10)).await.expect("tick");
    server.cache.collect().await.expect("collect");
    let missing = server
        .cache
        .find_missing(&[read, unread])
        .await
        .expect("find");
    assert_eq!(missing, [unread]);
    server.stop().await;
}

/// Catches (mutant "authorize after the read"): a caller the authorizer refuses
/// learning whether a blob is held (`404` for an absent one, `403` for a present one),
/// or keeping a blob alive by asking for it. Both asks are `403`, and the asked-for
/// blob is collected on day 10 as if nobody had asked; its neighbour, read on day 5 by
/// a caller the authorizer allows, survives, so the collection does see a touch.
#[tokio::test]
async fn a_refused_caller_learns_nothing_and_touches_nothing() {
    let server = Server::with(
        r#"{"authenticationPolicy": {"allow": {}},
            "invocations": {"getAuthorizer": {"allow": {}}}, "blobs": {"getAuthorizer": {"instanceNamePrefix": {"allowedInstanceNamePrefixes": ["ci"]}}}}"#,
    );
    let asked = server
        .store(b"asked for by a refused caller".to_vec())
        .await;
    let read = server.store(b"read by an allowed caller".to_vec()).await;
    let absent = Digest::new(kbf_types::DigestFunction::Sha256, [9; 32], 7);
    server.cache.tick(days(5)).await.expect("tick");
    for digest in [absent, asked] {
        let answer = server
            .get(&format!("{}?instance=other", path(&digest)), "")
            .await;
        assert_eq!(answer.code, 403, "{digest}: {}", answer.head);
    }
    let allowed = server
        .get(&format!("{}?instance=ci", path(&read)), "")
        .await;
    assert_eq!(allowed.code, 200, "{}", allowed.head);
    server.cache.tick(days(10)).await.expect("tick");
    server.cache.collect().await.expect("collect");
    let missing = server
        .cache
        .find_missing(&[asked, read])
        .await
        .expect("find");
    assert_eq!(missing, [asked], "the refused ask touched the blob");
    server.stop().await;
}

/// Catches: a large digest not held answered `503` ("try again", which can never
/// succeed), or a size too large for a permit count (2^33) panicking: both are `404`
/// with the reason.
#[tokio::test]
async fn a_large_digest_not_held_is_404() {
    let server = Server::with(OPEN);
    for size in [(5 << 30) + 1, 1 << 33] {
        let digest = Digest::new(kbf_types::DigestFunction::Sha256, [9; 32], size);
        let answer = server.get(&path(&digest), "").await;
        assert_eq!(answer.code, 404, "{size}: {}", answer.head);
        let body = String::from_utf8_lossy(&answer.body);
        assert!(body.contains("is not in the cache"), "{size}: {body}");
    }
    server.stop().await;
}
