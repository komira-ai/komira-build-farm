//! The pages over a fake read API: the security headers on every answer, escaping of
//! what the API sends, the page contract, and routing. The fixtures are the JSON the
//! `/v1` routes answer (`kbf-server`'s `tests/ui_pages.rs` renders the same pages
//! over the real routes).

use std::collections::BTreeMap;
use std::future::{Future, ready};
use std::sync::Mutex;

use futures::executor::block_on;
use serde_json::{Value, json};

use crate::pages::{ACTION, BUILD, BUILDS, PAGES};
use crate::read::Page;
use crate::{Answer, CONTENT_SECURITY_POLICY, Fetched, Source, SourceError, respond};

/// A read API that answers from a table and remembers every URL it was asked.
#[derive(Default)]
pub(crate) struct Fake {
    answers: BTreeMap<String, Result<Fetched, String>>,
    seen: Mutex<Vec<String>>,
}

impl Fake {
    /// `url` answers `status` with `body`.
    pub(crate) fn with(self, url: &str, status: u16, body: impl Into<Vec<u8>>) -> Self {
        self.with_headers(url, status, &[], body)
    }

    /// `url` answers `status` with `headers` and `body`.
    pub(crate) fn with_headers(
        mut self,
        url: &str,
        status: u16,
        headers: &[(&str, &str)],
        body: impl Into<Vec<u8>>,
    ) -> Self {
        let fetched = Fetched {
            status,
            body: body.into(),
            headers: headers
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
        };
        self.answers.insert(url.to_owned(), Ok(fetched));
        self
    }

    /// `url` answers `200` with `value`.
    pub(crate) fn json(self, url: &str, value: &Value) -> Self {
        self.with(url, 200, value.to_string())
    }

    /// Asking for `url` fails.
    pub(crate) fn down(mut self, url: &str) -> Self {
        self.answers
            .insert(url.to_owned(), Err("connection refused".to_owned()));
        self
    }

    /// Every URL asked, in order.
    pub(crate) fn seen(&self) -> Vec<String> {
        self.seen.lock().expect("not poisoned").clone()
    }
}

impl Source for Fake {
    fn get(&self, url: &str) -> impl Future<Output = Result<Fetched, SourceError>> + Send {
        self.seen.lock().expect("not poisoned").push(url.to_owned());
        let answer = self.answers.get(url).cloned().unwrap_or_else(|| {
            Ok(Fetched {
                status: 404,
                body: br#"{"error": "no such route"}"#.to_vec(),
                headers: Vec::new(),
            })
        });
        ready(answer.map_err(SourceError))
    }
}

pub(crate) const ID: &str = "build-1";

/// A digest as the API writes it, of `size` bytes.
pub(crate) fn digest(fill: char, size: u64) -> String {
    format!("{}-{size}", fill.to_string().repeat(64))
}

/// The digest of the action the fixtures hold.
pub(crate) fn action_digest() -> String {
    digest('a', 140)
}

pub(crate) fn invocation(id: &str) -> Value {
    json!({
        "id": id, "url": null, "tool": "buck2", "tool_version": "2026.10.01",
        "correlated_invocations_id": null, "instances": ["main", "edge"],
        "first_seen": "2026-10-11T09:00:00.000000Z", "last_seen": "2026-10-11T09:01:00.000000Z",
        "state": "idle", "complete": true, "rows_capped": false, "actions": 2,
        // Every counter its own value, so a value under another's label shows.
        "counts": {"execute": 4, "cache_hit_execute": 5, "joined": 3, "ran": 2, "failed": 1,
            "ac_hit": 6, "ac_miss": 7, "blobs_asked": 8, "blobs_missing": 9,
            "bytes_uploaded": 2048, "bytes_downloaded": 10},
        "collisions": [],
        "links": {"actions": format!("/v1/invocations/{id}/actions")},
    })
}

/// A row as `/actions` lists it.
pub(crate) fn row(action: &str, target: &str, outcome: &Value) -> Value {
    json!({
        "action": action, "instance": "main",
        "target": {"value": target, "truncated": false},
        "mnemonic": {"value": "CxxCompile", "truncated": false},
        "configuration": null, "action_id": null, "how": "ran", "operation": "op-1",
        "attempts": [{"lease": "3.1", "worker": "n4",
            "queued": "2026-10-11T09:00:01.000000Z", "granted": "2026-10-11T09:00:02.000000Z",
            "started": "2026-10-11T09:00:03.000000Z", "ended": "2026-10-11T09:00:04.000000Z",
            "end": {"failure": "exit", "reason": "exit 1"}}],
        "outcome": outcome, "result": null, "command": digest('c', 90),
        "href": format!("/v1/invocations/{ID}/actions/{action}"),
    })
}

