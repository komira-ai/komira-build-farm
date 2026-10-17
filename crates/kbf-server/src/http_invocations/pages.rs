//! The part of the build pages that reads one record and no I/O: what a request asks
//! for, its page token, and which rows a page holds (`docs/api.md`,
//! `docs/design/invocations.md` sections 4.1 and 4.5); [`super::views`] writes them.

use std::collections::BTreeSet;

use kbf_front::InvocationId as AcceptedId;
use kbf_invocation::{
    ActionRow, AttemptEnd, How, Invocation, InvocationId, MemoryKillKind, Outcome, Phase,
    REQUEUE_NAMES, RecordKey, WallTime, requeue_name,
};
use kbf_types::Digest;
use serde_json::{Map, Value};

use super::views::{Builder, Draft, action, artifact, digest_text, invocation, logs, metrics, row};
use crate::http_blobs::parse_digest;

/// Rows (or entries) a page holds unless `?page_size=` says otherwise.
pub const DEFAULT_PAGE_SIZE: usize = 100;

/// The largest `?page_size=`.
pub const MAX_PAGE_SIZE: usize = 1000;

/// The routes' path prefix; the record's key follows it.
pub(crate) const PREFIX: &str = "/v1/invocations/";

/// Why a request is not answered with a page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Bad {
    /// `404`: no such route, no such row.
    NotFound(String),
    /// `404` with the body of `docs/design/invocations-api.md` 4.10: the record is
    /// not held, or is another one than a page token (or the authorization) was made
    /// for: one removed since and made again under the same key.
    Gone,
    /// `400`: a malformed query or page token, or a token made for another route or
    /// filter.
    Invalid(String),
    /// `503`: the record changed between its authorization and its read (it gained an
    /// instance name); the request may be sent again.
    Changed(String),
}

/// Which route a request is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Route {
    /// `/v1/invocations/{id}`.
    Invocation,
    /// `/actions`, a page of rows.
    Actions,
    /// `/actions/{digest}`, one row.
    Action(Digest),
    /// `/artifacts`, a page of outputs.
    Artifacts,
    /// `/logs`, a page of stdout and stderr, failed actions first.
    Logs,
    /// `/metrics`.
    Metrics,
}

/// The outcomes `?outcome=` names (section 3.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OutcomeFilter {
    Ok,
    Failed,
    TimedOut,
    Refused,
}

const OUTCOMES: [(OutcomeFilter, &str); 4] = [
    (OutcomeFilter::Ok, "ok"),
    (OutcomeFilter::Failed, "failed"),
    (OutcomeFilter::TimedOut, "timed_out"),
    (OutcomeFilter::Refused, "refused"),
];

const HOWS: [(How, &str); 4] = [
    (How::CacheHit, "cache_hit"),
    (How::CacheHitExecute, "cache_hit_execute"),
    (How::Joined, "joined"),
    (How::Ran, "ran"),
];

/// The name of `outcome`, as `?outcome=` takes it.
fn outcome_name(outcome: OutcomeFilter) -> &'static str {
    OUTCOMES
        .iter()
        .find(|(o, _)| *o == outcome)
        .map_or("", |(_, n)| n)
}

/// The name of `how`, as the API writes it.
pub(super) fn how_name(how: How) -> &'static str {
    HOWS.iter().find(|(h, _)| *h == how).map_or("", |(_, n)| n)
}

/// What `/actions` keeps and in which order: rows of one outcome, of one `how`, with
/// a lease given up for one reason, with (or without) a memory kill of one kind, any
/// of these; in row order, or by one phase's duration, longest first.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Filter {
    outcome: Option<OutcomeFilter>,
    how: Option<How>,
    requeue_reason: Option<&'static str>,
    memory_kill: Option<MemoryKillKind>,
    sort: Option<Phase>,
}

