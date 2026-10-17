//! Paging the build pages (`docs/design/invocations.md` section 4.5): pages read while
//! rows are appended, tokens used with another filter, and tokens for a record that
//! is gone.

mod pages;

use kbf_invocation::{ActionRow, Event, Outcome};
use kbf_sim::SimRng;
use pages::{OPEN, Server, at, cached, digest, finished, hit, key, listing, queued, text};
use serde_json::Value;

/// The rows a session appends: each ends as it is added (a hit, a cached Execute, or
/// an Execute that fails), so its outcome and listing do not change while it is
/// paged. Some fail (exit 1, or a failed status), some have outputs, stdout, stderr.
struct Rows {
    rng: SimRng,
    next: u64,
}

impl Rows {
    /// The events of one new row of invocation `id`.
    fn add(&mut self, id: &str) -> Vec<Event> {
        self.next += 1;
        let n = self.next;
        let action = digest(n);
        let outputs: Vec<_> = (0..self.rng.below(3)).map(|o| digest(n * 10 + o)).collect();
        let stdout = (self.rng.below(2) == 0).then(|| digest(n * 10 + 7));
        let stderr = (self.rng.below(3) == 0).then(|| digest(n * 10 + 8));
        let exit = i32::from(self.rng.below(3) == 0);
        let l = listing(digest(n * 10 + 9), exit, &outputs, (stdout, stderr));
        match self.rng.below(3) {
            0 => vec![hit(id, at(n), action, l)],
            1 => vec![cached(id, at(n), action, l)],
            _ => {
                let failed = Outcome::Failed {
                    code: 13,
                    message: "the action failed".to_owned(),
                };
                vec![
                    queued(id, "main", at(n), action, n),
                    finished(n, 1, failed, Some(l), at(n)),
                ]
            }
        }
    }
}

/// What a route of one session pages over, and the filter it asks.
#[derive(Clone, Copy, Debug)]
enum Paged {
    Actions(&'static str),
    Artifacts,
    Logs,
}

impl Paged {
    fn path(self, id: &str) -> String {
        match self {
            Self::Actions(filter) => format!("/v1/invocations/{id}/actions?{filter}"),
            Self::Artifacts => format!("/v1/invocations/{id}/artifacts?"),
            Self::Logs => format!("/v1/invocations/{id}/logs?"),
        }
    }

    fn list(self) -> &'static str {
        match self {
            Self::Actions(_) => "actions",
            Self::Artifacts => "artifacts",
            Self::Logs => "logs",
        }
    }

    /// What each entry is: its action, and the output path for an artifact.
    fn entry(self, v: &Value) -> String {
        match self {
            Self::Artifacts => format!("{} {}", v["action"], v["path"]),
            _ => v["action"].to_string(),
        }
    }

    /// The entries the route lists for `rows`, in its order, as [`Self::entry`] names
    /// them; for `/logs`, in row order (its order is checked apart).
    fn expected(self, rows: &[ActionRow]) -> Vec<String> {
        let action = |r: &ActionRow| format!("\"{}\"", text(&r.action));
        let mut out = Vec::new();
        for r in rows {
            let l = r.listing.as_ref().expect("every row ends as it is added");
            match self {
                Self::Actions(filter) => {
                    let failed = matches!(r.outcome, Some(Outcome::Failed { .. }));
                    let keep = match filter {
                        "outcome=ok" => !failed,
                        "outcome=failed" => failed,
                        "how=cache_hit" => r.how == kbf_invocation::How::CacheHit,
                        "how=ran&outcome=failed" => r.how == kbf_invocation::How::Ran && failed,
                        _ => true,
                    };
                    if keep {
                        out.push(action(r));
                    }
                }
                Self::Artifacts => {
                    out.extend(
                        l.outputs
                            .iter()
                            .map(|o| format!("{} \"{}\"", action(r), o.path)),
                    );
                }
                Self::Logs => {
                    if l.stdout.is_some() || l.stderr.is_some() {
                        out.push(action(r));
                    }
                }
            }
        }
        out
    }
}

