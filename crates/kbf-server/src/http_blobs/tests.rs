use super::*;

/// Decodes a `filename*` value of RFC 8187 (`UTF-8''<value-chars>`), refusing any
/// byte that is neither an `attr-char` nor a `%` and two hex digits.
fn decode_ext_value(value: &str) -> Result<String, String> {
    let encoded = value
        .strip_prefix("UTF-8''")
        .ok_or_else(|| format!("{value:?}: no UTF-8'' charset"))?;
    let bytes = encoded.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'%' {
            let hex = encoded
                .get(i + 1..i + 3)
                .filter(|h| h.bytes().all(|c| c.is_ascii_hexdigit()))
                .ok_or_else(|| format!("{value:?}: a bad escape at {i}"))?;
            out.push(u8::from_str_radix(hex, 16).expect("hex"));
            i += 3;
        } else if b.is_ascii_alphanumeric() || b"!#$&+-.^_`|~".contains(&b) {
            out.push(b);
            i += 1;
        } else {
            return Err(format!("{value:?}: {:?} is not an attr-char", b as char));
        }
    }
    String::from_utf8(out).map_err(|e| format!("{value:?}: {e}"))
}

/// The `filename*` of a `Content-Disposition` of `kind` with exactly one parameter;
/// an error if it is not that.
fn filename(kind: &str, header: &HeaderValue) -> Result<String, String> {
    let text = header.to_str().map_err(|e| e.to_string())?;
    let rest = text
        .strip_prefix(kind)
        .and_then(|r| r.strip_prefix("; filename*="))
        .ok_or_else(|| format!("{text:?}: not `{kind}; filename*=`"))?;
    decode_ext_value(rest)
}

/// Catches (mutant "splice the name raw", or `filename="<name>"`): a name with a
/// quote, a `;`, a line break, a `%` or non-ASCII that ends the value early, adds a
/// parameter, or does not decode back to itself; and a disposition with a filename
/// when no name is given.
#[test]
fn a_name_is_sent_only_percent_encoded_in_filename_star() {
    assert_eq!(content_disposition("attachment", None), "attachment");
    assert_eq!(content_disposition("inline", None), "inline");
    for name in [
        "lib.a",
        "a \"quoted\"; filename=evil.html",
        "line\r\nX-Injected: 1",
        "100%.txt",
        "caf\u{e9} \u{4e2d}\u{6587}.log",
        "dir/../file",
    ] {
        for kind in ["attachment", "inline"] {
            let header = content_disposition(kind, Some(name));
            assert_eq!(filename(kind, &header).as_deref(), Ok(name), "{header:?}");
        }
    }
    assert_eq!(
        content_disposition("attachment", Some("a b;\"\u{e9}")),
        "attachment; filename*=UTF-8''a%20b%3B%22%C3%A9"
    );
}

/// Catches: the checker above accepting what it must refuse, so the test above would
/// pass a raw splice.
#[test]
fn the_filename_checker_refuses_a_raw_name() {
    let raw = HeaderValue::from_static("attachment; filename*=UTF-8''a\"b;c");
    assert!(filename("attachment", &raw).is_err());
    let quoted = HeaderValue::from_static("attachment; filename=\"a\"");
    assert!(filename("attachment", &quoted).is_err());
    assert!(decode_ext_value("utf-8''a").is_err());
    assert!(decode_ext_value("UTF-8''%4").is_err());
    assert!(decode_ext_value("UTF-8''%zz").is_err());
    assert!(decode_ext_value("UTF-8''%FF").is_err());
    assert!(
        filename(
            "attachment",
            &HeaderValue::from_bytes(b"\xff").expect("obs-text")
        )
        .is_err()
    );
}

const HASH: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// Catches: accepting a digest form other than `<hash>-<size>` (the REAPI `/` form, an
/// uppercase or short hash, a signed or missing size, trailing text), which would let
/// two spellings name one blob.
#[test]
fn a_digest_is_lowercase_hex_dash_size() {
    let d = parse_digest(&format!("{HASH}-0")).expect("a digest");
    assert_eq!((d.hash_hex(), d.size_bytes), (HASH.to_owned(), 0));
    assert_eq!(
        parse_digest(&format!("{HASH}-142")).map(|d| d.size_bytes),
        Some(142)
    );
    for bad in [
        format!("{HASH}/142"),
        format!("{}-142", HASH.to_uppercase()),
        format!("{}-142", &HASH[1..]),
        format!("{HASH}-+142"),
        format!("{HASH}-"),
        format!("{HASH}-142-1"),
        format!("{HASH}-142 "),
        HASH.to_owned(),
        String::new(),
    ] {
        assert_eq!(parse_digest(&bad), None, "{bad:?}");
    }
}

