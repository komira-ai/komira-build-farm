//! What the cross-build routes refuse, hide or say is not there. Each test names the
//! defect it catches.

use kbf_history::{IndexRoot, InvKey};
use kbf_state::{Cond, Store as _, Txn};
use serde_json::Value;

use crate::kit::{indexed, run, shape};
use crate::pages::{PAGES_ONLY, Server};
use crate::world::{ONLY_A, World, ran};

const SHARED: u64 = 0x1111_1111_1111_1111;
const THEIRS: u64 = 0x3333_3333_3333_3333;
const COMPILE: u64 = 0x2222_2222_2222_2222;

/// The `id`s of a list, the `key`s of a row list.
fn names(answer: &Value, list: &str, field: &str) -> Vec<String> {
    answer[list]
        .as_array()
        .expect("a list")
        .iter()
        .map(|i| i[field].as_str().expect("a name").to_owned())
        .collect()
}

/// A server over history with two instances' builds: `mine` on `a/main`, `theirs` on `b`
/// and `span` on both. Test `SHARED` fails on both instances, `THEIRS` only on `b`;
/// `COMPILE` is no test, on `a/main`.
async fn two_instances(world: &World) {
    let a = world.start(ONLY_A);
    let mut events = run("mine", "a/main", shape(SHARED, "c", true, None), 1, 2);
    events.extend(run(
        "mine",
        "a/main",
        shape(COMPILE, "-", false, None),
        2,
        0,
    ));
    events.extend(run("theirs", "b", shape(SHARED, "c", true, None), 3, 2));
    events.extend(run("theirs", "b", shape(THEIRS, "c", true, None), 4, 2));
    events.extend(ran("span", "a/main", 5, 5, 0));
    events.extend(ran("span", "b", 6, 6, 0));
    a.feed(events).await;
    world.flushed("mine", 2).await;
    world.flushed("theirs", 2).await;
    world.flushed("span", 2).await;
    indexed(world, 4, 3).await;
    crate::kit::until("every by-status row", || {
        world.rows("ui/statusx/failing/").len() == 3
    })
    .await;
    a.stop().await;
}

/// Catches (security, ui-data 8; mutants "authorize only the query's `instance=`" and "a
/// fan-out that skips the all-must-allow check on spanning invocations"): a list, a shape
/// list, a test list, a status list, a key's rows or a ring that holds a row of an instance
/// the caller may not read (read from the instance's own rows, or from a global status
/// list that holds every instance's), a spanning invocation listed under the instance the
/// caller may read, and an instance named in the query that is refused with an empty
/// answer where the build pages refuse with `403`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_caller_sees_only_its_instances_on_every_route() {
    let world = World::new();
    two_instances(&world).await;
    let b = world.start(ONLY_A);

    let list = b.ok("/v1/invocations").await;
    assert_eq!(names(&list, "invocations", "id"), ["mine"]);
    assert_eq!(
        names(
            &b.ok("/v1/invocations?instance=a%2Fmain").await,
            "invocations",
            "id"
        ),
        ["mine"],
        "the spanning build is listed under the instance the caller may read, and not shown"
    );
    let rows = |answer: &Value| -> Vec<(String, String)> {
        answer["shapes"]
            .as_array()
            .expect("a list")
            .iter()
            .map(|s| {
                (
                    s["instance"].as_str().expect("an instance").to_owned(),
                    s["key"].as_str().expect("a key").to_owned(),
                )
            })
            .collect()
    };
    let mine = |key: u64| ("a/main".to_owned(), format!("{key:016x}"));
    assert_eq!(
        rows(&b.ok("/v1/shapes").await),
        [mine(SHARED), mine(COMPILE)]
    );
    assert_eq!(rows(&b.ok("/v1/tests").await), [mine(SHARED)]);
    assert_eq!(
        rows(&b.ok("/v1/tests?status=failing").await),
        [mine(SHARED)],
        "the status index holds every instance's rows"
    );
    assert_eq!(
        rows(&b.ok("/v1/shapes/1111111111111111").await),
        [mine(SHARED)]
    );
    assert_eq!(
        rows(&b.ok("/v1/shapes/1111111111111111?cfg=c").await),
        [mine(SHARED)]
    );
    assert_eq!(
        b.get("/v1/shapes/3333333333333333").await.code,
        404,
        "a key only another instance has is not known"
    );
    // An instance the caller may not read: refused, on every route that takes one.
    for route in [
        "/v1/invocations?instance=b",
        "/v1/shapes?instance=b",
        "/v1/tests?instance=b",
        "/v1/tests?status=failing&instance=b",
        "/v1/shapes/1111111111111111?instance=b&cfg=c",
        "/v1/shapes/1111111111111111?instance=b",
        "/v1/shapes/1111111111111111/runs?instance=b&cfg=c",
    ] {
        let answer = b.get(route).await;
        assert_eq!(answer.code, 403, "{route}: {}", answer.json);
        assert!(answer.json.get("shapes").is_none() && answer.json.get("runs").is_none());
    }
    assert_eq!(
        b.get("/v1/shapes/3333333333333333?instance=a%2Fmain&cfg=c")
            .await
            .code,
        404
    );
    b.stop().await;
}