/// Catches (mutants "the token points at the last row listed" and "the token skips a
/// row", for each route; "logs list the failed rows again" and "logs miss failed rows
/// added after the failed part"; and "`next_page` without the filter", a `400`): a
/// page that repeats or skips a row (or an output) while rows are appended between
/// pages, on `/actions` with each filter, on `/artifacts` and on `/logs`, following
/// each page's `next_page` from the first. A seeded property test: 30 seeds, each a
/// session per route with 0 to 24 rows to start, pages of 1 to 5, and 0 to 3 rows
/// appended before each page; when the last page comes (no `next_page`, and no
/// `next_page_token`), every entry of the rows there are then has been listed exactly
/// once, in row order (`/logs`: once each, failed rows of the first part before the
/// others).
#[tokio::test]
async fn paging_while_rows_are_appended_never_skips_or_repeats() {
    let server = Server::with(OPEN);
    let routes = [
        Paged::Actions(""),
        Paged::Actions("outcome=ok"),
        Paged::Actions("outcome=failed"),
        Paged::Actions("how=cache_hit"),
        Paged::Actions("how=ran&outcome=failed"),
        Paged::Artifacts,
        Paged::Logs,
    ];
    let (mut sessions, mut pages_read, mut appended) = (0, 0, 0);
    for seed in 0..30u64 {
        for (r, route) in routes.into_iter().enumerate() {
            let id = format!("s{seed}-{r}");
            let mut rows = Rows {
                rng: SimRng::from_seed(seed * 100 + r as u64),
                next: seed * 1_000_000 + r as u64 * 10_000,
            };
            let start = rows.rng.below(25);
            let mut events = Vec::new();
            for _ in 0..=start {
                events.extend(rows.add(&id));
            }
            server.feed(events).await;
            let size = rows.rng.between(1, 6);
            let mut listed = Vec::new();
            let mut first_part = None;
            // The first page; then each page's `next_page`, as an agent follows it.
            let mut path = format!("{}&page_size={size}", route.path(&id));
            for page in 0.. {
                assert!(page < 1_000, "{id}: paging does not end");
                let answer = server.ok(&path).await;
                pages_read += 1;
                let entries = answer[route.list()].as_array().expect("entries");
                assert!(entries.len() <= size as usize, "{id}: {answer}");
                for e in entries {
                    if matches!(route, Paged::Logs) && e["failed"] == false {
                        first_part.get_or_insert(listed.len());
                    }
                    listed.push(route.entry(e));
                }
                let token = answer["next_page_token"].as_str();
                let Some(next) = answer["next_page"].as_str() else {
                    assert!(
                        token.is_none(),
                        "{id}: a token without a next_page: {answer}"
                    );
                    break;
                };
                let token = token.expect("a next_page comes with its token");
                assert!(
                    next.contains(&format!("page_token={token}")),
                    "{id}: {next}"
                );
                path = next.to_owned();
                // Rows appended between pages, fewer once paging has gone on a while,
                // so that it ends.
                let mut events = Vec::new();
                if page < 20 {
                    for _ in 0..rows.rng.below(4) {
                        events.extend(rows.add(&id));
                        appended += 1;
                    }
                }
                server.feed(events).await;
            }
            let record = server.recorder.get(&key(&id)).expect("the record is held");
            let mut want = route.expected(record.rows());
            if matches!(route, Paged::Logs) {
                // Once each, in any order; the failed part is checked below.
                let mut got = listed.clone();
                got.sort();
                want.sort();
                assert_eq!(got, want, "{id}: listed {listed:?}");
                let failed_part = &listed[..first_part.unwrap_or(listed.len())];
                let failed: Vec<String> = record
                    .rows()
                    .iter()
                    .filter(|r| r.outcome.as_ref().is_some_and(Outcome::is_failure))
                    .map(|r| format!("\"{}\"", text(&r.action)))
                    .collect();
                assert!(
                    failed_part.iter().all(|a| failed.contains(a)),
                    "{id}: a row that did not fail is in the failed part: {listed:?}"
                );
            } else {
                assert_eq!(listed, want, "{id}");
            }
            sessions += 1;
        }
    }
    assert_eq!(sessions, 30 * 7);
    // The scenario holds: sessions of many pages, with rows appended between them.
    assert!(
        pages_read > 1_000 && appended > 1_000,
        "{pages_read} pages, {appended} rows appended"
    );
    server.stop().await;
}