/// Catches: a query decoded other than as a browser encodes it (`+`, `%2F`), an
/// empty `name` sent as `filename*=UTF-8''`, a key given twice read either way, and a
/// typo'd key or `inline` value ignored.
#[test]
fn the_query_is_decoded_and_strict() {
    assert_eq!(Query::parse(""), Ok(Query::default()));
    assert_eq!(
        Query::parse("instance=ci%2Fmain&name=my+file%3B.txt&inline=text"),
        Ok(Query {
            instance: "ci/main".to_owned(),
            name: Some("my file;.txt".to_owned()),
            inline: true,
        })
    );
    assert_eq!(Query::parse("instance=&name="), Ok(Query::default()));
    for (bad, why) in [
        ("instance=a&instance=b", "more than once"),
        ("name=a&name=", "more than once"),
        ("inlne=text", "unknown parameter"),
        ("inline=html", "the one form is inline=text"),
        ("inline=", "the one form is inline=text"),
    ] {
        let e = Query::parse(bad).expect_err(bad);
        assert!(e.contains(why), "{bad}: {e}");
    }
}

fn range(value: &str) -> Wanted {
    wanted(Some(&HeaderValue::from_str(value).expect("a header")), 10)
}

fn part(first: u64, last: u64) -> Wanted {
    Wanted::Part { first, last }
}

/// Catches: an off-by-one at either end (the last byte is included), a suffix or
/// open range read wrong, an end past the blob not cut to it, a start at the end
/// served instead of `416`, and a malformed or multi-range header refused instead of
/// ignored (RFC 9110 lets a server ignore it; it must not fail the read).
#[test]
fn one_byte_range_is_honoured_and_the_rest_ignored() {
    assert_eq!(wanted(None, 10), Wanted::Whole);
    for (value, want) in [
        ("bytes=0-0", part(0, 0)),
        ("bytes=2-5", part(2, 5)),
        ("bytes=2-", part(2, 9)),
        ("bytes=9-9", part(9, 9)),
        ("bytes=5-100", part(5, 9)),
        ("bytes=-3", part(7, 9)),
        ("bytes=-30", part(0, 9)),
        ("BYTES = 1 - 2", part(1, 2)),
        ("bytes=10-", Wanted::Unsatisfiable),
        ("bytes=10-20", Wanted::Unsatisfiable),
        ("bytes=-0", Wanted::Unsatisfiable),
        ("bytes=5-3", Wanted::Whole),
        ("bytes=0-1,3-4", Wanted::Whole),
        ("bytes=-", Wanted::Whole),
        ("bytes=+1-2", Wanted::Whole),
        ("bytes=1-x", Wanted::Whole),
        ("bytes=-x", Wanted::Whole),
        ("bytes=99999999999999999999-", Wanted::Whole),
        ("bytes=3", Wanted::Whole),
        ("items=0-1", Wanted::Whole),
        ("0-1", Wanted::Whole),
    ] {
        assert_eq!(range(value), want, "{value}");
    }
    let empty = HeaderValue::from_static("bytes=-3");
    assert_eq!(wanted(Some(&empty), 0), Wanted::Unsatisfiable);
    let zero = HeaderValue::from_static("bytes=0-");
    assert_eq!(wanted(Some(&zero), 0), Wanted::Unsatisfiable);
    let opaque = HeaderValue::from_bytes(b"bytes=\xff-1").expect("obs-text");
    assert_eq!(wanted(Some(&opaque), 10), Wanted::Whole);
}

/// Catches: a refusal answered with the wrong HTTP status (an authentication failure
/// as 403, a denial as 404, a store failure as 500), so a client cannot tell "who are
/// you" from "not you" from "try again".
#[test]
fn refusals_keep_their_meaning() {
    for (status, code) in [
        (Status::unauthenticated("u"), StatusCode::UNAUTHORIZED),
        (Status::permission_denied("p"), StatusCode::FORBIDDEN),
        (Status::invalid_argument("i"), StatusCode::BAD_REQUEST),
        (Status::not_found("n"), StatusCode::NOT_FOUND),
        (Status::unavailable("a"), StatusCode::SERVICE_UNAVAILABLE),
        (Status::internal("x"), StatusCode::INTERNAL_SERVER_ERROR),
    ] {
        let answer = refusal(&status);
        assert_eq!(answer.status(), code, "{status:?}");
        assert_eq!(answer.headers()[CONTENT_TYPE], crate::api::JSON);
    }
}