impl Filter {
    /// Whether `row` passes. `?memory_kill=none` keeps the rows none of whose leases
    /// was killed for memory; any other kind, the rows one of whose leases was.
    fn keeps(self, row: &ActionRow) -> bool {
        let requeued = self.requeue_reason.is_none_or(|want| {
            row.attempts
                .iter()
                .any(|a| matches!(&a.end, Some(AttemptEnd::Requeued(r)) if requeue_name(r) == want))
        });
        let killed = self.memory_kill.is_none_or(|want| {
            let kills = || row.attempts.iter().filter_map(|a| a.memory_kill());
            match want {
                MemoryKillKind::None => kills().all(|k| k == MemoryKillKind::None),
                kind => kills().any(|k| k == kind),
            }
        });
        requeued && killed && self.keeps_built(row)
    }

    /// Whether `row` passes the filters `/actions` was built with.
    fn keeps_built(self, row: &ActionRow) -> bool {
        let outcome = self.outcome.is_none_or(|want| {
            matches!(
                (want, &row.outcome),
                (OutcomeFilter::Ok, Some(Outcome::Ok { .. }))
                    | (OutcomeFilter::Failed, Some(Outcome::Failed { .. }))
                    | (OutcomeFilter::TimedOut, Some(Outcome::TimedOut))
                    | (OutcomeFilter::Refused, Some(Outcome::Refused(_)))
            )
        });
        outcome && self.how.is_none_or(|h| row.how == h)
    }

    /// The filter as a page token carries it: `<outcome>+<how>`, `-` for either one
    /// absent, then `+<name>=<value>` for each later filter and the sort given (so a
    /// token of the built filters reads as it did).
    fn text(self) -> String {
        let outcome = self.outcome.map_or("-", outcome_name);
        let how = self.how.map_or("-", how_name);
        let mut text = format!("{outcome}+{how}");
        for (name, value) in self.later() {
            text.push_str(&format!("+{name}={value}"));
        }
        text
    }

    /// The filters and sort added after the built ones, by query name, those given.
    fn later(self) -> impl Iterator<Item = (&'static str, &'static str)> {
        [
            ("requeue_reason", self.requeue_reason),
            ("memory_kill", self.memory_kill.map(MemoryKillKind::name)),
            ("sort", self.sort.map(Phase::name)),
        ]
        .into_iter()
        .filter_map(|(name, value)| Some((name, value?)))
    }

    /// Adds the filter to a query, as `/actions` takes it.
    fn append_to(self, query: &mut form_urlencoded::Serializer<'_, String>) {
        if let Some(outcome) = self.outcome {
            query.append_pair("outcome", outcome_name(outcome));
        }
        if let Some(how) = self.how {
            query.append_pair("how", how_name(how));
        }
        for (name, value) in self.later() {
            query.append_pair(name, value);
        }
    }
}

/// What a request for a page asks: its size, where to start and what to keep.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PageQuery {
    size: usize,
    token: Option<String>,
    filter: Filter,
}

/// A request to one of the routes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Request {
    /// The record's key.
    pub(crate) key: RecordKey,
    route: Route,
    /// The page asked for, on the paged routes.
    page: Option<PageQuery>,
}

impl Request {
    /// The request for `path` (under [`PREFIX`]) with the query `query` (after `?`).
    ///
    /// # Errors
    /// [`Bad::NotFound`] for a path that names no route, or a key that is not an
    /// invocation id of section 2.2 or `<id>~<n>` (n from 2); [`Bad::Invalid`] for a
    /// query the route does not take.
    pub(crate) fn parse(path: &str, query: &str) -> Result<Self, Bad> {
        let rest = path.strip_prefix(PREFIX).unwrap_or_default();
        let mut parts = rest.split('/');
        let text = parts.next().unwrap_or_default();
        let key = parse_key(text).ok_or_else(|| {
            Bad::NotFound(format!(
                "{text:?} is not an invocation: an id of [A-Za-z0-9._-] that starts with a \
                 letter or digit, or <id>~<n>"
            ))
        })?;
        let route = match (parts.next(), parts.next(), parts.next()) {
            (None, _, _) => Route::Invocation,
            (Some("actions"), None, _) => Route::Actions,
            (Some("actions"), Some(digest), None) => {
                Route::Action(parse_digest(digest).ok_or_else(|| {
                    Bad::NotFound(format!("{digest:?} is not a digest: <sha256 hex>-<size>"))
                })?)
            }
            (Some("artifacts"), None, _) => Route::Artifacts,
            (Some("logs"), None, _) => Route::Logs,
            (Some("metrics"), None, _) => Route::Metrics,
            _ => return Err(Bad::NotFound(format!("no route {path:?}"))),
        };
        let page = parse_query(query, route)?;
        Ok(Self { key, route, page })
    }
}

