//! What each page shows for each answer the read API can give.

use serde_json::json;

use super::{action_label, attempt_end, bytes, micros, outcome};
use crate::api::{Attempt, End, Outcome, Text};
use crate::tests::{
    Fake, ID, STDERR, action, action_digest, body, digest, farm, invocation, render,
};

fn page(fake: &Fake, path: &str) -> (u16, String) {
    let a = render(fake, path);
    (a.status(), body(&a))
}

/// Catches: the lookup form missing, a read made without an id, the footer not
/// saying nothing was read.
#[test]
fn builds_without_an_id_shows_the_form_and_reads_nothing() {
    let fake = farm();
    for path in ["/ui", "/ui?id="] {
        let (status, html) = page(&fake, path);
        assert_eq!(status, 200);
        assert!(
            html.contains("<form method=\"get\" action=\"/ui\""),
            "{html}"
        );
        assert!(html.contains("Nothing: this page read no route."), "{html}");
        assert!(
            html.contains("aria-current=\"page\""),
            "Builds is the current section"
        );
        assert!(html.contains("<title>Builds - kbf</title>"), "{html}");
        assert!(html.contains("href=\"/ui/static/kbf.css\""), "{html}");
    }
    assert!(fake.seen().is_empty());
}

/// Catches: the summary not shown, not linked to the Build page, or the id not kept
/// in the form; a field or counter shown under another's name (mutants: the
/// uploaded and downloaded values swapped, first and last seen swapped), since every
/// one has its own value in the fixture and is checked with its name.
#[test]
fn builds_with_an_id_shows_its_summary() {
    let fake = farm();
    let (status, html) = page(&fake, &format!("/ui?id={ID}"));
    assert_eq!(status, 200, "{html}");
    assert!(
        html.contains("<a href=\"/ui/builds/build-1\">build-1</a>"),
        "{html}"
    );
    assert!(html.contains("value=\"build-1\""), "{html}");
    for (name, value) in [
        ("State", "idle, complete"),
        ("Tool", "buck2 2026.10.01"),
        ("Instances", "main, edge"),
        ("First seen", "2026-10-11T09:00:00.000000Z"),
        ("Last seen", "2026-10-11T09:01:00.000000Z"),
        ("Actions", "2"),
    ] {
        let pair = format!("<dt>{name}</dt><dd>{value}</dd>");
        assert!(html.contains(&pair), "{pair}: {html}");
    }
    for (value, label) in [
        ("2", "ran"),
        ("1", "failed"),
        ("3", "joined"),
        ("4", "execute"),
        ("5", "cache hit execute"),
        ("6", "AC hit"),
        ("7", "AC miss"),
        ("2.0 KiB", "uploaded"),
        ("10 B", "downloaded"),
    ] {
        let cell = format!(
            "<li><span class=\"n\">{value}</span><span class=\"label\">{label}</span></li>"
        );
        assert!(html.contains(&cell), "{cell}: {html}");
    }
    assert!(
        html.contains(
            "<link rel=\"alternate\" type=\"application/json\" href=\"/v1/invocations/build-1\">"
        ),
        "{html}"
    );
}

/// Catches: a build the API has no record of answered `200`, or without when records
/// start; an API that cannot be reached answered other than `502`.
#[test]
fn builds_with_an_unknown_id_or_no_api_says_so() {
    let fake = Fake::default()
        .with(
            "/v1/invocations/gone",
            404,
            r#"{"error": "no_record", "id": "gone", "records_since": "2026-10-11T09:00:00.000000Z"}"#,
        )
        .down("/v1/invocations/down");
    let (status, html) = page(&fake, "/ui?id=gone");
    assert_eq!(status, 404);
    assert!(
        html.contains("records are held since 2026-10-11T09:00:00.000000Z"),
        "{html}"
    );
    let (status, html) = page(&fake, "/ui?id=down");
    assert_eq!(status, 502);
    assert!(html.contains("connection refused"), "{html}");
}

