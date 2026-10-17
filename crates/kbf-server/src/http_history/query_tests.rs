//! The query strings and page tokens. Each test names the defect it catches.

use axum::http::StatusCode;
use kbf_history::MAX_PAGE;
use kbf_history::records::InvState;

use super::{DEFAULT_PAGE, Query, at_ms};

const KNOWN: [&str; 5] = ["instance", "state", "since", "page_size", "page_token"];

fn query(text: &str) -> Query {
    Query::parse(text, &KNOWN).unwrap_or_else(|_| panic!("{text:?} parses"))
}

/// Catches: a name the route does not take that is ignored (a typo that filters
/// nothing and lists everything), a name given twice that takes either value, and values
/// that are not decoded (`+` and `%20` are spaces, `%2F` a slash).
#[test]
fn a_query_takes_known_names_once_and_decodes_them() {
    assert!(Query::parse("instance=a&bogus=1", &KNOWN).is_err());
    assert!(Query::parse("instance=a&instance=b", &KNOWN).is_err());
    let q = query("instance=a%2Fb+c&state=open");
    assert_eq!(q.get("instance"), Some("a/b c"));
    assert_eq!(q.get("state"), Some("open"));
    assert_eq!(q.get("since"), None);
    let Err(refusal) = Query::parse("bogus=1", &KNOWN) else {
        panic!("a name the route does not take is refused");
    };
    assert_eq!(refusal.status(), StatusCode::BAD_REQUEST);
}

/// Catches: a page size of 0 or past the reader's bound passed on (the reader would clamp
/// it quietly: the next link would say a size the page did not have), and a default that
/// is not the documented one.
#[test]
fn the_page_size_is_one_to_the_readers_bound() {
    assert_eq!(query("").page_size().ok(), Some(DEFAULT_PAGE));
    assert_eq!(query("page_size=1").page_size().ok(), Some(1));
    assert_eq!(
        query(&format!("page_size={MAX_PAGE}")).page_size().ok(),
        Some(MAX_PAGE)
    );
    for bad in ["0", "201", "-1", "x", ""] {
        assert!(
            query(&format!("page_size={bad}")).page_size().is_err(),
            "{bad:?}"
        );
    }
}

/// Catches: a token that works under other filters or on another route, a next link that
/// loses a filter or gains the old page's token, and filters that count by the order they
/// were written in (the same query written another way is the same query).
#[test]
fn a_token_names_its_route_filters_and_position() {
    let first = query("state=open&instance=a%2Fb&page_size=5");
    let token = first.token("/v1/x", "7:abc");
    let next = first.next_page("/v1/x", 5, &token);
    assert!(
        next.starts_with("/v1/x?instance=a%2Fb&state=open&page_size=5&page_token="),
        "{next}"
    );
    let second = query(next.split_once('?').expect("a query").1);
    assert_eq!(
        second.position("/v1/x").ok(),
        Some(Some("7:abc".to_owned()))
    );
    let written_otherwise = query(&format!("instance=a%2Fb&state=open&page_token={token}"));
    assert_eq!(
        written_otherwise.position("/v1/x").ok(),
        Some(Some("7:abc".to_owned()))
    );
    assert!(second.position("/v1/y").is_err(), "another route");
    let other_filter = query(&format!("state=closed&instance=a%2Fb&page_token={token}"));
    assert!(other_filter.position("/v1/x").is_err(), "another filter");
    let fewer = query(&format!("page_token={token}"));
    assert!(fewer.position("/v1/x").is_err(), "no filter");
    assert_eq!(
        first.position("/v1/x").ok(),
        Some(None),
        "no token, no position"
    );
}

/// Catches: a time read in the wrong zone or unit, a date alone or a time before the epoch
/// taken as a time, and the writer's own `at_ms` not reading back as it was written.
#[test]
fn times_are_rfc_3339_in_milliseconds() {
    let ms = |text: &str| query(&format!("since={text}")).time_ms("since").ok();
    assert_eq!(ms("2026-10-11T09:00:00Z"), Some(Some(1_791_709_200_000)));
    assert_eq!(
        ms("2026-10-11T10:00:00.250%2B01:00"),
        Some(Some(1_791_709_200_250))
    );
    assert_eq!(query("").time_ms("since").ok(), Some(None));
    for bad in ["2026-10-11", "yesterday", "1969-12-31T23:59:59Z", ""] {
        assert!(ms(bad).is_none(), "{bad:?}");
    }
    assert_eq!(at_ms(1_791_709_200_250), "2026-10-11T09:00:00.250000Z");
}

/// Catches: a state that is anything but open or closed passed on as no filter.
#[test]
fn a_state_is_open_or_closed() {
    assert_eq!(query("state=open").state().ok(), Some(Some(InvState::Open)));
    assert_eq!(
        query("state=closed").state().ok(),
        Some(Some(InvState::Closed))
    );
    assert_eq!(query("").state().ok(), Some(None));
    assert!(query("state=").state().is_err());
}