/// A record key as `{id}` names it: an id the metadata layer accepts, or `<id>~<n>`
/// with `n` a decimal from 2 without leading zeros. Anything else is `None`.
fn parse_key(text: &str) -> Option<RecordKey> {
    let (id, n) = match text.split_once('~') {
        None => (text, None),
        Some((id, n)) => (id, Some(n)),
    };
    let id = InvocationId::new(AcceptedId::parse(id)?.as_str());
    match n {
        None => Some(RecordKey::owner(id)),
        Some(n) => {
            let digits = n.bytes().all(|b| b.is_ascii_digit()) && !n.starts_with('0');
            let n: usize = n.parse().ok().filter(|&n| digits && n >= 2)?;
            Some(RecordKey::collision(id, n))
        }
    }
}

/// The page a paged route's query asks for (`None` for a route that is not paged).
fn parse_query(text: &str, route: Route) -> Result<Option<PageQuery>, Bad> {
    let (paged, filtered, takes) = match route {
        Route::Actions => (
            true,
            true,
            "page_size, page_token, outcome, how, requeue_reason, memory_kill and sort",
        ),
        Route::Artifacts | Route::Logs => (true, false, "page_size and page_token"),
        _ => (false, false, "no parameters"),
    };
    let mut query = PageQuery {
        size: DEFAULT_PAGE_SIZE,
        token: None,
        filter: Filter::default(),
    };
    let mut seen: Vec<String> = Vec::new();
    for (key, value) in form_urlencoded::parse(text.as_bytes()) {
        if seen.iter().any(|k| *k == key) {
            return Err(Bad::Invalid(format!("{key:?} is given more than once")));
        }
        match &*key {
            "page_size" if paged => {
                query.size = value
                    .parse()
                    .ok()
                    .filter(|n| (1..=MAX_PAGE_SIZE).contains(n))
                    .ok_or_else(|| {
                        Bad::Invalid(format!(
                            "page_size={value:?}: a page holds 1 to {MAX_PAGE_SIZE}"
                        ))
                    })?;
            }
            "page_token" if paged => query.token = Some(value.clone().into_owned()),
            "outcome" if filtered => {
                let found = OUTCOMES.iter().find(|(_, n)| *n == value);
                query.filter.outcome = Some(found.map(|(o, _)| *o).ok_or_else(|| {
                    Bad::Invalid(format!(
                        "outcome={value:?}: one of ok, failed, timed_out, refused"
                    ))
                })?);
            }
            "how" if filtered => {
                let found = HOWS.iter().find(|(_, n)| *n == value);
                query.filter.how = Some(found.map(|(h, _)| *h).ok_or_else(|| {
                    Bad::Invalid(format!(
                        "how={value:?}: one of cache_hit, cache_hit_execute, joined, ran"
                    ))
                })?);
            }
            "requeue_reason" if filtered => {
                let found = REQUEUE_NAMES.iter().find(|n| **n == value);
                query.filter.requeue_reason = Some(*found.ok_or_else(|| {
                    Bad::Invalid(format!(
                        "requeue_reason={value:?}: one of {}",
                        REQUEUE_NAMES.join(", ")
                    ))
                })?);
            }
            "memory_kill" if filtered => {
                let found = MemoryKillKind::NAMED.iter().find(|(_, n)| *n == value);
                query.filter.memory_kill = Some(found.map(|(k, _)| *k).ok_or_else(|| {
                    Bad::Invalid(format!(
                        "memory_kill={value:?}: one of none, own_limit, node_pressure"
                    ))
                })?);
            }
            "sort" if filtered => {
                let found = Phase::NAMED.iter().find(|(_, n)| *n == value);
                query.filter.sort = Some(found.map(|(p, _)| *p).ok_or_else(|| {
                    Bad::Invalid(format!(
                        "sort={value:?}: one of wall_ms, execution_ms, queue_ms, \
                         input_fetch_ms, output_upload_ms (each longest first)"
                    ))
                })?);
            }
            _ => {
                return Err(Bad::Invalid(format!(
                    "unknown parameter {key:?}; this route takes {takes}"
                )));
            }
        }
        seen.push(key.into_owned());
    }
    Ok(paged.then_some(query))
}

