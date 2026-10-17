//! The walk over instances, the page position and the names of a ring's classes. Each test
//! names the defect it catches.

use axum::http::StatusCode;
use kbf_history::records::TestStatus;
use kbf_history::{
    Mark, PageCursor, ReadError, RingRun, ShapeKind, ShapeRow, StatusKind, StatusRow,
};
use serde_json::{Value, json};

use super::{
    Found, MAX_FETCHES, Position, across, class, decode_position, encode_position, parse_key,
    render, ring_run,
};

fn cursor(text: &str) -> PageCursor {
    text.parse().expect("a cursor")
}

/// Three items in every instance, named `<instance><index>`; the cursor is the index of
/// the next one.
fn pages(
    instance: &str,
    after: Option<&PageCursor>,
    room: usize,
) -> (Vec<String>, Option<PageCursor>) {
    let from: usize = after.map_or(0, |c| c.to_string().parse().expect("an index"));
    let upto = (from + room).min(3);
    let items = (from..upto).map(|i| format!("{instance}{i}")).collect();
    (items, (upto < 3).then(|| cursor(&upto.to_string())))
}

async fn walk(
    instances: &[&str],
    start: Option<Position>,
    limit: usize,
) -> (Vec<String>, Option<String>) {
    let instances: Vec<String> = instances.iter().map(|i| (*i).to_owned()).collect();
    let got = across(&instances, start, limit, |instance, after, room| {
        std::future::ready(Ok::<_, ReadError>(pages(&instance, after.as_ref(), room)))
    })
    .await;
    got.expect("a walk")
}

fn at(name: &str, c: Option<&str>) -> Option<Position> {
    Some((name.to_owned(), c.map(cursor)))
}

/// Catches: a page that skips or repeats an item where it ends (inside an instance, at an
/// instance's last item, at the last instance), a token that resumes at the wrong place,
/// and a walk that never says it is done.
#[tokio::test]
async fn a_walk_resumes_where_the_page_before_ended() {
    let all = ["a", "b", "c"];
    let (first, token) = walk(&all, None, 4).await;
    assert_eq!(first, ["a0", "a1", "a2", "b0"]);
    let token = token.expect("more");
    assert_eq!(token, "1:b1");
    let (second, token) = walk(&all, decode_position(&token).ok(), 4).await;
    assert_eq!(second, ["b1", "b2", "c0", "c1"]);
    let token = token.expect("more");
    let (third, token) = walk(&all, decode_position(&token).ok(), 4).await;
    assert_eq!(third, ["c2"]);
    assert_eq!(token, None, "the last page says it is the last");
    // A page that ends exactly where an instance does goes on at the next instance.
    let (exact, token) = walk(&all, None, 3).await;
    assert_eq!(exact, ["a0", "a1", "a2"]);
    assert_eq!(token.as_deref(), Some("1:b"));
}

/// Catches: a position held across a change of instances: a cursor made for an instance
/// that is not in the list now, used on the instance that sorts after it (another
/// instance's rows skipped from a cursor that means nothing there), and a position past the
/// last instance read as the first.
#[tokio::test]
async fn a_position_in_a_vanished_instance_starts_the_next_one() {
    let all = ["a", "c"];
    let (got, _) = walk(&all, at("b", Some("2")), 10).await;
    assert_eq!(got, ["c0", "c1", "c2"]);
    let (got, _) = walk(&all, at("a", Some("2")), 10).await;
    assert_eq!(got, ["a2", "c0", "c1", "c2"]);
    let (got, token) = walk(&all, at("d", None), 10).await;
    assert!(got.is_empty() && token.is_none());
}

/// Catches: a filter that matches nothing making one request read every instance of a
/// large farm: the walk stops after [`MAX_FETCHES`] pages with a position to go on from.
#[tokio::test]
async fn a_walk_reads_a_bounded_number_of_pages() {
    let names: Vec<String> = (0..MAX_FETCHES + 4).map(|i| format!("i{i:02}")).collect();
    let started = std::sync::atomic::AtomicUsize::new(0);
    let (got, token) = across(&names, None, 5, |_, _, _| {
        started.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        std::future::ready(Ok::<(Vec<u8>, _), ReadError>((Vec::new(), None)))
    })
    .await
    .expect("a walk");
    assert!(got.is_empty());
    assert_eq!(started.into_inner(), MAX_FETCHES);
    assert_eq!(token, Some(format!("3:i{MAX_FETCHES:02}")));
}

/// Catches: a failed read that is a short page (rows missing and no sign of it): it is
/// `503`.
#[tokio::test]
async fn a_failed_read_is_503() {
    let names = ["a".to_owned()];
    let failed = across(&names, None, 5, |_, _, _| {
        std::future::ready(Err::<(Vec<u8>, Option<PageCursor>), _>(ReadError::Row {
            key: "k".to_owned(),
            message: "m".to_owned(),
        }))
    })
    .await;
    assert_eq!(
        failed.err().map(|r| r.status()),
        Some(StatusCode::SERVICE_UNAVAILABLE)
    );
}

/// Catches: a position that breaks on a name with a `:`, a digit, a multibyte letter or
/// nothing at all, and one that reads back as another place.
#[test]
fn a_position_reads_back_as_written() {
    for name in ["main", "a:1/x", "7:x", "", "caf\u{e9}", "12"] {
        for c in [None, Some(cursor("00000000a1b2/key"))] {
            let at = (name.to_owned(), c);
            let text = encode_position(&at);
            assert_eq!(decode_position(&text).ok(), Some(at), "{text:?}");
        }
    }
    assert_eq!(
        encode_position(&("ab".to_owned(), Some(cursor("7")))),
        "2:ab7"
    );
}