/// Catches (mutant "skip the filter check" in the token's decoding): a token made for
/// one filter used with another, with none, or on another route, answered as if it
/// were made for it; and a token refused when the same filter is given in another
/// order.
#[tokio::test]
async fn a_token_used_with_another_filter_is_400() {
    let server = Server::with(OPEN);
    let mut events = Vec::new();
    for n in 1..=4 {
        let l = listing(
            digest(100 + n),
            0,
            &[digest(200 + n)],
            (Some(digest(300 + n)), None),
        );
        events.push(hit("b", at(n), digest(n), l));
    }
    server.feed(events).await;
    let base = "/v1/invocations/b";
    let first = server
        .ok(&format!(
            "{base}/actions?outcome=ok&how=cache_hit&page_size=1"
        ))
        .await;
    let token = first["next_page_token"]
        .as_str()
        .expect("a next page")
        .to_owned();
    for query in [
        "outcome=failed&how=cache_hit",
        "outcome=ok",
        "",
        "how=cache_hit",
    ] {
        let path = format!("{base}/actions?{query}&page_token={token}");
        let answer = server.get(&path).await;
        assert_eq!(answer.code, 400, "{query}: {}", answer.json);
        let why = answer.json["error"].as_str().unwrap_or_default();
        assert!(why.contains("another filter"), "{query}: {why}");
        for name in ["outcome", "how", "requeue_reason", "memory_kill", "sort"] {
            assert!(why.contains(name), "{query}: the error names {name}: {why}");
        }
    }
    for route in ["logs", "artifacts"] {
        let answer = server
            .get(&format!("{base}/{route}?page_token={token}"))
            .await;
        assert_eq!(answer.code, 400, "{route}: {}", answer.json);
        let why = answer.json["error"].as_str().unwrap_or_default();
        assert!(why.contains("made for /actions"), "{route}: {why}");
    }
    let same = server
        .ok(&format!(
            "{base}/actions?how=cache_hit&outcome=ok&page_size=1&page_token={token}"
        ))
        .await;
    assert_eq!(same["actions"][0]["action"], text(&digest(2)));
    server.stop().await;
}

/// Catches (mutant "resolve the token's position anyway": the serial check dropped
/// from the token's decoding): a page token for a record removed at the memory bound
/// after the token was issued answered with a page of the record made since under
/// the same key, rather than `404`; and the record's routes answering anything but
/// `404` while it is gone.
#[tokio::test]
async fn a_token_for_a_record_evicted_after_it_was_issued_is_404() {
    let policy = kbf_auth::Policy::from_json(OPEN).expect("a policy");
    let server = Server::start(policy, 64 << 10);
    let rows = |id: &str, from: u64, n: u64, when: u64| -> Vec<Event> {
        (from..from + n)
            .map(|i| {
                let l = listing(digest(10_000 + i), 0, &[], (Some(digest(20_000 + i)), None));
                hit(id, at(when + i), digest(i), l)
            })
            .collect()
    };
    // Ten rows: removing it frees room enough for the record made again below.
    server.feed(rows("old", 0, 10, 0)).await;
    let first = server.ok("/v1/invocations/old/actions?page_size=1").await;
    let token = first["next_page_token"]
        .as_str()
        .expect("a next page")
        .to_owned();
    let page = format!("/v1/invocations/old/actions?page_size=1&page_token={token}");
    assert_eq!(
        server.ok(&page).await["actions"][0]["action"],
        text(&digest(1))
    );
    // Eleven minutes on, `old` is idle; new builds fill the bound until it is removed.
    let later = 11 * 60 * 1_000_000;
    let mut builds = 0;
    while server.recorder.get(&key("old")).is_some() {
        builds += 1;
        assert!(builds < 10_000, "the record is never removed");
        server
            .feed(rows(&format!("new-{builds}"), 100, 1, later))
            .await;
    }
    for path in [
        page.as_str(),
        "/v1/invocations/old",
        "/v1/invocations/old/actions",
    ] {
        let answer = server.get(path).await;
        assert_eq!(answer.code, 404, "{path}: removed");
        assert_eq!(answer.json["error"], "no_record", "{path}: {}", answer.json);
        assert_eq!(answer.json["id"], "old", "{path}");
    }
    // Made again under the same key, with rows enough for the token's position.
    server.feed(rows("old", 500, 2, later + 1_000)).await;
    let again = server.recorder.get(&key("old")).expect("made again");
    assert_eq!(again.rows().len(), 2, "both rows fit");
    let answer = server.get(&page).await;
    assert_eq!(
        answer.code, 404,
        "the token of the removed record: {}",
        answer.json
    );
    assert_eq!(answer.json["error"], "no_record", "{}", answer.json);
    let fresh = server.ok("/v1/invocations/old/actions?page_size=1").await;
    assert_eq!(fresh["actions"][0]["action"], text(&digest(500)));
    server.stop().await;
}