/// Where a page starts, in the order its route lists entries. Rows are only added,
/// at the end, so a position stays valid while the record is held.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Cursor {
    /// A route walked in row order, one entry at a time.
    Walk(Walk),
    /// `/actions?sort=<phase>`: the duration (microseconds) and index of the last
    /// row listed, `None` before the first page. Rows come longest first, ties in row
    /// order; a page is read in one pass ([`sorted`]). Only `queue` changes once
    /// measured, and only grows (by each lease granted after a requeue): a row whose
    /// queueing grows past the cursor's after it was passed is not listed again, and
    /// none is listed twice. The other phases are measured once the operation has
    /// ended and never change.
    Sorted {
        phase: Phase,
        after: Option<(u64, usize)>,
    },
}

/// A position in a route walked in row order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Walk {
    /// `/actions`: the next row to look at.
    Actions { row: usize },
    /// `/artifacts`: the next output of a row to look at.
    Artifacts { row: usize, output: usize },
    /// `/logs`: the next row to look at; `rest: None` while it lists failed rows,
    /// then `Some(n)` with `n` the number of rows there were when those ran out.
    Logs { row: usize, rest: Option<usize> },
}

/// One entry a cursor found: a row, or one output of a row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Entry {
    Row(usize),
    Output(usize, usize),
}

/// Whether the row's result lists a stdout or a stderr.
fn has_logs(row: &ActionRow) -> bool {
    row.listing
        .as_ref()
        .is_some_and(|l| l.stdout.is_some() || l.stderr.is_some())
}

/// Whether the client saw the row's action fail: an outcome other than OK exit 0.
pub(super) fn failed(row: &ActionRow) -> bool {
    row.outcome.as_ref().is_some_and(Outcome::is_failure)
}

/// The rows `filter` keeps whose duration of `phase` comes after `after` in the
/// order of [`Cursor::Sorted`], the first `n` of them in that order: each row's
/// duration and index.
fn sorted(
    rows: &[ActionRow],
    filter: Filter,
    phase: Phase,
    after: Option<(u64, usize)>,
    n: usize,
) -> Vec<(u64, usize)> {
    let order = |a: &(u64, usize), b: &(u64, usize)| b.0.cmp(&a.0).then(a.1.cmp(&b.1));
    let mut found: Vec<(u64, usize)> = rows
        .iter()
        .enumerate()
        .filter(|(_, r)| filter.keeps(r))
        .filter_map(|(i, r)| Some((r.durations().of(phase)?, i)))
        .filter(|e| after.is_none_or(|a| order(&a, e).is_lt()))
        .collect();
    if found.len() > n {
        found.select_nth_unstable_by(n, order);
        found.truncate(n);
    }
    found.sort_unstable_by(order);
    found
}