/// A row that ran twice: requeued out of memory on `n3`, then failed on `n4`.
pub(crate) fn retried() -> Value {
    let mut r = row(
        &digest('7', 20),
        "//app:retried",
        &json!({"failed": {"code": 2, "message": "exit status 1"}}),
    );
    r["attempts"] = json!([
        {"lease": "3.2", "worker": "n3",
         "queued": "2026-10-11T09:00:05.000000Z", "granted": "2026-10-11T09:00:06.000000Z",
         "started": "2026-10-11T09:00:07.000000Z", "ended": "2026-10-11T09:00:08.000000Z",
         "end": {"requeue": "out_of_memory", "reason": "raised"}},
        {"lease": "3.3", "worker": "n4",
         "queued": "2026-10-11T09:00:08.000000Z", "granted": "2026-10-11T09:00:09.000000Z",
         "started": "2026-10-11T09:00:10.000000Z", "ended": "2026-10-11T09:00:11.000000Z",
         "end": {"failure": "exit"}},
    ]);
    r
}

/// The answer of `/actions/{digest}` with `stderr` as its stderr object.
pub(crate) fn action(stderr: &Value) -> Value {
    let mut a = row(
        &action_digest(),
        "//app:<b>lib</b>",
        &json!({"failed": {"code": 2, "message": "exit status 1"}}),
    );
    let extra = json!({
        "outputs": [
            {"digest": digest('d', 300), "available": true,
             "href": format!("/v1/blobs/{}?instance=main&name=lib.o", digest('d', 300)),
             "path": "out/lib.o", "executable": false, "directory": false},
            {"digest": digest('e', 70), "available": false,
             "href": format!("/v1/blobs/{}?instance=main", digest('e', 70)),
             "path": "out/gen", "executable": false, "directory": true},
        ],
        "outputs_truncated": true,
        "stdout": {"digest": digest('f', 12), "available": false,
            "href": format!("/v1/blobs/{}?instance=main&name=stdout&inline=text", digest('f', 12))},
        "stderr": stderr,
        "usage": {"cpu_user_micros": 1_500_000, "cpu_system_micros": 2_500,
            "peak_memory_bytes": 3_221_225_472_u64, "wall_micros": 4_000_000},
        "blobs": {"action": null, "command": null},
    });
    for (k, v) in extra.as_object().expect("an object") {
        a[k] = v.clone();
    }
    a
}

/// The stderr the fixtures hold: script markup, as a hostile build would print it.
pub(crate) const STDERR: &str = "<script>alert(1)</script>";

pub(crate) fn stderr_blob() -> Value {
    let d = digest('5', STDERR.len() as u64);
    json!({"digest": d, "available": true,
        "href": format!("/v1/blobs/{d}?instance=main&name=stderr&inline=text")})
}

/// A read API holding build `ID` with one page of three rows and a second page, and its
/// failed action with a stderr blob.
pub(crate) fn farm() -> Fake {
    let base = format!("/v1/invocations/{ID}");
    let ok = row(
        &digest('b', 10),
        "//app:ok",
        &json!({"ok": {"exit_code": 0}}),
    );
    let failed = row(
        &action_digest(),
        "//app:<b>lib</b>",
        &json!({"failed": {"code": 2, "message": "<img src=x onerror=alert(1)>"}}),
    );
    let stderr_href = stderr_blob()["href"].as_str().expect("a string").to_owned();
    Fake::default()
        .json(&base, &invocation(ID))
        .json(
            &format!("{base}/actions"),
            &json!({"actions": [ok, failed, retried()], "next_page": format!("{base}/actions?page_size=2&page_token=t%2B1"),
                "next_page_token": "t+1"}),
        )
        .json(&format!("{base}/actions?page_token=t%2B1"), &json!({"actions": []}))
        .json(
            &format!("{base}/actions/{}", action_digest()),
            &action(&stderr_blob()),
        )
        .with(&stderr_href, 200, STDERR)
}