/// Catches: a `?id=` that decodes to `.` or `..` read as a build: written into
/// `/v1/invocations/{id}` it is a dot segment, which the reader refuses, so the page
/// answered `500` as a page bug (mutant: the dot check in `builds` dropped). It names
/// no build: `404` with the lookup form, nothing read.
#[test]
fn builds_with_a_dot_id_names_no_build_and_reads_nothing() {
    let fake = farm();
    for path in ["/ui?id=..", "/ui?id=.", "/ui?id=%2E%2E"] {
        let (status, html) = page(&fake, path);
        assert_eq!(status, 404, "{path}: {html}");
        assert!(!html.contains("problem bug"), "{path}: {html}");
        assert!(
            html.contains("<form method=\"get\" action=\"/ui\""),
            "{path}: {html}"
        );
        assert!(html.contains("names no build"), "{path}: {html}");
        assert!(
            html.contains("Nothing: this page read no route."),
            "{path}: {html}"
        );
    }
    assert!(fake.seen().is_empty());
}

/// Catches: an outcome tag this page does not know (the server adds outcomes,
/// `docs/design/ui-data.md` 6.1) failing the whole page as a malformed answer
/// (mutant: the `Other` fallback dropped); a known tag with the wrong shape, or an
/// outcome that is not one tag, rendered as a guess rather than refused; no outcome
/// yet (`null`) not shown as pending; each on the Build page's rows and on the Action
/// page, which reads the row flattened into its answer.
#[test]
fn an_outcome_of_a_tag_the_page_does_not_know_is_shown_by_its_tag() {
    let base = format!("/v1/invocations/{ID}");
    let with_outcome = |outcome: serde_json::Value| {
        Fake::default().json(&base, &invocation(ID)).json(
            &format!("{base}/actions"),
            &json!({"actions": [crate::tests::row(&digest('b', 10), "//app:x", &outcome)]}),
        )
    };
    let (status, html) = page(
        &with_outcome(json!({"lost": {}})),
        &format!("/ui/builds/{ID}"),
    );
    assert_eq!(status, 200, "{html}");
    assert!(
        html.contains("<span class=\"pill other\">lost</span>"),
        "{html}"
    );
    for malformed in [
        json!({"ok": {}}),
        json!({"ok": {"exit_code": 0}, "lost": {}}),
        json!("lost"),
    ] {
        let (status, html) = page(
            &with_outcome(malformed.clone()),
            &format!("/ui/builds/{ID}"),
        );
        assert_eq!(status, 502, "{malformed}: {html}");
    }
    // No outcome yet (`null`) is pending, not malformed.
    let (status, html) = page(&with_outcome(json!(null)), &format!("/ui/builds/{ID}"));
    assert_eq!(status, 200, "{html}");
    assert!(
        html.contains("<span class=\"pill pending\">pending</span>"),
        "{html}"
    );

    // The Action page reads the row flattened into its answer: the same outcomes.
    let url = format!("{base}/actions/{}", action_digest());
    for (outcome, pill) in [
        (
            json!({"lost": {}}),
            "<span class=\"pill other\">lost</span>",
        ),
        (json!(null), "<span class=\"pill pending\">pending</span>"),
    ] {
        let mut a = action(&json!(null));
        a["outcome"] = outcome.clone();
        let fake = Fake::default().json(&url, &a);
        let (status, html) = page(
            &fake,
            &format!("/ui/builds/{ID}/actions/{}", action_digest()),
        );
        assert_eq!(status, 200, "{outcome}: {html}");
        assert!(html.contains(pill), "{outcome}: {html}");
    }
}