impl Cursor {
    /// Where a route's first page starts, in the order `filter` asks.
    const fn start(route: Route, filter: Filter) -> Self {
        match (route, filter.sort) {
            (Route::Artifacts, _) => Self::Walk(Walk::Artifacts { row: 0, output: 0 }),
            (Route::Logs, _) => Self::Walk(Walk::Logs { row: 0, rest: None }),
            (_, Some(phase)) => Self::Sorted { phase, after: None },
            _ => Self::Walk(Walk::Actions { row: 0 }),
        }
    }

    /// The route's name, as a page token carries it.
    const fn kind(self) -> &'static str {
        match self {
            Self::Walk(Walk::Actions { .. }) | Self::Sorted { .. } => "actions",
            Self::Walk(Walk::Artifacts { .. }) => "artifacts",
            Self::Walk(Walk::Logs { .. }) => "logs",
        }
    }
}

impl Walk {
    /// The first entry at or after the cursor that `filter` keeps, and the cursor just
    /// after it; `None` when there is none.
    ///
    /// `/logs` lists the failed rows that have logs, in row order; when they run out,
    /// it lists the other rows with logs among those there were then, and every row
    /// with logs added since, failed or not. So no row added while it is paged is
    /// missed or listed twice; a row that ends while it is paged is listed where its
    /// outcome put it when the page that passed it was read.
    fn next(self, rows: &[ActionRow], filter: Filter) -> Option<(Entry, Self)> {
        match self {
            Self::Actions { row } => {
                let i = (row..rows.len()).find(|&i| filter.keeps(&rows[i]))?;
                Some((Entry::Row(i), Self::Actions { row: i + 1 }))
            }
            Self::Artifacts { row, output } => {
                let outputs = |i: usize| rows[i].listing.as_ref().map_or(0, |l| l.outputs.len());
                // The cursor's own row from its output on; every later row from its first.
                let from = |i: usize| if i == row { output } else { 0 };
                let i = (row..rows.len()).find(|&i| outputs(i) > from(i))?;
                let o = from(i);
                Some((
                    Entry::Output(i, o),
                    Self::Artifacts {
                        row: i,
                        output: o + 1,
                    },
                ))
            }
            Self::Logs { row, rest: None } => {
                match (row..rows.len()).find(|&i| has_logs(&rows[i]) && failed(&rows[i])) {
                    Some(i) => Some((
                        Entry::Row(i),
                        Self::Logs {
                            row: i + 1,
                            rest: None,
                        },
                    )),
                    None => Self::Logs {
                        row: 0,
                        rest: Some(rows.len()),
                    }
                    .next(rows, filter),
                }
            }
            Self::Logs {
                row,
                rest: Some(then),
            } => {
                let listed = |i: usize| has_logs(&rows[i]) && (i >= then || !failed(&rows[i]));
                let i = (row..rows.len()).find(|&i| listed(i))?;
                Some((
                    Entry::Row(i),
                    Self::Logs {
                        row: i + 1,
                        rest: Some(then),
                    },
                ))
            }
        }
    }
}

/// The version a page token's text starts with.
const TOKEN_VERSION: &str = "1";

/// A page token: the record it was made for (its [`Invocation::serial`]), the filter,
/// and where the next page starts. Sent hex-encoded; clients treat it as opaque.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Token {
    serial: u64,
    filter: Filter,
    cursor: Cursor,
}

impl Token {
    /// The token as it is sent.
    pub(crate) fn encode(self) -> String {
        let (a, b) = match self.cursor {
            Cursor::Walk(Walk::Actions { row }) => (row, 0),
            Cursor::Walk(Walk::Artifacts { row, output }) => (row, output as u64),
            Cursor::Walk(Walk::Logs { row, rest }) => (row, rest.map_or(0, |n| n as u64 + 1)),
            Cursor::Sorted { after, .. } => after.map_or((0, 0), |(value, row)| (row + 1, value)),
        };
        let text = format!(
            "{TOKEN_VERSION}:{}:{}:{}:{a}:{b}",
            self.serial,
            self.cursor.kind(),
            self.filter.text()
        );
        hex::encode(text)
    }