/// Catches: a key that is read loosely: upper case, short, long, with a sign or a prefix.
#[test]
fn a_shape_key_is_sixteen_lowercase_hex_digits() {
    assert_eq!(
        parse_key("0123456789abcdef").ok(),
        Some(0x0123_4567_89ab_cdef)
    );
    for bad in [
        "",
        "0123456789ABCDEF",
        "123",
        "0123456789abcdef0",
        "+123456789abcdef",
        "0x23456789abcdef",
    ] {
        assert!(parse_key(bad).is_err(), "{bad:?}");
    }
}

/// Catches: a class of a ring's run named as another: they are the API's words.
#[test]
fn a_ring_run_has_the_apis_class_name() {
    for (status, name) in [
        (TestStatus::Unspecified, "unspecified"),
        (TestStatus::Pass, "pass"),
        (TestStatus::Fail, "fail"),
        (TestStatus::Timeout, "timeout"),
        (TestStatus::OomKilled, "oom_killed"),
        (TestStatus::Infra, "infra"),
    ] {
        assert_eq!(class(status), name);
    }
}

fn found(catalog: Option<ShapeRow>, status: Option<StatusRow>) -> Found {
    Found {
        instance: "a b".to_owned(),
        shape: 0xab,
        cfg: "c/1".to_owned(),
        catalog,
        status,
    }
}

fn catalog(kind: ShapeKind, target: &str) -> ShapeRow {
    ShapeRow {
        kind: kind.into(),
        first_seen_ms: 1_000,
        last_seen_ms: 2_000,
        target: target.to_owned(),
    }
}

fn mark(id: u8) -> Mark {
    Mark {
        inv_key: String::new(),
        time_ms: 3_000,
        run_id: vec![id; 12],
    }
}

/// Catches: a row of an answer that says a test is not one (or the reverse) when one of
/// its two rows is missing, an empty target shown as a label, a link with its instance or
/// configuration unescaped, a status with no non-infrastructure run shown as a status, a
/// proof shown on a test that has none, and the last time told from the catalog when the
/// status row has a later one.
#[test]
fn a_row_is_a_test_if_either_row_says_so() {
    // A catalog row alone: its kind decides, the target is null when it is empty.
    let other = render(&found(Some(catalog(ShapeKind::Other, "")), None));
    assert_eq!(other["kind"], "other");
    assert_eq!(other["target"], Value::Null);
    assert_eq!(other["last_seen"], "1970-01-01T00:00:02.000000Z");
    assert!(other.get("status").is_none() && other.get("first_fail").is_none());
    assert_eq!(
        other["href"],
        "/v1/shapes/00000000000000ab?instance=a+b&cfg=c%2F1"
    );
    assert_eq!(
        other["runs_href"],
        "/v1/shapes/00000000000000ab/runs?instance=a+b&cfg=c%2F1"
    );
    let test_without_status = render(&found(Some(catalog(ShapeKind::Test, "//t:x")), None));
    assert_eq!(test_without_status["kind"], "test");
    assert_eq!(test_without_status["target"], "//t:x");
    // A status row alone (its catalog row is gone): a test, with no first seen.
    let status = StatusRow {
        last_outcome: kbf_history::records::TestStatus::Infra.into(),
        last_seen_ms: 5_000,
        ..StatusRow::default()
    };
    let infra_only = render(&found(None, Some(status.clone())));
    assert_eq!(infra_only["kind"], "test");
    assert_eq!(infra_only["status"], Value::Null, "no result yet");
    assert!(infra_only.get("first_seen").is_none());
    assert_eq!(
        infra_only["last_seen"], "1970-01-01T00:00:05.000000Z",
        "the later of the two"
    );
    assert_eq!(
        infra_only["flaky"],
        json!({"proven": false, "proof_digest": null, "since": null})
    );
    // A proven flake that is passing now.
    let flaky = StatusRow {
        last_outcome: kbf_history::records::TestStatus::Pass.into(),
        proven: true,
        proof_digest: 0xfeed,
        proven_since: Some(mark(2)),
        last_pass: Some(mark(2)),
        ..StatusRow::default()
    };
    assert_eq!(flaky.kind(), Some(StatusKind::Flaky));
    let row = render(&found(Some(catalog(ShapeKind::Test, "")), Some(flaky)));
    assert_eq!(row["status"], "flaky");
    assert_eq!(row["flaky"]["proof_digest"], "000000000000feed");
    assert_eq!(row["flaky"]["since"]["run_id"], "02".repeat(12));
    assert_eq!(
        row["last_pass"]["run_href"],
        format!("/v1/runs/{}", "02".repeat(12))
    );
    assert_eq!(
        row["last_seen"], "1970-01-01T00:00:02.000000Z",
        "the status row has no later"
    );
}

/// Catches: a ring run with a digest shown when it has none (a zero shown as a digest that
/// proves nothing), a digest cut short, and the run's links not made from its id.
#[test]
fn a_ring_run_shows_its_digest_only_when_it_has_one() {
    let with = RingRun {
        run_id: vec![0xab; 12],
        status: kbf_history::records::TestStatus::Timeout.into(),
        execution_ms: 12,
        digest16: 0x0123_4567_89ab_cdef,
    };
    let run = ring_run(&with);
    assert_eq!(run["digest16"], "0123456789abcdef");
    assert_eq!(run["class"], "timeout");
    assert_eq!(run["run_href"], format!("/v1/runs/{}", "ab".repeat(12)));
    let without = ring_run(&RingRun {
        digest16: 0,
        ..with
    });
    assert!(without.get("digest16").is_none(), "{without}");
}