pub(crate) fn render(fake: &Fake, path_and_query: &str) -> Answer {
    let (path, query) = path_and_query
        .split_once('?')
        .unwrap_or((path_and_query, ""));
    block_on(respond(fake, path, query))
}

pub(crate) fn body(a: &Answer) -> String {
    String::from_utf8(a.body().to_vec()).expect("pages are UTF-8")
}

fn action_page() -> String {
    format!("/ui/builds/{ID}/actions/{}", action_digest())
}

/// Catches: an answer, of any kind, without the CSP of `docs/design/ui-data.md` 8 or
/// without `nosniff` (mutants: a weakened policy string, the nosniff entry dropped,
/// the stylesheet or an error answer built without them).
#[test]
fn every_answer_carries_the_csp_and_nosniff() {
    assert_eq!(
        CONTENT_SECURITY_POLICY,
        "default-src 'none'; style-src 'self'; img-src 'self'; script-src 'self'; \
         frame-ancestors 'none'"
    );
    let fake = farm().down("/v1/invocations/down");
    let paths = [
        "/ui".to_owned(),
        "/ui/".to_owned(),
        format!("/ui?id={ID}"),
        "/ui?id=missing".to_owned(),
        "/ui?id=down".to_owned(),
        format!("/ui/builds/{ID}"),
        "/ui/builds/missing".to_owned(),
        action_page(),
        "/ui/static/kbf.css".to_owned(),
        "/ui/nothing-here".to_owned(),
        "/elsewhere".to_owned(),
    ];
    for path in &paths {
        let answer = render(&fake, path);
        let headers: BTreeMap<_, _> = answer.headers().into_iter().collect();
        assert_eq!(
            headers.get("content-security-policy"),
            Some(&CONTENT_SECURITY_POLICY),
            "{path}"
        );
        assert_eq!(
            headers.get("x-content-type-options"),
            Some(&"nosniff"),
            "{path}"
        );
        assert!(headers.contains_key("content-type"), "{path}");
    }
}