    /// The cursor `text` names for a page of `start`'s route with `filter` in `inv`.
    ///
    /// # Errors
    /// [`Bad::Invalid`] for a token that does not decode, names another route or
    /// another filter, or a position past the record's rows; [`Bad::Gone`] for a
    /// token made for another record (one removed since and made again under the same
    /// key, say): its position is never read against this one.
    pub(crate) fn decode(
        text: &str,
        inv: &Invocation,
        start: Cursor,
        filter: Filter,
    ) -> Result<Cursor, Bad> {
        let malformed = || Bad::Invalid(format!("{text:?} is not a page token"));
        let decoded = hex::decode(text)
            .ok()
            .and_then(|b| String::from_utf8(b).ok())
            .ok_or_else(malformed)?;
        let fields: Vec<&str> = decoded.split(':').collect();
        let [TOKEN_VERSION, serial, kind, filtered, a, b] = fields[..] else {
            return Err(malformed());
        };
        let serial: u64 = serial.parse().map_err(|_| malformed())?;
        same_record(inv, serial)?;
        if kind != start.kind() {
            return Err(Bad::Invalid(format!(
                "this page token was made for /{kind}, not /{}",
                start.kind()
            )));
        }
        if filtered != filter.text() {
            return Err(Bad::Invalid(
                "this page token was made for another filter: send the outcome, how, \
                 requeue_reason, memory_kill and sort of the first page"
                    .to_owned(),
            ));
        }
        let (a, b): (usize, u64) = (
            a.parse().map_err(|_| malformed())?,
            b.parse().map_err(|_| malformed())?,
        );
        let rows = inv.rows().len();
        let walk = match start {
            Cursor::Sorted { phase, .. } => {
                if a > rows {
                    return Err(malformed());
                }
                let after = a.checked_sub(1).map(|row| (b, row));
                return Ok(Cursor::Sorted { phase, after });
            }
            Cursor::Walk(walk) => walk,
        };
        let rest = b.checked_sub(1);
        if a > rows || rest.is_some_and(|n| n > rows as u64) {
            return Err(malformed());
        }
        // Both fit: `b` is at most one past the rows.
        let (b, rest) = (b as usize, rest.map(|n| n as usize));
        Ok(Cursor::Walk(match walk {
            Walk::Actions { .. } => Walk::Actions { row: a },
            Walk::Artifacts { .. } => Walk::Artifacts { row: a, output: b },
            Walk::Logs { .. } => Walk::Logs { row: a, rest },
        }))
    }
}

/// `Ok` when `inv` is the record `serial` names.
///
/// # Errors
/// [`Bad::Gone`] when it is another one: the record the serial was read from is gone,
/// and this is a new one under its key.
fn same_record(inv: &Invocation, serial: u64) -> Result<(), Bad> {
    if inv.serial() == serial {
        Ok(())
    } else {
        Err(Bad::Gone)
    }
}

/// The instance names a reader of `inv` must be allowed on, whatever the route: see
/// [`reader_names`].
pub(crate) fn instance_names(inv: &Invocation) -> Vec<String> {
    reader_names(inv.instances(), inv.rows())
}

/// The instance names a reader of a record with `instances` and `rows` must be allowed
/// on, sorted: each of `instances` (every name its calls used that fit) and every
/// row's instance, or the empty name when there are none (its first call's did not
/// fit). The recorder never sets a row of a name the record does not hold; the
/// rows are asked about anyway, so a page is not served on that alone.
pub(crate) fn reader_names(instances: &BTreeSet<String>, rows: &[ActionRow]) -> Vec<String> {
    let mut names: BTreeSet<&str> = instances.iter().map(String::as_str).collect();
    names.extend(rows.iter().map(|r| r.instance.as_str()));
    let none = names.is_empty().then_some("");
    names.into_iter().chain(none).map(str::to_owned).collect()
}