/// The state of a route over a memory cache that holds at most `budget` blob bytes at
/// once, with every caller allowed and `contents` stored; their digests, in order.
async fn budgeted(
    budget: usize,
    contents: &[&'static [u8]],
) -> (
    BlobState<kbf_front::MemoryMetaLog, kbf_objstore::MemoryStore>,
    Vec<Digest>,
) {
    let policy = Policy::from_json(
        r#"{"authenticationPolicy": {"allow": {}}, "invocations": {"getAuthorizer": {"allow": {}}}, "blobs": {"getAuthorizer": {"allow": {}}}}"#,
    )
    .expect("a policy");
    let cache = Arc::new(Cache::memory());
    let blobs: Vec<_> = contents
        .iter()
        .map(|c| kbf_front::VerifiedBlob::hashed(Bytes::from_static(c)))
        .collect();
    let digests = blobs.iter().map(kbf_front::VerifiedBlob::digest).collect();
    cache.store_blobs(blobs).await.expect("store");
    let state = BlobState::new(cache, BlobReaders::from_policy(&policy), budget);
    (state, digests)
}

/// The answer to a `GET` of `digest` (no query, no headers).
async fn get<M: MetaLog, O: ObjectStore + 'static>(
    state: &BlobState<M, O>,
    digest: &Digest,
) -> Response {
    get_range(state, digest, None).await
}

/// The answer to a `GET` of `digest` with `Range: <range>` when `range` is given.
async fn get_range<M: MetaLog, O: ObjectStore + 'static>(
    state: &BlobState<M, O>,
    digest: &Digest,
    range: Option<&str>,
) -> Response {
    let uri = format!("{PREFIX}{}-{}", digest.hash_hex(), digest.size_bytes);
    let mut request = axum::http::Request::builder().uri(uri);
    if let Some(range) = range {
        request = request.header(RANGE, range);
    }
    let (parts, ()) = request.body(()).expect("a request").into_parts();
    match answer(state, &parts).await {
        Ok(answer) | Err(answer) => answer,
    }
}

/// The answer to a `GET` of `digest`, which must come within twice the wait: a read
/// that waits forever fails here instead of hanging the test.
async fn get_in_time<M: MetaLog, O: ObjectStore + 'static>(
    state: &BlobState<M, O>,
    digest: &Digest,
    range: Option<&str>,
) -> Response {
    tokio::time::timeout(2 * READ_BUDGET_WAIT, get_range(state, digest, range))
        .await
        .expect("answered within twice the wait")
}

