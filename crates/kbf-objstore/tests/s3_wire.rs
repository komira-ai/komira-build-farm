//! The S3 backend against a scripted HTTP server on loopback: what it sends, and what it
//! refuses to believe. The real-store runs (MinIO, RustFS) are in CI; these pin the
//! cases a real store does not produce on demand.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use kbf_objstore::s3::{Credentials, S3Config, S3Store};
use kbf_objstore::{
    ByteRange, Capabilities, KeyPrefix, ObjectKey, ObjectStore, ObjectStoreError, PageSize,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Serves `responses` in order, one per connection, and records each request head.
async fn script(responses: Vec<String>) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    tokio::spawn(async move {
        for resp in responses {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            let head_end = loop {
                let n = sock.read(&mut chunk).await.unwrap();
                assert!(n > 0, "client closed before sending a request");
                buf.extend_from_slice(&chunk[..n]);
                if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    break i + 4;
                }
            };
            let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
            let body_len = head
                .lines()
                .find_map(|l| {
                    l.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(|v| v.trim().parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            while buf.len() < head_end + body_len {
                let n = sock.read(&mut chunk).await.unwrap();
                buf.extend_from_slice(&chunk[..n]);
            }
            log.lock().unwrap().push(head);
            sock.write_all(resp.as_bytes()).await.unwrap();
            sock.shutdown().await.unwrap();
        }
    });
    (endpoint, seen)
}

fn answer(status: &str, headers: &[&str], body: &str) -> String {
    let mut s = format!(
        "HTTP/1.1 {status}\r\nconnection: close\r\ncontent-length: {}\r\n",
        body.len()
    );
    for h in headers {
        s.push_str(h);
        s.push_str("\r\n");
    }
    s.push_str("\r\n");
    s.push_str(body);
    s
}

fn store(endpoint: String, capabilities: Capabilities) -> S3Store {
    S3Store::new(S3Config {
        endpoint,
        region: "us-east-1".into(),
        bucket: "kbf-test".into(),
        credentials: Credentials::new("AKIDEXAMPLE", "secret"),
        capabilities,
        connect_timeout: Duration::from_secs(5),
        request_timeout: Duration::from_secs(5),
    })
    .unwrap()
}

fn key() -> ObjectKey {
    ObjectKey::new("cas/seg-1").unwrap()
}

/// Catches: a client that accepts a 200 to a ranged GET (a server or proxy that ignores
/// Range), or a 206 whose Content-Range names other bytes, and hands the wrong bytes to
/// the caller as if they were the window asked for.
#[tokio::test]
async fn a_server_ignoring_range_is_refused() {
    let (endpoint, seen) = script(vec![
        answer("200 OK", &[], "0123456789"),
        answer(
            "206 Partial Content",
            &["content-range: bytes 0-3/10"],
            "0123",
        ),
        answer(
            "206 Partial Content",
            &["content-range: bytes 2-5/10"],
            "2345",
        ),
    ])
    .await;
    let s = store(endpoint, Capabilities::default());
    let r = ByteRange::new(2, 4).unwrap();
    let err = s.get_range(&key(), r).await.unwrap_err();
    assert!(
        matches!(&err, ObjectStoreError::Protocol(m) if m.contains("ignored Range")),
        "{err}"
    );
    let err = s.get_range(&key(), r).await.unwrap_err();
    assert!(matches!(err, ObjectStoreError::Protocol(_)), "{err}");
    assert_eq!(s.get_range(&key(), r).await.unwrap(), Bytes::from("2345"));

    let heads = seen.lock().unwrap();
    let first = heads[0].to_ascii_lowercase();
    assert!(
        first.starts_with("get /kbf-test/cas/seg-1 http/1.1\r\n"),
        "path-style: {first}"
    );
    assert!(first.contains("\r\nrange: bytes=2-5\r\n"), "{first}");
    assert!(
        first.contains("signedheaders=host;range;x-amz-content-sha256;x-amz-date"),
        "{first}"
    );
}

/// Catches: a put that does not ask for a conditional write when the store claims it,
/// or reads 412 as anything but `AlreadyExists`; and a retained put without the lock
/// headers and the checksum S3 requires.
#[tokio::test]
async fn put_new_sends_conditions_and_retention() {
    let (endpoint, seen) = script(vec![
        answer("412 Precondition Failed", &[], ""),
        answer("200 OK", &[], ""),
    ])
    .await;
    let s = store(
        endpoint,
        Capabilities {
            conditional_put: true,
            object_lock: true,
        },
    );
    let err = s
        .put_new(&key(), Bytes::from("abc"), None)
        .await
        .unwrap_err();
    assert!(matches!(err, ObjectStoreError::AlreadyExists(_)), "{err}");
    let until = std::time::UNIX_EPOCH + Duration::from_secs(4_107_542_400);
    s.put_new(&key(), Bytes::from("abc"), Some(until))
        .await
        .unwrap();

    let heads = seen.lock().unwrap();
    let put = heads[1].to_ascii_lowercase();
    assert!(put.contains("\r\nif-none-match: *\r\n"), "{put}");
    assert!(
        put.contains("\r\nx-amz-object-lock-mode: compliance\r\n"),
        "{put}"
    );
    assert!(
        put.contains("\r\nx-amz-object-lock-retain-until-date: 2100-03-01t00:00:00z\r\n"),
        "{put}"
    );
    // MD5("abc"), base64.
    assert!(
        put.contains(
            "\r\ncontent-md5: kAFQmDzST7DWlj99KOF/cg==\r\n"
                .to_ascii_lowercase()
                .as_str()
        ),
        "{put}"
    );
    // SHA-256("abc"), the signed payload hash.
    assert!(
        put.contains(
            "x-amz-content-sha256: ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        ),
        "{put}"
    );
}

/// Catches: a delete on an Object Lock store sent without the version id, which S3
/// answers by adding a delete marker (success) while keeping the bytes; and a delete of
/// an absent key reported as an error.
#[tokio::test]
async fn delete_on_a_locked_store_targets_the_version() {
    let (endpoint, seen) = script(vec![
        answer("200 OK", &["x-amz-version-id: v+1/x"], ""),
        answer(
            "403 Forbidden",
            &[],
            "<Error><Code>AccessDenied</Code><Message>locked</Message></Error>",
        ),
        answer("404 Not Found", &[], ""),
    ])
    .await;
    let s = store(
        endpoint,
        Capabilities {
            conditional_put: false,
            object_lock: true,
        },
    );
    let err = s.delete(&key()).await.unwrap_err();
    assert!(
        matches!(&err, ObjectStoreError::Service { status: 403, code, .. } if code == "AccessDenied"),
        "{err}"
    );
    s.delete(&key()).await.unwrap();

    let heads = seen.lock().unwrap();
    assert!(
        heads[0].starts_with("HEAD /kbf-test/cas/seg-1 HTTP/1.1"),
        "{}",
        heads[0]
    );
    assert!(
        heads[1].starts_with("DELETE /kbf-test/cas/seg-1?versionId=v%2B1%2Fx HTTP/1.1"),
        "{}",
        heads[1]
    );
    assert!(heads[2].starts_with("HEAD "), "{}", heads[2]);
}

/// Catches: a list request that is not ListObjectsV2, loses the continuation token or
/// encodes the query differently from how it was signed.
#[tokio::test]
async fn list_sends_the_token_and_reads_the_next_one() {
    let page = "<ListBucketResult><IsTruncated>true</IsTruncated>\
                <NextContinuationToken>t2</NextContinuationToken>\
                <Contents><Key>p/a</Key><Size>1</Size></Contents></ListBucketResult>";
    let (endpoint, seen) = script(vec![
        answer("200 OK", &[], page),
        answer("200 OK", &[], page),
    ])
    .await;
    let s = store(endpoint, Capabilities::default());
    let prefix = KeyPrefix::new("p/").unwrap();
    let first = s
        .list(&prefix, None, PageSize::new(3).unwrap())
        .await
        .unwrap();
    let token = first.next.expect("a truncated page has a token");
    s.list(&prefix, Some(&token), PageSize::new(3).unwrap())
        .await
        .unwrap();

    let heads = seen.lock().unwrap();
    assert!(
        heads[0].starts_with("GET /kbf-test?list-type=2&max-keys=3&prefix=p%2F HTTP/1.1"),
        "{}",
        heads[0]
    );
    assert!(
        heads[1].starts_with(
            "GET /kbf-test?continuation-token=t2&list-type=2&max-keys=3&prefix=p%2F HTTP/1.1"
        ),
        "{}",
        heads[1]
    );
}