/// `Ok` when `inv` is still the record whose `serial` and instance `names` the reader
/// was authorized for.
///
/// # Errors
/// [`Bad::Gone`] for another record under the key; [`Bad::Changed`] when the
/// record has gained an instance name since.
pub(crate) fn still_authorized(inv: &Invocation, serial: u64, names: &[String]) -> Result<(), Bad> {
    same_record(inv, serial)?;
    if instance_names(inv) == names {
        Ok(())
    } else {
        Err(Bad::Changed(
            "the invocation gained an instance name while it was read; send the request \
             again"
                .to_owned(),
        ))
    }
}

pub(crate) fn answer(
    inv: &Invocation,
    request: &Request,
    authorized: (u64, &[String]),
    blobs_on: bool,
    now: WallTime,
) -> Result<Draft, Bad> {
    still_authorized(inv, authorized.0, authorized.1)?;
    let mut b = Builder {
        blobs_on,
        found: Vec::new(),
    };
    let base = format!("{PREFIX}{}", inv.key());
    let value = match (request.route, &request.page) {
        (Route::Action(digest), _) => {
            let row = inv.row(&digest).ok_or_else(|| {
                Bad::NotFound(format!(
                    "invocation {} has no row for action {}",
                    inv.key(),
                    digest_text(&digest)
                ))
            })?;
            Value::Object(action(&mut b, row, &base))
        }
        (Route::Metrics, _) => metrics(inv),
        (route, Some(page)) => paged(&mut b, inv, route, page, &base)?,
        (_, None) => invocation(inv, &base, now),
    };
    Ok(Draft {
        value,
        blobs: b.found,
    })
}

/// A page of `route`.
fn paged(
    b: &mut Builder,
    inv: &Invocation,
    route: Route,
    page: &PageQuery,
    base: &str,
) -> Result<Value, Bad> {
    let start = Cursor::start(route, page.filter);
    let mut cursor = match &page.token {
        Some(text) => Token::decode(text, inv, start, page.filter)?,
        None => start,
    };
    let rows = inv.rows();
    let (found, more): (Vec<Entry>, bool) = match cursor {
        // One pass over the rows for the page and whether another follows (one row
        // past it), not one per entry.
        Cursor::Sorted { phase, after } => {
            let mut found = sorted(rows, page.filter, phase, after, page.size + 1);
            let more = found.len() > page.size;
            found.truncate(page.size);
            if let Some(&last) = found.last() {
                cursor = Cursor::Sorted {
                    phase,
                    after: Some(last),
                };
            }
            (
                found.into_iter().map(|(_, i)| Entry::Row(i)).collect(),
                more,
            )
        }
        Cursor::Walk(mut walk) => {
            let mut found = Vec::new();
            while found.len() < page.size {
                let Some((entry, next)) = walk.next(rows, page.filter) else {
                    break;
                };
                found.push(entry);
                walk = next;
            }
            cursor = Cursor::Walk(walk);
            (found, walk.next(rows, page.filter).is_some())
        }
    };
    let mut entries = Vec::new();
    for entry in found {
        let at = format!("/{}/{}", start.kind(), entries.len());
        entries.push(match entry {
            Entry::Row(i) if route == Route::Logs => logs(b, &rows[i], base, &at),
            Entry::Row(i) => Value::Object(row(&rows[i], base)),
            Entry::Output(i, o) => artifact(b, &rows[i], o, &at),
        });
    }
    let mut value = Map::new();
    value.insert(start.kind().to_owned(), Value::Array(entries));
    if more {
        let token = Token {
            serial: inv.serial(),
            filter: page.filter,
            cursor,
        }
        .encode();
        // The link an agent follows: the same filter, page size and the token.
        let mut query = form_urlencoded::Serializer::new(String::new());
        page.filter.append_to(&mut query);
        query.append_pair("page_size", &page.size.to_string());
        query.append_pair("page_token", &token);
        let next = format!("{base}/{}?{}", start.kind(), query.finish());
        value.insert("next_page".to_owned(), next.into());
        value.insert("next_page_token".to_owned(), token.into());
    }
    Ok(Value::Object(value))
}