/// Catches (mutant "`slower` mapped to an empty page"): a status the index cannot answer
/// that is answered with no tests (a reader told that nothing is slower), and filters on
/// what the catalog does not keep that are ignored. `slower` needs the day-close fold,
/// `target` and `program` need the catalog's body: each says so, by a name, as `400`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_slower_is_400_not_empty() {
    let world = World::new();
    let a = world.start(PAGES_ONLY);
    for route in [
        "/v1/shapes?status=slower",
        "/v1/tests?status=slower",
        "/v1/tests?status=slower&instance=main",
    ] {
        let answer = a.get(route).await;
        assert_eq!(answer.code, 400, "{route}: {}", answer.json);
        assert_eq!(answer.json["error"], "unsupported_status", "{route}");
        assert!(
            answer.json["detail"]
                .as_str()
                .is_some_and(|d| d.contains("fold"))
        );
        assert!(answer.json.get("shapes").is_none());
    }
    for route in [
        "/v1/shapes?target=komira//a:b",
        "/v1/tests?program=cc",
        "/v1/invocations?tool=buck2",
    ] {
        let answer = a.get(route).await;
        assert_eq!(answer.code, 400, "{route}: {}", answer.json);
        assert_eq!(answer.json["error"], "unsupported_filter", "{route}");
    }
    a.stop().await;
}

/// Catches: a `commit=` filter that is accepted and ignored (the list of every build
/// answering a question about one commit): no commit is kept before the client report
/// exists, so it is `400`, named `needs_report`, whatever else the query holds.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn commit_filter_is_400() {
    let world = World::new();
    let a = world.start(PAGES_ONLY);
    a.feed(ran("b", "main", 1, 1, 0)).await;
    world.flushed("b", 1).await;
    for route in [
        "/v1/invocations?commit=abc123",
        "/v1/invocations?commit=abc123&instance=main&state=open",
    ] {
        let answer = a.get(route).await;
        assert_eq!(answer.code, 400, "{route}: {}", answer.json);
        assert_eq!(answer.json["error"], "needs_report");
        assert!(answer.json.get("invocations").is_none());
    }
    assert_eq!(a.get("/v1/invocations").await.code, 200);
    a.stop().await;
}

/// Every route of `routes` answers `400` with the error `code`.
async fn expect(server: &Server, code: &str, routes: Vec<String>) {
    for route in routes {
        let answer = server.get(&route).await;
        assert_eq!(answer.code, 400, "{route}: {}", answer.json);
        assert_eq!(answer.json["error"], code, "{route}: {}", answer.json);
    }
}

/// Catches: a malformed query that is answered as another, and a page token that is
/// used where it was not made: another route, other filters, an invented position. Every
/// answer is `400` with a name a client can match, and none reads the stores.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_bad_query_or_token_is_400_with_a_name() {
    let world = World::new();
    let a = world.start(PAGES_ONLY);
    let token = |text: &str| hex::encode(text);
    expect(
        &a,
        "bad_request",
        [
            "/v1/invocations?foo=1",
            "/v1/invocations?state=open&state=closed",
            "/v1/invocations?page_size=0",
            "/v1/invocations?page_size=201",
            "/v1/invocations?page_size=many",
            "/v1/invocations?since=yesterday",
            "/v1/invocations?until=2026-10-11",
            "/v1/invocations?state=half",
            "/v1/shapes?kind=bogus",
            "/v1/tests?kind=other",
            "/v1/shapes?status=bogus",
            "/v1/shapes?kind=other&status=failing",
            "/v1/shapes/xyz",
            "/v1/shapes/ABCDEF0123456789",
            "/v1/shapes/1111111111111111/runs",
            "/v1/shapes/1111111111111111/runs?instance=main",
            "/v1/shapes/1111111111111111/runs?cfg=c",
            "/v1/shapes/1111111111111111?page_size=0",
        ]
        .map(str::to_owned)
        .into(),
    )
    .await;
    expect(
        &a,
        "bad_page_token",
        [
            "/v1/invocations?page_token=not-hex".to_owned(),
            // Made for another route.
            format!("/v1/tests?page_token={}", token("/v1/shapes\n\n1:a")),
            // Made under other filters.
            format!(
                "/v1/tests?page_token={}",
                token("/v1/tests\nstatus=failing\n1:a")
            ),
            // Fewer fields than a token has.
            format!("/v1/shapes?page_token={}", token("/v1/shapes\n")),
            // A position that is no place: no cursor text, a control character, no length,
            // a length that is not one, a length past the text, a length inside a letter.
            format!(
                "/v1/invocations?page_token={}",
                token("/v1/invocations\n\n\u{1}")
            ),
            format!(
                "/v1/invocations?page_token={}",
                token("/v1/invocations\n\n")
            ),
            format!(
                "/v1/tests?status=failing&page_token={}",
                token("/v1/tests\nstatus=failing\n\u{1}")
            ),
            format!("/v1/shapes?page_token={}", token("/v1/shapes\n\nnolength")),
            format!("/v1/shapes?page_token={}", token("/v1/shapes\n\nx:abc")),
            format!("/v1/shapes?page_token={}", token("/v1/shapes\n\n99:ab")),
            format!("/v1/shapes?page_token={}", token("/v1/shapes\n\n1:\u{e9}")),
            format!("/v1/shapes?page_token={}", token("/v1/shapes\n\n1:a\u{1}")),
        ]
        .into(),
    )
    .await;
    a.stop().await;
}

