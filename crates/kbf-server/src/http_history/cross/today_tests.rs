//! The digest and class a request names, and the answer for a page. Each test names the
//! defect it catches.

use kbf_history::records::RunOutcome;
use kbf_history::{RunId, StreamId, TodayPage, TodayRun};
use serde_json::{Value, json};

use super::{CLASSES, item, paging, parse_digest, render};
use crate::http_history::query::Query;

fn run(shape: u64) -> TodayRun {
    TodayRun {
        instance: "main".into(),
        run_id: RunId::new(1_000, StreamId(1), 2),
        shape,
        cfg: "c".into(),
        outcome: RunOutcome::Failed,
        node: "n1".into(),
        started_ms: 990,
        ended_ms: 1_000,
        action: None,
    }
}

/// Catches: a digest read loosely (upper case, a short hash, a size with a sign or a
/// leading zero, no size): it is the one a client got from the API, written the same way.
#[test]
fn a_digest_is_the_hash_and_the_size_as_the_api_writes_it() {
    let hash = "ab".repeat(32);
    assert_eq!(
        parse_digest(&format!("{hash}-17")).ok(),
        Some((vec![0xab; 32], 17))
    );
    assert_eq!(
        parse_digest(&format!("{hash}-0")).ok(),
        Some((vec![0xab; 32], 0))
    );
    for bad in [
        String::new(),
        hash.clone(),
        format!("{hash}-"),
        format!("{hash}-+1"),
        format!("{hash}-01"),
        format!("{hash}--1"),
        format!("{}-1", hash.to_uppercase()),
        format!("{}-1", "ab".repeat(31)),
        format!("{}-1", "zz".repeat(32)),
    ] {
        assert!(parse_digest(&bad).is_err(), "{bad:?}");
    }
}

/// Catches: a class that is not one of the API's words taken as no filter, and a class
/// name that reads as another (every class has its own).
#[test]
fn a_class_is_one_of_the_apis_words() {
    let known = ["class", "page_size", "page_token"];
    let q = |text: &str| Query::parse(text, &known).expect("a query");
    for (outcome, name) in CLASSES.into_iter().zip([
        "unspecified",
        "ok",
        "failed",
        "timed_out",
        "memory_killed",
        "infra",
        "lost",
        "cancelled",
    ]) {
        let (_, _, wanted) = paging(&q(&format!("class={name}")), "/r").expect("a class");
        assert_eq!(wanted, Some(outcome), "{name}");
    }
    let (size, after, wanted) = paging(&q(""), "/r").expect("no filter");
    assert_eq!((size, after, wanted), (100, None, None));
    assert!(paging(&q("class=exploded"), "/r").is_err());
    assert!(paging(&q("page_size=0"), "/r").is_err());
}

/// Catches: a position that is not a number taken as the start, and a token made for
/// another route used here.
#[test]
fn a_position_is_the_number_a_page_named() {
    let known = ["page_size", "page_token"];
    let first = Query::parse("", &known).expect("a query");
    let token = first.token("/r", "41");
    let next = Query::parse(&format!("page_token={token}"), &known).expect("a query");
    assert_eq!(paging(&next, "/r").ok().map(|p| p.1), Some(Some(41)));
    assert!(paging(&next, "/other").is_err());
    let words =
        Query::parse(&format!("page_token={}", first.token("/r", "x")), &known).expect("a query");
    assert!(paging(&words, "/r").is_err());
}

/// Catches: a run shown without the key of its shape (or a shape that is none shown as
/// a key of zeros), an end or a start in another unit, and a run with no link.
#[test]
fn a_run_is_listed_with_its_links_times_and_shape() {
    let shaped = item(&run(0xab));
    let id = RunId::new(1_000, StreamId(1), 2).to_string();
    assert_eq!(shaped["run_id"], id);
    assert_eq!(shaped["run_href"], format!("/v1/runs/{id}"));
    assert_eq!(shaped["class"], "failed");
    assert_eq!(shaped["worker"], "n1");
    assert_eq!(shaped["instance"], "main");
    assert_eq!(
        shaped["shape"],
        json!({"key": "00000000000000ab", "cfg": "c"})
    );
    assert_eq!(shaped["started"], "1970-01-01T00:00:00.990000Z");
    assert_eq!(shaped["ended"], "1970-01-01T00:00:01.000000Z");
    assert_eq!(item(&run(0))["shape"], Value::Null);
}

/// Catches: an answer that does not say it is partial, a start of coverage shown when the
/// index has none (and one that is not said when it has), and a next link that drops the
/// filters or the path it was asked on.
#[test]
fn a_page_says_it_is_partial_and_from_when_it_covers() {
    let known = ["class", "page_size", "page_token"];
    let q = Query::parse("class=failed&page_size=3", &known).expect("a query");
    let page = TodayPage {
        runs: vec![run(1)],
        next: Some(7),
        covers_from_ms: Some(2_000),
    };
    let answer = render(&page, &q, 3, "/v1/actions/d/runs", "/route");
    assert_eq!(answer["partial"], true);
    assert_eq!(answer["covers_from"], "1970-01-01T00:00:02.000000Z");
    assert_eq!(answer["runs"].as_array().map(Vec::len), Some(1));
    let next = answer["next_page"].as_str().expect("a next link");
    assert!(
        next.starts_with("/v1/actions/d/runs?class=failed&page_size=3&page_token="),
        "{next}"
    );
    let unknown = render(&TodayPage::default(), &q, 3, "/p", "/route");
    assert_eq!(unknown["partial"], true);
    assert!(unknown.get("covers_from").is_none() && unknown.get("next_page").is_none());
    assert_eq!(unknown["runs"], json!([]));
}
