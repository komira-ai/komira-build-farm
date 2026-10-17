//! Route matching, and what a read answers for each way it can fail.

use futures::executor::block_on;
use serde_json::Value;

use super::{Page, ReadError, Reader, error_message, matches};
use crate::tests::Fake;

/// Catches: a pattern segment matching more or less than one non-empty segment
/// (mutants: each condition of the `{name}` guard dropped, a prefix match), or a
/// dot segment, in any spelling, matching one (mutant: the dot check dropped); a
/// path with a backslash, whitespace or a control character matching (mutant: the
/// character check dropped).
#[test]
fn a_pattern_matches_segment_by_segment() {
    let cases = [
        ("/v1/invocations/{id}", "/v1/invocations/b1", true),
        ("/v1/invocations/{id}", "/v1/invocations/", false),
        ("/v1/invocations/{id}", "/v1/invocations/b1/actions", false),
        ("/v1/invocations/{id}/actions", "/v1/invocations/b1", false),
        ("/v1/invocations/{id}", "/v1/invocations/{id}", true),
        ("/v1/invocations/{id", "/v1/invocations/b1", false),
        ("/v1/invocations/id}", "/v1/invocations/b1", false),
        ("/v1/blobs/{digest}", "/v2/blobs/x", false),
        // Dot segments in every spelling a URL client resolves (WHATWG URL 4.4).
        ("/v1/invocations/{id}", "/v1/invocations/.", false),
        ("/v1/invocations/{id}", "/v1/invocations/..", false),
        ("/v1/invocations/{id}", "/v1/invocations/%2e", false),
        ("/v1/invocations/{id}", "/v1/invocations/%2E%2e", false),
        ("/v1/invocations/{id}", "/v1/invocations/.%2E", false),
        ("/v1/invocations/{id}", "/v1/invocations/%2e.", false),
        (
            "/v1/invocations/{id}/actions",
            "/v1/invocations/../actions",
            false,
        ),
        // A backslash (a WHATWG URL client reads it as `/` in a path), whitespace or a
        // control character: refused, as `Href::root_relative` refuses them.
        ("/v1/blobs/{digest}", "/v1/blobs/..\\x", false),
        ("/v1/blobs/{digest}", "/v1/blobs/a\\b", false),
        ("/v1/blobs/{digest}", "/v1/blobs/a b", false),
        ("/v1/blobs/{digest}", "/v1/blobs/a\tb", false),
        ("/v1/blobs/{digest}", "/v1/blobs/a\u{7f}b", false),
        // Not dot segments: matched as any other id.
        ("/v1/invocations/{id}", "/v1/invocations/...", true),
        ("/v1/invocations/{id}", "/v1/invocations/.a", true),
        ("/v1/invocations/{id}", "/v1/invocations/%252e", true),
    ];
    for (pattern, path, want) in cases {
        assert_eq!(matches(pattern, path), want, "{pattern} {path}");
    }
    let page = Page {
        name: "p",
        reads: &["/v1/a/{x}", "/v1/b"],
    };
    assert!(page.declares("/v1/b?x=1"));
    assert!(page.declares("/v1/a/y"));
    assert!(!page.declares("/v1/c"));
}

const PAGE: Page = Page {
    name: "p",
    reads: &["/v1/doc"],
};

/// Catches: a failed read answered as a document, or with another status than its
/// kind's (mutants: a non-200 status treated as success; `status` mapping changed).
#[test]
fn each_failure_has_its_kind_and_status() {
    let fake = Fake::default().down("/v1/doc");
    let e = block_on(Reader::new(&fake, &PAGE).json::<Value>("/v1/doc")).expect_err("down");
    assert!(matches!(&e, ReadError::Unreachable { .. }), "{e}");
    assert_eq!((e.status(), e.is_page_bug()), (502, false));
    assert_eq!(e.to_string(), "/v1/doc: connection refused");

    let fake = Fake::default().with("/v1/doc", 200, "not json");
    let e = block_on(Reader::new(&fake, &PAGE).json::<Value>("/v1/doc")).expect_err("malformed");
    assert!(matches!(&e, ReadError::Malformed { .. }), "{e}");
    assert_eq!(e.status(), 502);

    let fake = Fake::default().with("/v1/doc", 403, r#"{"error": "denied"}"#);
    let e = block_on(Reader::new(&fake, &PAGE).text("/v1/doc")).expect_err("refused");
    assert_eq!(e.to_string(), "/v1/doc answered 403: denied");
    assert_eq!(e.status(), 403);

    let fake = Fake::default().with("/v1/doc", 200, b"a\xffb".to_vec());
    let text = block_on(Reader::new(&fake, &PAGE).text("/v1/doc")).expect("text");
    assert_eq!(text, "a\u{fffd}b");
}

/// Catches: an API error body shown as raw JSON when it has an `error`, or the
/// `records_since` of a missing record dropped.
#[test]
fn an_error_body_is_shown_by_its_error_string() {
    assert_eq!(error_message(br#"{"error": "denied"}"#), "denied");
    assert_eq!(
        error_message(br#"{"error": "no_record", "records_since": "2026-10-11T09:00:00Z"}"#),
        "no_record: no record of it is held; records are held since 2026-10-11T09:00:00Z"
    );
    assert_eq!(error_message(br#"{"other": 1}"#), r#"{"other": 1}"#);
    assert_eq!(error_message(b"plain"), "plain");
}