/// Catches: rows not shown or not linked to their Action page, the next-page link
/// missing or not carrying the token, the token not passed to the API; a row's
/// first attempt shown as its last, or its attempt count wrong (mutant:
/// `attempts.first()` for `attempts.last()`), seen on a row requeued once.
#[test]
fn build_shows_its_actions_and_pages_through_them() {
    let fake = farm();
    let (status, html) = page(&fake, &format!("/ui/builds/{ID}"));
    assert_eq!(status, 200, "{html}");
    let action = format!("/ui/builds/{ID}/actions/{}", action_digest());
    assert!(
        html.contains(&format!(
            "<a href=\"{action}\">//app:&lt;b&gt;lib&lt;/b&gt;</a>"
        )),
        "{html}"
    );
    assert!(html.contains("<span class=\"pill ok\">ok</span>"), "{html}");
    assert!(
        html.contains("<span class=\"pill fail\">failed (code 2)"),
        "{html}"
    );
    assert!(
        html.contains("<td>failed: exit (exit 1) on n4</td>"),
        "{html}"
    );
    assert!(
        html.contains("<td>ran</td><td>2</td><td>failed: exit on n4</td><td>main</td>"),
        "{html}"
    );
    assert!(!html.contains("on n3"), "{html}");
    assert!(
        html.contains("<a href=\"/ui/builds/build-1?page_token=t%2B1\">Next page</a>"),
        "{html}"
    );
    assert!(!html.contains("aria-current"), "Builds is not this page");

    let (status, html) = page(&fake, &format!("/ui/builds/{ID}?page_token=t%2B1"));
    assert_eq!(status, 200, "{html}");
    assert!(html.contains("No actions on this page."), "{html}");
    assert!(!html.contains("Next page"), "{html}");
    assert_eq!(
        fake.seen().last().map(String::as_str),
        Some("/v1/invocations/build-1/actions?page_token=t%2B1")
    );
}