/// Catches: a caller with no credential answered (the routes read the authenticator the
/// build pages do, first), and routes that answer when the policy names no
/// `invocations.getAuthorizer` (they are off, `404`, as the build pages are).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_routes_follow_the_build_pages_policy() {
    let routes = [
        "/v1/invocations",
        "/v1/shapes",
        "/v1/tests",
        "/v1/shapes/1111111111111111",
        "/v1/shapes/1111111111111111/runs?instance=main&cfg=c",
    ];
    let world = World::new();
    let denied = world.start(
        r#"{"authenticationPolicy": {"deny": "who are you"},
            "invocations": {"getAuthorizer": {"allow": {}}}}"#,
    );
    for route in routes {
        let answer = denied.get(route).await;
        assert_eq!(answer.code, 401, "{route}: {}", answer.json);
    }
    denied.stop().await;
    let off = world.start(r#"{"authenticationPolicy": {"allow": {}}}"#);
    for route in routes {
        let answer = off.get(route).await;
        assert_eq!(answer.code, 404, "{route}: {}", answer.json);
        assert_eq!(answer.json["off"], "invocations", "{route}");
    }
    off.stop().await;
}

/// A response head without its `date` line, which differs between two answers a second
/// apart.
fn undated(head: &str) -> Vec<&str> {
    head.lines()
        .filter(|line| !line.to_ascii_lowercase().starts_with("date:"))
        .collect()
}

/// Catches (cutover, ui-data 5.5): a server with no history that has the routes, or
/// answers them differently from a path it never had: without history there is nothing
/// to read, and the answer is the one an unknown path gets, byte for byte.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_history_the_routes_do_not_exist() {
    let server = Server::with(PAGES_ONLY);
    let unknown = server.get("/v1/never_a_route").await;
    for route in [
        "/v1/invocations",
        "/v1/invocations?commit=x",
        "/v1/shapes",
        "/v1/tests?status=slower",
        "/v1/shapes/1111111111111111",
        "/v1/shapes/1111111111111111/runs?instance=a&cfg=b",
    ] {
        let answer = server.get(route).await;
        assert_eq!(answer.code, unknown.code, "{route}");
        assert_eq!(answer.body, unknown.body, "{route}");
        assert_eq!(undated(&answer.head), undated(&unknown.head), "{route}");
    }
    server.stop().await;
}

/// Catches: more instances than the index reads at once passed off as all of them: a
/// list over 257 instances answers from 256 and says `partial`, on every route that reads
/// across instances (the invocation list, the shape list and the rows of a key), and a
/// list for one instance (no names read) does not. The instances after `main` are ghosts,
/// each with a list row for a build with no header: there to be counted, and nothing to read.
/// The rows of a key stop after 16 reads of the index with a link to go on from, whether
/// they found a row (`main` has one) or none.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_list_over_too_many_instances_says_partial() {
    let world = World::new();
    let a = world.start(PAGES_ONLY);
    a.feed(run("real", "main", shape(SHARED, "c", true, None), 1, 0))
        .await;
    world.flushed("real", 1).await;
    let root = IndexRoot::default();
    let ghost = InvKey::from_record_id("ghost");
    let mut puts = Txn::new();
    for i in 0..257 {
        puts = puts.put(
            root.inv_list(&format!("z{i:03}"), 1, &ghost),
            vec![1],
            Cond::Any,
        );
    }
    world
        .state
        .fake
        .txn(puts)
        .expect("a write")
        .expect("no conflict");
    let all = a.ok("/v1/invocations").await;
    assert_eq!(all["partial"], true, "{all}");
    assert_eq!(names(&all, "invocations", "id"), ["real"]);
    let one = a.ok("/v1/invocations?instance=main").await;
    assert!(one.get("partial").is_none());
    assert_eq!(names(&one, "invocations", "id"), ["real"]);
    // The same bound on the shape list, and on the rows of a key.
    assert_eq!(a.ok("/v1/shapes").await["partial"], true);
    let key = a.ok("/v1/shapes/1111111111111111").await;
    assert_eq!(key["partial"], true, "{key}");
    assert_eq!(names(&key, "shapes", "instance"), ["main"]);
    assert!(key["next_page"].is_string(), "16 reads, then a link: {key}");
    let none = a.ok("/v1/shapes/4444444444444444").await;
    assert_eq!(
        none["shapes"],
        serde_json::json!([]),
        "no row is not a 404 while there is more to read: {none}"
    );
    assert_eq!(none["partial"], true);
    assert!(none["next_page"].is_string());
    let narrow = a.ok("/v1/shapes/1111111111111111?instance=main").await;
    assert!(narrow.get("partial").is_none(), "{narrow}");
    a.stop().await;
}