/// Catches: a page that answers the API's `401` without its `WWW-Authenticate`
/// (RFC 9110 15.5.2 requires one), or its `429` or `503` without its `Retry-After`
/// (mutants: the headers not passed on by the Builds page, or by a failed Build or
/// Action page); any other API header passed on (mutant: the allowlist check dropped); one
/// passed on with a page whose status is not the API's (a log read that failed).
#[test]
fn a_page_with_the_apis_status_passes_on_the_headers_it_needs() {
    let base = format!("/v1/invocations/{ID}");
    let challenge = ("WWW-Authenticate", "Bearer realm=\"kbf\"");
    let cases = [
        (401, challenge, "www-authenticate", "Bearer realm=\"kbf\""),
        (429, ("retry-after", "7"), "retry-after", "7"),
        (503, ("Retry-After", "30"), "retry-after", "30"),
    ];
    for (status, header, name, value) in cases {
        let headers = [
            header,
            ("set-cookie", "s=1"),
            ("content-type", "application/json"),
        ];
        let fake = Fake::default()
            .with_headers(&base, status, &headers, r#"{"error": "no"}"#)
            .with_headers(
                &format!("{base}/actions/{}", action_digest()),
                status,
                &headers,
                r#"{"error": "no"}"#,
            );
        for path in [
            format!("/ui?id={ID}"),
            format!("/ui/builds/{ID}"),
            action_page(),
        ] {
            let answer = render(&fake, &path);
            assert_eq!(answer.status(), status, "{path}");
            let headers = answer.headers();
            assert!(headers.contains(&(name, value)), "{path}: {headers:?}");
            assert_eq!(headers.len(), 4, "{path}: {headers:?}");
            assert!(
                headers.contains(&("content-type", "text/html; charset=utf-8")),
                "{path}: {headers:?}"
            );
        }
    }

    // A log read that answered 429: the page is 200, so the header is not passed on.
    let blob = "/v1/blobs/x";
    let stderr = json!({"digest": digest('5', 10), "available": true, "href": blob});
    let fake = Fake::default()
        .json(
            &format!("{base}/actions/{}", action_digest()),
            &action(&stderr),
        )
        .with_headers(blob, 429, &[("retry-after", "7")], "{}");
    let answer = render(&fake, &action_page());
    assert_eq!(answer.status(), 200);
    assert_eq!(answer.headers().len(), 3, "{:?}", answer.headers());
}

/// Catches (`docs/design/ui-data.md` 8, red test of U6): a stderr of
/// `<script>alert(1)</script>` written as markup (mutant: `Html::text` writing raw),
/// and the same for a target label and a failure message on the Build page.
#[test]
fn a_stderr_of_script_renders_as_text() {
    let fake = farm();
    let page = body(&render(&fake, &action_page()));
    assert!(
        page.contains("<pre>&lt;script&gt;alert(1)&lt;/script&gt;</pre>"),
        "{page}"
    );
    assert!(page.contains("//app:&lt;b&gt;lib&lt;/b&gt;"), "{page}");
    assert!(!page.contains("<script"), "{page}");
    assert!(!page.contains("<b>"), "{page}");

    let build = body(&render(&fake, &format!("/ui/builds/{ID}")));
    assert!(
        build.contains("&lt;img src=x onerror=alert(1)&gt;"),
        "{build}"
    );
    assert!(!build.contains("<img"), "{build}");
    assert!(!build.contains("<b>"), "{build}");
}

/// The pages the contract test renders, each with the UI paths that make it read
/// every route it declares.
fn contract_table() -> Vec<(&'static Page, Vec<String>)> {
    vec![
        (&BUILDS, vec!["/ui".to_owned(), format!("/ui?id={ID}")]),
        (
            &BUILD,
            vec![
                format!("/ui/builds/{ID}"),
                format!("/ui/builds/{ID}?page_token=t%2B1"),
            ],
        ),
        (&ACTION, vec![action_page()]),
    ]
}

/// The URLs in `seen` that `page` does not declare.
fn violations(page: &Page, seen: &[String]) -> Vec<String> {
    seen.iter().filter(|u| !page.declares(u)).cloned().collect()
}

/// The page contract (`docs/design/ui-data.md` 6.1). Catches: a page that reads a
/// route its `reads` does not name (mutant: Build reading `/actions` with only the
/// summary declared, which the reader refuses, so the page is not `200`; and with the
/// reader's check removed as well, the fake sees the undeclared URL); a declared route
/// no page reads any more (a stale list); a page missing from the table; a URL read
/// that the page does not list for an agent.
#[test]
fn every_page_reads_only_the_routes_it_declares() {
    let table = contract_table();
    let names: Vec<&str> = table.iter().map(|(p, _)| p.name).collect();
    let all: Vec<&str> = PAGES.iter().map(|p| p.name).collect();
    assert_eq!(names, all, "every page has a contract row");
    for (page, paths) in &table {
        let mut used = vec![false; page.reads.len()];
        for path in paths {
            let fake = farm();
            let answer = render(&fake, path);
            assert_eq!(answer.status(), 200, "{path}: {}", body(&answer));
            let seen = fake.seen();
            assert_eq!(violations(page, &seen), Vec::<String>::new(), "{path}");
            let html = body(&answer);
            for url in &seen {
                let escaped = crate::html::escape(url);
                assert!(
                    html.contains(&format!("<a href=\"{escaped}\">{escaped}</a>")),
                    "{path}: {url} not listed"
                );
                let path_only = url.split_once('?').map_or(url.as_str(), |(p, _)| p);
                for (i, pattern) in page.reads.iter().enumerate() {
                    let one = Page {
                        name: "",
                        reads: std::slice::from_ref(pattern),
                    };
                    used[i] |= one.declares(path_only);
                }
            }
        }
        for (pattern, used) in page.reads.iter().zip(used) {
            assert!(used, "{}: {pattern} is declared but never read", page.name);
        }
    }
}

/// Catches: a reader that sends an undeclared URL (mutant: the `declares` check in
/// `Reader` removed), and a contract check that cannot see it.
#[test]
fn a_read_of_an_undeclared_route_is_refused_and_is_a_contract_violation() {
    let fake = farm();
    let mut r = crate::read::Reader::new(&fake, &BUILDS);
    let url = format!("/v1/invocations/{ID}/metrics");
    let got = block_on(r.json::<Value>(&url));
    let e = got.expect_err("refused");
    assert!(e.is_page_bug(), "{e}");
    assert_eq!(e.status(), 500);
    let asked = crate::read::Asked {
        url: url.clone(),
        sent: false,
    };
    assert_eq!(r.asked(), [asked]);
    assert!(fake.seen().is_empty(), "nothing was sent");
    assert_eq!(violations(&BUILDS, std::slice::from_ref(&url)), [url]);
}

/// Catches: a dot segment accepted as a route's `{name}` segment (mutant: the dot
/// check in `matches` dropped), so a page could send `/v1/invocations/..`, which a URL
/// client resolves to `/v1`, a route no page declares.
#[test]
fn no_page_declares_a_dot_segment() {
    for url in [
        "/v1/invocations/..",
        "/v1/invocations/.",
        "/v1/invocations/%2E%2E",
        "/v1/invocations/../actions",
    ] {
        for page in PAGES {
            assert!(!page.declares(url), "{}: {url}", page.name);
        }
    }
    assert!(!ACTION.declares("/v1/blobs/%2e%2E"));
    assert!(!ACTION.declares("/v1/invocations/x/actions/.%2e"));
}

/// Catches: a log `href` the API sends that leaves the page's routes (a foreign host,
/// another route) being fetched or linked: it is refused before any request, shown
/// as a page bug, and listed as text, not as a link.
#[test]
fn a_log_href_outside_the_declared_routes_is_not_read() {
    // A WHATWG URL client reads `\\` in a path as `/`, so the last resolves to `/v1/x`.
    for href in [
        "https://evil.example/x",
        "/v1/invocations/x/metrics",
        "/v1/blobs/..\\x",
    ] {
        let stderr = json!({"digest": digest('5', 10), "available": true, "href": href});
        let base = format!("/v1/invocations/{ID}/actions/{}", action_digest());
        let fake = Fake::default().json(&base, &action(&stderr));
        let answer = render(&fake, &action_page());
        assert_eq!(answer.status(), 200);
        assert_eq!(fake.seen(), [base], "{href}");
        let page = body(&answer);
        assert!(page.contains("is not a route this page declares"), "{page}");
        assert!(page.contains("problem bug"), "{page}");
        assert!(!page.contains(&format!("href=\"{href}\"")), "{page}");
        assert!(
            page.contains(&format!(
                "<code>{href}</code> (not read: not a route this page declares)"
            )),
            "{page}"
        );
    }
}

/// Catches: routing that answers a page for a path that names none (`/uix`, an
/// empty id or digest, a segment that is not UTF-8, a segment that decodes to `.` or
/// `..`, which a URL client would resolve to another route), or that does not decode
/// an id and encode it again for the API.
#[test]
fn routing() {
    let fake = farm();
    for path in [
        "/uix",
        "/ui/builds",
        "/ui/builds/",
        "/ui/builds//actions/x",
        "/ui/builds/x/actions/",
        "/ui/builds/%FF",
        "/ui/builds/.",
        "/ui/builds/%2E%2E",
        "/ui/builds/.%2e",
        "/ui/builds/x/actions/%2e%2e",
        "/ui/builds/%2e/actions/x",
        "/ui/builds/x/other/y",
        "/ui/static/other.css",
        "/elsewhere",
    ] {
        let answer = render(&fake, path);
        assert_eq!(answer.status(), 404, "{path}");
        assert!(body(&answer).contains("No page is at"), "{path}");
    }
    assert!(fake.seen().is_empty());

    let css = render(&fake, "/ui/static/kbf.css");
    assert_eq!(css.status(), 200);
    assert_eq!(
        css.headers()[0],
        ("content-type", "text/css; charset=utf-8")
    );
    assert!(body(&css).contains("prefers-color-scheme: dark"));

    let slash = render(&fake, "/ui/");
    assert_eq!(slash.status(), 200);
    assert_eq!(
        slash.headers()[0],
        ("content-type", "text/html; charset=utf-8")
    );

    let fake = Fake::default();
    let answer = render(&fake, "/ui/builds/a%20b%3C~");
    assert_eq!(answer.status(), 404);
    assert_eq!(fake.seen(), ["/v1/invocations/a%20b%3C~"]);
    let answer = render(&fake, "/ui/builds/a%20b/actions/d%2Fe");
    assert_eq!(answer.status(), 404);
    assert_eq!(fake.seen()[1], "/v1/invocations/a%20b/actions/d%2Fe");
}