/// Catches: a failed summary read still reading `/actions`, or a document the page
/// cannot read answered `200`.
#[test]
fn build_answers_the_api_status_or_502() {
    let base = format!("/v1/invocations/{ID}");
    let fake = Fake::default().with(&base, 403, r#"{"error": "not allowed"}"#);
    let (status, html) = page(&fake, &format!("/ui/builds/{ID}"));
    assert_eq!(status, 403);
    assert!(html.contains("answered 403: not allowed"), "{html}");
    assert_eq!(fake.seen(), std::slice::from_ref(&base));

    let fake = Fake::default()
        .json(&base, &invocation(ID))
        .json(&format!("{base}/actions"), &json!({"rows": []}));
    let (status, html) = page(&fake, &format!("/ui/builds/{ID}"));
    assert_eq!(status, 502);
    assert!(html.contains("a document this page cannot read"), "{html}");
}

/// Catches: the action's fields, attempts, usage, outputs or logs not shown, or a
/// field or attempt cell shown under another's name (mutant: CPU user showing the
/// system time), an output the store no longer holds still linked, a log the store
/// no longer holds read.
#[test]
fn action_shows_its_answer_and_its_logs() {
    let fake = farm();
    let path = format!("/ui/builds/{ID}/actions/{}", action_digest());
    let (status, html) = page(&fake, &path);
    assert_eq!(status, 200, "{html}");
    assert!(
        html.contains("<a href=\"/ui/builds/build-1\">build-1</a>"),
        "{html}"
    );
    let (this, command) = (action_digest(), digest('c', 90));
    for (name, value) in [
        ("Action", this.as_str()),
        ("Instance", "main"),
        ("How", "ran"),
        ("Command", command.as_str()),
        ("Wall", "4.00 s"),
        ("CPU user", "1.50 s"),
        ("CPU system", "2.5 ms"),
        ("Peak memory", "3.0 GiB"),
    ] {
        let pair = format!("<dt>{name}</dt><dd>{value}</dd>");
        assert!(html.contains(&pair), "{pair}: {html}");
    }
    assert!(
        html.contains(
            "<tr><td>3.1</td><td>n4</td><td>2026-10-11T09:00:01.000000Z</td>\
             <td>2026-10-11T09:00:03.000000Z</td><td>2026-10-11T09:00:04.000000Z</td>\
             <td>failed: exit (exit 1)</td></tr>"
        ),
        "{html}"
    );
    let lib = format!(
        "/v1/blobs/{}?instance=main&amp;name=lib.o",
        digest('d', 300)
    );
    assert!(
        html.contains(&format!(
            "<a href=\"{lib}\">out/lib.o</a></code> file 300 B"
        )),
        "{html}"
    );
    assert!(
        html.contains("<code>out/gen</code> directory 70 B"),
        "{html}"
    );
    assert!(html.contains("the list is cut"), "{html}");
    assert!(
        html.contains("No longer held by the store."),
        "stdout: {html}"
    );
    assert!(html.contains(&crate::html::escape(STDERR)), "{html}");
    // The stdout blob is gone: never asked for.
    assert_eq!(fake.seen().len(), 2, "{:?}", fake.seen());

    // The same outputs, all of them kept: listed, with no note that the list is cut.
    let mut doc = action(&json!(null));
    doc["outputs_truncated"] = json!(false);
    let base = format!("/v1/invocations/{ID}/actions/{}", action_digest());
    let fake = Fake::default().json(&base, &doc);
    let (status, html) = page(&fake, &path);
    assert_eq!(status, 200, "{html}");
    assert!(
        html.contains("<code>out/gen</code> directory 70 B"),
        "{html}"
    );
    assert!(!html.contains("the list is cut"), "{html}");
}

/// Catches: a large log read into the page, a log linked when the blob route is off,
/// an empty answer shown as an error, a log read that failed failing the page.
#[test]
fn action_logs_in_each_state() {
    let base = format!("/v1/invocations/{ID}/actions/{}", action_digest());
    let path = format!("/ui/builds/{ID}/actions/{}", action_digest());
    let big = digest('9', 1 << 20);
    let big_href = format!("/v1/blobs/{big}?instance=main&name=stderr");
    let cases = [
        (
            json!({"digest": big, "available": true, "href": big_href}),
            format!(
                "1.0 MiB is too large to show here. <a href=\"{}\">Open the whole log</a>",
                crate::html::escape(&big_href)
            ),
        ),
        (
            json!({"digest": "not-a-size", "available": null, "href": "https://x.example/"}),
            "Its size is not known, so it is not shown here. </p>".to_owned(),
        ),
        (
            json!({"digest": digest('9', 5), "available": true, "href": null}),
            "Not linked: the blob route is off".to_owned(),
        ),
        (json!(null), "None recorded.".to_owned()),
        (
            json!({"digest": digest('9', 5), "available": null, "href": "/v1/blobs/missing"}),
            "/v1/blobs/missing answered 404: no such route".to_owned(),
        ),
        // Sent (its path is declared) but no link: browsers rewrite the space.
        (
            json!({"digest": digest('9', 5), "available": true, "href": "/v1/blobs/x?a b"}),
            "<li><code>/v1/blobs/x?a b</code></li>".to_owned(),
        ),
    ];
    for (stderr, want) in cases {
        let mut doc = action(&stderr);
        doc["usage"] = json!(null);
        doc["outputs"] = json!([]);
        doc["outputs_truncated"] = json!(false);
        doc["attempts"] = json!([]);
        doc["command"] = json!(null);
        let fake = Fake::default().json(&base, &doc);
        let (status, html) = page(&fake, &path);
        assert_eq!(status, 200, "{html}");
        assert!(html.contains(&want), "{want}: {html}");
        assert!(html.contains("<dd>none</dd>"), "{html}");
        assert!(html.contains("None: the action did not run here"), "{html}");
        assert!(!html.contains("Wall"), "{html}");
        assert!(!html.contains("the list is cut"), "{html}");
    }
}

#[test]
fn action_not_found_is_404() {
    let fake = Fake::default();
    let (status, html) = page(&fake, &format!("/ui/builds/{ID}/actions/x"));
    assert_eq!(status, 404);
    assert!(html.contains("no such route"), "{html}");
}

/// Catches: unit boundaries off by one, the largest unit overflowing its table.
#[test]
fn sizes_and_durations() {
    assert_eq!(bytes(0), "0 B");
    assert_eq!(bytes(1023), "1023 B");
    assert_eq!(bytes(1024), "1.0 KiB");
    assert_eq!(bytes(1536), "1.5 KiB");
    assert_eq!(bytes(1 << 30), "1.0 GiB");
    assert_eq!(bytes(u64::MAX), "16384.0 PiB");
    assert_eq!(micros(999_999), "1000.0 ms");
    assert_eq!(micros(1_000_000), "1.00 s");
    assert_eq!(micros(1_500), "1.5 ms");
}

fn text(value: &str, truncated: bool) -> Text {
    Text {
        value: value.to_owned(),
        truncated,
    }
}

#[test]
fn an_action_is_named_by_target_then_mnemonic_then_digest() {
    let (t, m) = (text("//a:b", false), text("Cxx", true));
    assert_eq!(action_label(Some(&t), Some(&m), "d"), "//a:b");
    assert_eq!(action_label(None, Some(&m), "d"), "Cxx...");
    assert_eq!(action_label(None, None, "d"), "d");
}

#[test]
fn outcomes_in_words() {
    let cases = [
        (None, "pending", "pending"),
        (Some(Outcome::Ok { exit_code: 0 }), "ok", "ok"),
        (Some(Outcome::Ok { exit_code: 3 }), "exit 3", "fail"),
        (Some(Outcome::TimedOut {}), "timed out", "fail"),
        (Some(Outcome::Other("lost".to_owned())), "lost", "other"),
        (
            Some(Outcome::Refused {
                reason: "too big".to_owned(),
            }),
            "refused: too big",
            "fail",
        ),
    ];
    for (o, words, class) in cases {
        assert_eq!(outcome(o.as_ref()), (words.to_owned(), class));
    }
}

#[test]
fn attempt_ends_in_words() {
    let attempt = |end: Option<End>| Attempt {
        lease: "1.1".to_owned(),
        worker: "n1".to_owned(),
        queued: String::new(),
        started: None,
        ended: None,
        end,
    };
    let end = |result: Option<&str>,
               requeue: Option<&str>,
               failure: Option<&str>,
               reason: Option<&str>| {
        Some(End {
            result: result.map(str::to_owned),
            requeue: requeue.map(str::to_owned),
            failure: failure.map(str::to_owned),
            reason: reason.map(str::to_owned),
        })
    };
    let cases = [
        (None, "running"),
        (end(Some("ok"), None, None, None), "ok"),
        (
            end(None, Some("out_of_memory"), None, Some("raised")),
            "requeued: out_of_memory (raised)",
        ),
        (end(None, None, Some("exit"), None), "failed: exit"),
        (end(None, None, None, None), "ended"),
    ];
    for (e, want) in cases {
        assert_eq!(attempt_end(&attempt(e)), want);
    }
}

/// Catches: a summary that names a tool version without a tool, or drops the state
/// flags.
#[test]
fn summary_tool_and_state() {
    let base = format!("/v1/invocations/{ID}");
    for (tool, version, capped, want_tool, want_state) in [
        (
            json!("buck2"),
            json!(null),
            true,
            "buck2",
            "idle, complete, rows capped",
        ),
        (json!(null), json!("1"), false, "unknown", "idle, complete"),
    ] {
        let mut inv = invocation(ID);
        inv["tool"] = tool;
        inv["tool_version"] = version;
        inv["rows_capped"] = json!(capped);
        let fake = Fake::default().json(&base, &inv);
        let (_, html) = page(&fake, &format!("/ui?id={ID}"));
        assert!(html.contains(&format!("<dd>{want_tool}</dd>")), "{html}");
        assert!(html.contains(&format!("<dd>{want_state}</dd>")), "{html}");
    }
    let mut inv = invocation(ID);
    inv["complete"] = json!(false);
    let fake = Fake::default().json(&base, &inv);
    let (_, html) = page(&fake, &format!("/ui?id={ID}"));
    assert!(html.contains("<dd>idle</dd>"), "{html}");
}

/// Catches: the size bound off by one (mutant: `<` for `<=`), so a log of exactly
/// the bound is a link, or one byte over it is read into the page.
#[test]
fn a_log_of_the_bound_is_shown_and_one_byte_more_is_a_link() {
    let base = format!("/v1/invocations/{ID}/actions/{}", action_digest());
    let path = format!("/ui/builds/{ID}/actions/{}", action_digest());
    for (size, read) in [
        (super::LOG_SHOWN_MAX_BYTES, true),
        (super::LOG_SHOWN_MAX_BYTES + 1, false),
    ] {
        let d = digest('9', size);
        let href = format!("/v1/blobs/{d}?instance=main&name=stderr&inline=text");
        let stderr = json!({"digest": d, "available": true, "href": href});
        let fake = Fake::default().json(&base, &action(&stderr)).with(
            &href,
            200,
            "x".repeat(usize::try_from(size).expect("fits")),
        );
        let (status, html) = page(&fake, &path);
        assert_eq!(status, 200);
        assert_eq!(fake.seen().len(), if read { 2 } else { 1 }, "{size}");
        assert_eq!(html.contains("too large to show here"), !read, "{size}");
    }
}