/// Catches (mutants "release the budget once the blob is read", "one permit per read
/// instead of per byte", "no budget", "wait forever", "another wait", "a busy answer
/// without Retry-After or `busy`"): reads that together hold more blob bytes than the
/// budget, and a busy read not told so. With 10 bytes, an 8-byte answer not yet sent
/// leaves room for a 2-byte read; a second 8-byte read waits exactly
/// [`READ_BUDGET_WAIT`] and is `503` with `Retry-After: 1` and `"busy": true`. Once
/// the first answer's bytes are dropped the 8-byte blob is served again.
#[tokio::test(start_paused = true)]
async fn bytes_not_yet_sent_hold_the_read_budget() {
    let (state, digests) = budgeted(10, &[b"8 bytes!", b"2b"]).await;
    let (eight, two) = (digests[0], digests[1]);
    let first = get(&state, &eight).await;
    assert_eq!(first.status(), StatusCode::OK);
    let asked = tokio::time::Instant::now();
    let busy = get_in_time(&state, &eight, None).await;
    assert_eq!(asked.elapsed(), READ_BUDGET_WAIT);
    assert_eq!(busy.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(busy.headers()[RETRY_AFTER], "1");
    assert_eq!(busy.headers()[CONTENT_TYPE], crate::api::JSON);
    let why = axum::body::to_bytes(busy.into_body(), usize::MAX)
        .await
        .expect("a body");
    let why: serde_json::Value = serde_json::from_slice(&why).expect("JSON");
    assert_eq!(why["busy"], true, "{why}");
    let text = why["error"].as_str().expect("an error");
    assert!(text.contains("was not free within 10 s"), "{text}");
    let small = get_in_time(&state, &two, None).await;
    assert_eq!(small.status(), StatusCode::OK);
    drop(small);
    let body = axum::body::to_bytes(first.into_body(), usize::MAX)
        .await
        .expect("a body");
    assert_eq!(&body[..], b"8 bytes!");
    drop(body);
    assert_eq!(
        get_in_time(&state, &eight, None).await.status(),
        StatusCode::OK
    );
}

/// Catches (mutant "take the budget only if it is free now", the first form): a large
/// read starved by smaller ones. With 8 of 10 bytes held, a 10-byte read waits in
/// line; a 2-byte read asked after it waits behind it although 2 bytes are free, and
/// is served only once the 10-byte answer is dropped.
#[tokio::test]
async fn reads_take_the_budget_in_the_order_they_asked() {
    let (state, digests) = budgeted(10, &[b"8 bytes!", b"ten bytes!", b"2b"]).await;
    let (eight, ten, two) = (digests[0], digests[1], digests[2]);
    let state = Arc::new(state);
    let first = get(&state, &eight).await;
    assert_eq!(first.status(), StatusCode::OK);
    let large = tokio::spawn({
        let state = Arc::clone(&state);
        async move { get_in_time(&state, &ten, None).await }
    });
    // The read at the head of the line takes the free permits as they come.
    while state.budget.available_permits() > 0 && !large.is_finished() {
        tokio::task::yield_now().await;
    }
    let small = tokio::spawn({
        let state = Arc::clone(&state);
        async move { get_in_time(&state, &two, None).await }
    });
    for _ in 0..100 {
        tokio::task::yield_now().await;
    }
    assert!(
        !large.is_finished(),
        "the 10-byte read waits for the 8 bytes held"
    );
    assert!(
        !small.is_finished(),
        "the 2-byte read waits behind the 10-byte one"
    );
    drop(first);
    let large = large.await.expect("the large read");
    assert_eq!(large.status(), StatusCode::OK);
    for _ in 0..100 {
        tokio::task::yield_now().await;
    }
    assert!(
        !small.is_finished(),
        "the 10-byte answer holds the whole budget"
    );
    drop(large);
    assert_eq!(
        small.await.expect("the small read").status(),
        StatusCode::OK
    );
}

/// Catches: a digest larger than any blob the cache accepts answered `503` ("try
/// again", which can never succeed), or a size too large for a permit count
/// panicking. Both are `404` with the limit named, at once.
#[tokio::test(start_paused = true)]
async fn a_digest_larger_than_any_blob_is_404_at_once() {
    let (state, _) = budgeted(10, &[]).await;
    for size in [kbf_front::MAX_BLOB_BYTES + 1, 1 << 33] {
        let asked = tokio::time::Instant::now();
        let digest = Digest::new(DigestFunction::Sha256, [9; 32], size);
        let answer = get(&state, &digest).await;
        assert_eq!(asked.elapsed(), Duration::ZERO, "{size}");
        assert_eq!(answer.status(), StatusCode::NOT_FOUND, "{size}");
        let why = axum::body::to_bytes(answer.into_body(), usize::MAX)
            .await
            .expect("a body");
        let why = String::from_utf8_lossy(&why);
        assert!(
            why.contains("larger than the largest blob"),
            "{size}: {why}"
        );
        assert!(why.contains("(10 bytes)"), "{size}: {why}");
    }
}

/// Catches (mutant "a reader makes a budget of its own"): the blob route and the other
/// readers of the process (the failure-log route, the history writer's tail reads) each
/// holding a whole budget of their own, so that together they hold more than the process
/// may. A budget given to the route is the route's: what another reader holds of it, the
/// route cannot take, and what it releases, the route can.
#[tokio::test(start_paused = true)]
async fn the_blob_route_and_other_readers_draw_on_one_budget() {
    let (state, digests) = budgeted(10, &[b"8 bytes!"]).await;
    let other = state.budget.clone();
    let held = other.acquire(10, READ_BUDGET_WAIT).await.expect("free");
    assert_eq!(other.available_permits(), 0);
    let busy = get_in_time(&state, &digests[0], None).await;
    assert_eq!(busy.status(), StatusCode::SERVICE_UNAVAILABLE);
    drop(held);
    assert_eq!(
        get_in_time(&state, &digests[0], None).await.status(),
        StatusCode::OK
    );
}

/// Catches: a share larger than the whole budget (or than a permit count holds) waiting
/// for the time it never gets (it is refused at once), a wait that is not the wait, and a
/// budget that does not start full.
#[tokio::test(start_paused = true)]
async fn a_read_budget_refuses_what_never_fits_and_times_out_what_is_not_free() {
    let budget = ReadBudget::new(100);
    assert_eq!(budget.available_permits(), 100);
    assert_eq!(
        budget.acquire(101, READ_BUDGET_WAIT).await.unwrap_err(),
        Refused::TooLarge
    );
    assert_eq!(
        budget.acquire(1 << 33, READ_BUDGET_WAIT).await.unwrap_err(),
        Refused::TooLarge
    );
    let all = budget.acquire(100, READ_BUDGET_WAIT).await.expect("free");
    let asked = tokio::time::Instant::now();
    let wait = std::time::Duration::from_secs(3);
    assert_eq!(budget.acquire(1, wait).await.unwrap_err(), Refused::Busy);
    assert_eq!(asked.elapsed(), wait);
    drop(all);
    assert_eq!(budget.available_permits(), 100);
    assert_eq!(ReadBudget::default().available_permits(), READ_BUDGET_BYTES);
    assert!(format!("{budget:?}").contains("largest: 100"));
}

/// Catches: a budget smaller than the largest blob the cache accepts, which could
/// never be served.
#[test]
fn the_read_budget_holds_the_largest_blob() {
    assert_eq!(
        u64::try_from(READ_BUDGET_BYTES),
        Ok(kbf_front::MAX_BLOB_BYTES)
    );
}

/// Catches (mutant "the budget is released only when the body's end is read"): an
/// answer the client never reads (it went away) keeping its blob's budget, so three
/// abandoned 8-byte downloads under a 10-byte budget would shut the route for good.
#[tokio::test(start_paused = true)]
async fn an_answer_dropped_unread_frees_its_budget() {
    let (state, digests) = budgeted(10, &[b"8 bytes!"]).await;
    for _ in 0..3 {
        let answer = get_in_time(&state, &digests[0], None).await;
        assert_eq!(answer.status(), StatusCode::OK);
        drop(answer);
    }
    assert_eq!(
        get_in_time(&state, &digests[0], None).await.status(),
        StatusCode::OK
    );
}

/// Catches (mutant "a failed read keeps its permit"): a read that fails (here an
/// absent blob of 10 bytes, the whole budget) never giving its budget back.
#[tokio::test(start_paused = true)]
async fn a_failed_read_frees_its_budget() {
    let (state, digests) = budgeted(10, &[b"8 bytes!"]).await;
    let absent = Digest::new(DigestFunction::Sha256, [9; 32], 10);
    assert_eq!(
        get_in_time(&state, &absent, None).await.status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        get_in_time(&state, &digests[0], None).await.status(),
        StatusCode::OK
    );
}

/// Catches (mutant "a range answer copies its part"): a `206` that frees the whole
/// blob's budget while its part is still being sent, which `Range: bytes=0-` would
/// turn into reads the budget never counts. The part holds the whole blob's share
/// until it is dropped.
#[tokio::test(start_paused = true)]
async fn a_part_holds_the_whole_blobs_budget() {
    let (state, digests) = budgeted(10, &[b"8 bytes!"]).await;
    let part = get_in_time(&state, &digests[0], Some("bytes=0-0")).await;
    assert_eq!(part.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        get_in_time(&state, &digests[0], None).await.status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    drop(part);
    assert_eq!(
        get_in_time(&state, &digests[0], None).await.status(),
        StatusCode::OK
    );
}

/// Catches (mutant "`<` for `<=`"): a blob of exactly the whole budget (the largest
/// the cache accepts, in the route's budget) answered `404`. With a 10-byte budget a
/// 10-byte blob is served; a digest of 11 bytes is `404` at once.
#[tokio::test(start_paused = true)]
async fn a_blob_of_exactly_the_budget_is_served_and_one_byte_more_is_404() {
    let (state, digests) = budgeted(10, &[b"ten bytes!"]).await;
    let whole = get_in_time(&state, &digests[0], None).await;
    assert_eq!(whole.status(), StatusCode::OK);
    let body = axum::body::to_bytes(whole.into_body(), usize::MAX)
        .await
        .expect("a body");
    assert_eq!(&body[..], b"ten bytes!");
    let over = Digest::new(DigestFunction::Sha256, [9; 32], 11);
    let asked = tokio::time::Instant::now();
    assert_eq!(
        get_in_time(&state, &over, None).await.status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(asked.elapsed(), Duration::ZERO);
}