/// Catches (mutant "`records_since` = start only"): the `404` of a record not held
/// without the body of `docs/design/invocations-api.md` 4.10, or with a
/// `records_since` other than the later of the server's start and the `last_seen` of
/// the newest client record removed at the bound. The records here are seen in 2030,
/// after the server started, so the two cases differ.
#[tokio::test]
async fn a_missing_record_says_since_when_records_are_held() {
    let before = kbf_front::wall_now();
    let policy = kbf_auth::Policy::from_json(OPEN).expect("a policy");
    let server = Server::start(policy, 64 << 10);
    let after = kbf_front::wall_now();
    let missing = server.get("/v1/invocations/never-seen").await;
    assert_eq!(missing.code, 404);
    assert_eq!(missing.json["error"], "no_record", "{}", missing.json);
    assert_eq!(missing.json["id"], "never-seen");
    let since = missing.json["records_since"].as_str().expect("a time");
    let since_micros = micros(since);
    assert!(
        before.unix_micros() <= since_micros && since_micros <= after.unix_micros(),
        "{since}: the server's start"
    );
    // `old`, last seen at 2030-01-01T00:00:00.000009Z, then idle and removed.
    server.feed(rows_at("old", 0, 10, 0)).await;
    let eleven_minutes_on = 11 * 60 * 1_000_000;
    let mut builds = 0;
    while server.recorder.get(&key("old")).is_some() {
        builds += 1;
        assert!(builds < 10_000, "the record is never removed");
        let id = format!("new-{builds}");
        server.feed(rows_at(&id, 100, 1, eleven_minutes_on)).await;
    }
    let gone = server.get("/v1/invocations/old/logs").await;
    assert_eq!(gone.code, 404);
    assert_eq!(gone.json["error"], "no_record", "{}", gone.json);
    assert_eq!(
        gone.json["records_since"], "2030-01-01T00:00:00.000009Z",
        "the removed record's last_seen: {}",
        gone.json
    );
    server.stop().await;
}

/// `n` hits of invocation `id` from action `from`, the i-th at `when + i` µs after
/// 2030-01-01.
fn rows_at(id: &str, from: u64, n: u64, when: u64) -> Vec<Event> {
    (from..from + n)
        .map(|i| {
            let l = listing(digest(10_000 + i), 0, &[], (Some(digest(20_000 + i)), None));
            hit(id, pages::later(when + i), digest(i), l)
        })
        .collect()
}

/// Microseconds since the Unix epoch of `YYYY-MM-DDTHH:MM:SS.ffffffZ` (H. Hinnant's
/// `days_from_civil`; the server writes times with its inverse).
fn micros(text: &str) -> u64 {
    let n = |range: std::ops::Range<usize>| -> u64 { text[range].parse().expect("a number") };
    let (y, m, d) = (n(0..4), n(5..7), n(8..10));
    let (y, m) = if m <= 2 { (y - 1, m + 9) } else { (y, m - 3) };
    let era = y / 400;
    let yoe = y - era * 400;
    let doy = (153 * m + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = days * 86_400 + n(11..13) * 3_600 + n(14..16) * 60 + n(17..19);
    secs * 1_000_000 + n(20..26)
}
