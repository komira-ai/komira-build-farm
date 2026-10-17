//! The build pages over a real listener (`docs/design/invocations.md` sections 4.1 to
//! 4.4, `docs/api.md`): the policy keys that turn them on, what each route answers,
//! and which `{id}` is a record. Paging is `tests/invocations_paging.rs`.

mod pages;

use std::sync::Arc;

use kbf_auth::{
    AllowAuthenticator, AllowAuthorizer, AuthenticationMetadata, Authorizer, Authorizers,
    BoxFuture, Policy,
};
use kbf_invocation::{CallKind, LeaseEnd, Outcome};
use kbf_sched::RequeueReason;
use kbf_types::Failure;
use pages::{
    OPEN, PAGES_ONLY, Server, at, cached, call, digest, ended, finished, granted, hit, listing,
    queued, queued_target, requeued, started, text, usage,
};
use serde_json::{Value, json};
use tonic::Status;

/// Every route of invocation `id`.
fn routes(id: &str, action: &str) -> Vec<String> {
    let base = format!("/v1/invocations/{id}");
    ["", "/actions", "/artifacts", "/logs", "/metrics"]
        .into_iter()
        .map(|r| format!("{base}{r}"))
        .chain([format!("{base}/actions/{action}")])
        .collect()
}

/// Catches (mutant "default allow": `InvocationReaders::from_policy` answering with an
/// `allow` when the policy names no `invocations.getAuthorizer`): any route answering
/// without a policy file, with a policy that names other keys, or with `blobs` only;
/// and a route not answering once the key is named. And (mutant "4.10's body for a
/// route that is off") an off route's `404` without `"off": "invocations"`, or with
/// the `no_record` body that means "no record of your build". The rest of the API
/// answers either way.
#[tokio::test]
async fn the_pages_are_off_unless_the_policy_names_their_authorizer() {
    let action = digest(1);
    for (what, policy, on) in [
        ("no policy file", Policy::allow_all(), false),
        (
            "no invocations key",
            Policy::from_json(
                r#"{"authenticationPolicy": {"allow": {}},
                    "executeAuthorizer": {"allow": {}},
                    "blobs": {"getAuthorizer": {"allow": {}}}}"#,
            )
            .expect("a policy"),
            false,
        ),
        (
            "invocations named",
            Policy::from_json(PAGES_ONLY).expect("a policy"),
            true,
        ),
    ] {
        let server = Server::start(policy, kbf_front::DEFAULT_INVOCATION_MEMORY);
        let l = listing(digest(2), 0, &[digest(3)], (Some(digest(4)), None));
        server.feed([hit("build-1", at(0), action, l)]).await;
        for route in routes("build-1", &text(&action)) {
            let answer = server.get(&route).await;
            let want = if on { 200 } else { 404 };
            assert_eq!(answer.code, want, "{what} {route}: {}", answer.json);
            let off = answer.json["error"]
                .as_str()
                .is_some_and(|e| e.contains("the build pages are off"));
            assert_eq!(off, !on, "{what} {route}: {}", answer.json);
            let says_off = answer.json["off"] == "invocations";
            assert_eq!(says_off, !on, "{what} {route}: {}", answer.json);
        }
        // A record that is not held: off says so, and never `no_record`.
        let missing = server.get("/v1/invocations/no-such-build").await;
        assert_eq!(missing.code, 404, "{what}");
        let no_record = missing.json["error"] == "no_record";
        assert_eq!(no_record, on, "{what}: {}", missing.json);
        assert_eq!(missing.json["off"] == "invocations", !on, "{what}");
        assert_eq!(server.get("/v1/nodes").await.code, 200, "{what}");
        server.stop().await;
    }
}

/// Catches (mutant "href ignores the blob route": `Builder::href` linking with the
/// route off): a page linking `/v1/blobs` when the policy names `invocations` but no
/// `blobs` key, where that route answers `404`; and (with both keys) links that are
/// missing, or that the blob route does not answer with the blob's bytes, stdout as
/// inline text.
#[tokio::test]
async fn hrefs_are_null_without_the_blobs_key_and_resolve_with_it() {
    for (policy, blobs) in [(PAGES_ONLY, false), (OPEN, true)] {
        let server = Server::with(policy);
        let out = server.store(b"an object file").await;
        let log = server.store(b"compiling\n").await;
        let action = digest(10);
        let l = listing(digest(11), 1, &[out], (Some(log), Some(digest(12))));
        server.feed([hit("build-2", at(0), action, l)]).await;
        let base = "/v1/invocations/build-2";
        let detail = server
            .ok(&format!("{base}/actions/{}", text(&action)))
            .await;
        let artifacts = server.ok(&format!("{base}/artifacts")).await;
        let logs = server.ok(&format!("{base}/logs")).await;
        let hrefs = [
            &detail["outputs"][0]["href"],
            &detail["stdout"]["href"],
            &detail["stderr"]["href"],
            &detail["blobs"]["action"],
            &artifacts["artifacts"][0]["href"],
            &logs["logs"][0]["stdout"]["href"],
        ];
        for href in hrefs {
            assert_eq!(href.is_string(), blobs, "{policy}: {href}");
        }
        if blobs {
            let output = server
                .get(detail["outputs"][0]["href"].as_str().expect("an href"))
                .await;
            assert_eq!(
                (output.code, output.body.as_slice()),
                (200, &b"an object file"[..])
            );
            assert!(
                output.head.contains("filename*=UTF-8''0.o"),
                "{}",
                output.head
            );
            let stdout = detail["stdout"]["href"].as_str().expect("an href");
            assert!(stdout.ends_with("&inline=text"), "{stdout}");
            let stdout = server.get(stdout).await;
            assert_eq!(
                (stdout.code, stdout.body.as_slice()),
                (200, &b"compiling\n"[..])
            );
            assert!(stdout.head.contains("text/plain"), "{}", stdout.head);
        }
        // Whether each blob is held does not depend on the blob route.
        assert_eq!(detail["outputs"][0]["available"], true, "{policy}");
        assert_eq!(detail["stdout"]["available"], true, "{policy}");
        assert_eq!(detail["stderr"]["available"], false, "{policy}");
        server.stop().await;
    }
}

/// Catches (mutant "ask about the first instance only": the authorizer asked with
/// `names[..1]`): a page served to a caller allowed on one of the invocation's
/// instance names but not another; and a page refused to a caller allowed on all.
/// Every route of the invocation is refused, `403`.
#[tokio::test]
async fn an_authorizer_refusing_one_instance_refuses_the_page() {
    let server = Server::with(
        r#"{"authenticationPolicy": {"allow": {}},
            "invocations": {"getAuthorizer": {"instanceNamePrefix": {"allowedInstanceNamePrefixes": ["ci"]}}}}"#,
    );
    let (a, b) = (digest(20), digest(21));
    server
        .feed([
            // `ci/main` sorts before `other`: the first name is the allowed one.
            queued("mixed", "ci/main", at(0), a, 1),
            queued("mixed", "other", at(1), b, 2),
            queued("ci-only", "ci", at(2), a, 3),
            queued("ci-only", "ci/main", at(3), b, 4),
        ])
        .await;
    for route in routes("mixed", &text(&a)) {
        let answer = server.get(&route).await;
        assert_eq!(answer.code, 403, "{route}: {}", answer.json);
        let why = answer.json["error"].as_str().unwrap_or_default();
        assert!(why.starts_with("Authorization: "), "{route}: {why}");
    }
    for route in routes("ci-only", &text(&a)) {
        assert_eq!(server.get(&route).await.code, 200, "{route}");
    }
    server.stop().await;
}

/// Catches (security): a row whose instance name the record never recorded served to a
/// caller allowed on the recorded names only. At the memory bound the record cannot
/// take the new name `secret` (its bytes do not fit), so a reader is asked about
/// `ci/...` alone; a later Execute of the same action must not move the row to
/// `secret`. Two layers stop it: the recorder leaves the row as it was (and the
/// record `complete: false`), and every route asks about each row's instance too.
/// Red with both removed; either alone keeps it green (see the record test in
/// `kbf-invocation`'s `tests/bound.rs` and `rows_instances_are_asked_about_too`).
#[tokio::test]
async fn a_name_the_record_could_not_take_is_not_read_through_a_row() {
    const CI_ONLY: &str = r#"{"authenticationPolicy": {"allow": {}},
        "invocations": {"getAuthorizer": {"instanceNamePrefix": {"allowedInstanceNamePrefixes": ["ci"]}}}}"#;
    let a = digest(20);
    let first = queued("leak", "ci/aaaaaaaaaaaaaaaa", at(0), a, 1);
    let probe = Server::with(CI_ONLY);
    probe.feed([first.clone()]).await;
    let (bytes, _) = probe.recorder.bytes();
    probe.stop().await;
    let server = Server::start(Policy::from_json(CI_ONLY).expect("a policy"), bytes);
    server
        .feed([first, queued("leak", "secret", at(1), a, 2)])
        .await;
    for route in routes("leak", &text(&a)) {
        let answer = server.get(&route).await;
        // A refusal may name the instance; a page must not.
        assert!(
            answer.code != 200 || !answer.json.to_string().contains("secret"),
            "{route}: a caller allowed on ci only read instance secret: {}",
            answer.json
        );
    }
    server.stop().await;
}

/// Catches (security): what a call left out of its record still does to it. At the
/// memory bound the record cannot take the name `secret`, so a reader is asked about
/// `ci/...` alone; an upload and an Execute under `secret` must then leave every
/// field a page shows as it was (counts, `last_seen`, state, rows), the record only
/// saying `complete: false` and, as the bound closed it to rows making room for the
/// name, `rows_capped: true`. Red with the counts or `last_seen` applied before the
/// instance name is checked.
#[tokio::test]
async fn a_call_under_a_name_the_record_could_not_take_changes_no_page() {
    const CI_ONLY: &str = r#"{"authenticationPolicy": {"allow": {}},
        "invocations": {"getAuthorizer": {"instanceNamePrefix": {"allowedInstanceNamePrefixes": ["ci"]}}}}"#;
    let first = queued("leak", "ci/aaaaaaaaaaaaaaaa", at(0), digest(20), 1);
    let probe = Server::with(CI_ONLY);
    probe.feed([first.clone()]).await;
    let (bytes, _) = probe.recorder.bytes();
    probe.stop().await;
    let server = Server::start(Policy::from_json(CI_ONLY).expect("a policy"), bytes);
    server.feed([first]).await;
    let pages = ["/v1/invocations/leak", "/v1/invocations/leak/metrics"];
    let mut before = Vec::new();
    for page in pages {
        let answer = server.get(page).await;
        assert_eq!(answer.code, 200, "{page}");
        before.push(answer.json);
    }
    before[0]["complete"] = json!(false);
    before[0]["rows_capped"] = json!(true);
    server
        .feed([
            call("leak", "", "secret", at(1), CallKind::Uploaded(987_654_321)),
            queued("leak", "secret", at(2), digest(21), 3),
        ])
        .await;
    for (page, was) in pages.into_iter().zip(before) {
        let answer = server.get(page).await;
        assert_eq!(answer.code, 200, "{page}");
        assert_eq!(
            answer.json, was,
            "{page}: a call under secret reached the page"
        );
    }
    server.stop().await;
}

/// An authorizer that answers no instance name.
struct Mute;

impl Authorizer for Mute {
    fn authorize<'a>(
        &'a self,
        _metadata: &'a AuthenticationMetadata,
        _instance_names: &'a [&'a str],
    ) -> BoxFuture<'a, Vec<Result<(), Status>>> {
        Box::pin(std::future::ready(Vec::new()))
    }
}

/// Catches: a page served when the authorizer gives no answer for a name (it is
/// refused, `500`, as a REAPI call is), and an unauthenticated caller let in (`401`).
#[tokio::test]
async fn a_name_without_an_answer_is_refused_and_so_is_an_unknown_caller() {
    let mute = Policy {
        authenticator: Arc::new(AllowAuthenticator::default()),
        authorizers: Authorizers::allow_all(),
        invocations_get: Some(Arc::new(Mute)),
        blobs_get: Some(Arc::new(AllowAuthorizer)),
    };
    let server = Server::start(mute, kbf_front::DEFAULT_INVOCATION_MEMORY);
    server
        .feed([queued("b", "main", at(0), digest(1), 1)])
        .await;
    let answer = server.get("/v1/invocations/b").await;
    assert_eq!(answer.code, 500, "{}", answer.json);
    let why = answer.json["error"].as_str().unwrap_or_default();
    assert!(why.contains("no answer"), "{why}");
    server.stop().await;
    let server = Server::with(
        r#"{"authenticationPolicy": {"deny": "who are you"},
            "invocations": {"getAuthorizer": {"allow": {}}}}"#,
    );
    server
        .feed([queued("b", "main", at(0), digest(1), 1)])
        .await;
    let answer = server.get("/v1/invocations/b").await;
    assert_eq!(answer.code, 401, "{}", answer.json);
    server.stop().await;
}

/// Catches (mutants "skip the id check": any text taken as a key; and "percent-decode
/// the path": `%6Fk` read as `ok`): a record served for an `{id}` that is not, as the
/// bytes of the path, an id of section 2.2 or `<id>~<n>` (`<`, `%3C`, a `/` that makes
/// another route). The records `a<b`, `a%3Cb`, `.` and `-x` are fed directly (the
/// metadata layer never makes one), so a route that accepted the text would find
/// them. And a collision's own key, `<id>~2`, answering its record; `~1`, `~02` and
/// `~x` not.
#[tokio::test]
async fn an_id_outside_the_accepted_forms_is_404() {
    let server = Server::with(OPEN);
    let a = digest(1);
    server
        .feed([
            queued("a<b", "main", at(0), a, 1),
            queued("a%3Cb", "main", at(1), a, 2),
            queued(".", "main", at(2), a, 3),
            queued("-x", "main", at(3), a, 4),
            queued("ok", "main", at(4), a, 5),
            call("ok", "someone else", "main", at(5), CallKind::Uploaded(7)),
        ])
        .await;
    for path in [
        "/v1/invocations/a%3Cb",
        "/v1/invocations/a%3cb",
        "/v1/invocations/a%3Cb/actions",
        "/v1/invocations/.",
        "/v1/invocations/-x",
        "/v1/invocations/ok/../ok",
        "/v1/invocations/ok/actions/x/y",
        "/v1/invocations/ok/other",
        "/v1/invocations/ok~1",
        "/v1/invocations/ok~02",
        "/v1/invocations/ok~x",
        "/v1/invocations/ok~3",
        "/v1/invocations/%6Fk",
    ] {
        let answer = server.get(path).await;
        assert_eq!(answer.code, 404, "{path}: {}", answer.json);
    }
    // A raw `<` is not a URI character: the HTTP server refuses the request line
    // before any route reads it.
    let raw = server.get("/v1/invocations/a<b").await;
    assert_eq!(
        (raw.code, raw.json.clone()),
        (400, Value::Null),
        "{}",
        raw.head
    );
    let owner = server.ok("/v1/invocations/ok").await;
    assert_eq!(owner["collisions"], json!(["ok~2"]));
    let other = server.ok("/v1/invocations/ok~2").await;
    assert_eq!(other["id"], "ok~2");
    assert_eq!(other["counts"]["bytes_uploaded"], 7);
    assert_eq!(other["links"]["actions"], "/v1/invocations/ok~2/actions");
    server.stop().await;
}

/// Catches: a header field or counter of section 3.1 missing or misnamed, a time not
/// in RFC 3339 UTC, the state read wrong (open while its operation runs), links that
/// do not answer, and `url` other than `null` (no `--invocation-url-template` yet).
#[tokio::test]
async fn the_invocation_answers_its_header_counts_and_links() {
    let server = Server::with(OPEN);
    let (a, b) = (digest(1), digest(2));
    server
        .feed([
            queued("build", "main", at(1_500_000), a, 1),
            hit(
                "build",
                at(2_000_000),
                b,
                listing(digest(3), 0, &[], (None, None)),
            ),
            call(
                "build",
                "",
                "main",
                at(2_000_001),
                CallKind::GetActionResult {
                    action: digest(9),
                    hit: None,
                },
            ),
        ])
        .await;
    let inv = server.ok("/v1/invocations/build").await;
    assert_eq!(inv["id"], "build");
    assert_eq!(inv["url"], Value::Null);
    assert_eq!(
        (&inv["tool"], &inv["tool_version"]),
        (&json!("buck2"), &json!("0.1.0"))
    );
    assert_eq!(inv["instances"], json!(["main"]));
    assert_eq!(inv["first_seen"], "2026-10-11T09:00:01.500000Z");
    assert_eq!(inv["last_seen"], "2026-10-11T09:00:02.000001Z");
    assert_eq!(inv["state"], "open");
    assert_eq!(
        (&inv["complete"], &inv["rows_capped"]),
        (&json!(true), &json!(false))
    );
    assert_eq!(inv["actions"], 2);
    let counts = &inv["counts"];
    assert_eq!((&counts["execute"], &counts["ran"]), (&json!(1), &json!(1)));
    assert_eq!(
        (&counts["ac_hit"], &counts["ac_miss"]),
        (&json!(1), &json!(1))
    );
    for link in ["actions", "artifacts", "logs", "metrics"] {
        let href = inv["links"][link].as_str().expect("a link");
        assert_eq!(href, format!("/v1/invocations/build/{link}"));
        server.ok(href).await;
    }
    server.stop().await;
}

/// Catches: an action's answer that drops or misreads what its row holds: the
/// attempts in order with how each ended (a requeue's reason, the accepted result),
/// the outcome with its exit code, the outputs and logs from the row's listing with
/// whether each blob is held, the usage, the link to the Action blob; and an action
/// the invocation has no row for, or a malformed digest, answered anything but `404`.
#[tokio::test]
async fn an_action_answers_its_row_with_attempts_outputs_logs_and_usage() {
    let server = Server::with(OPEN);
    let out = server.store(b"lib.a bytes").await;
    let stderr = server.store(b"warning: unused\n").await;
    let action = digest(30);
    // The empty blob is held by every cache, though never stored.
    let empty = kbf_segments::sha256(&[]);
    let mut l = listing(
        digest(31),
        1,
        &[out, digest(32), empty],
        (None, Some(stderr)),
    );
    l.usage = Some(usage(1_200_000, 512_000_000));
    server
        .feed([
            queued("b", "main", at(0), action, 7),
            granted(7, 1, "node-a", at(10)),
            started(7, 1, at(20)),
            requeued(7, 1, RequeueReason::Silent, at(30)),
            granted(7, 2, "node-b", at(40)),
            started(7, 2, at(50)),
            finished(7, 2, Outcome::Ok { exit_code: 1 }, Some(l), at(60)),
        ])
        .await;
    let path = format!("/v1/invocations/b/actions/{}", text(&action));
    let row = server.ok(&path).await;
    assert_eq!(row["action"], text(&action));
    assert_eq!(
        (&row["how"], &row["operation"]),
        (&json!("ran"), &json!("operations/1-7"))
    );
    let attempts = row["attempts"].as_array().expect("attempts");
    assert_eq!(attempts.len(), 2);
    assert_eq!(attempts[0]["worker"], "node-a");
    assert_eq!(attempts[0]["lease"], "1.1");
    assert_eq!(attempts[0]["end"]["requeue"], "silent");
    assert_eq!(attempts[0]["granted"], "2026-10-11T09:00:00.000010Z");
    assert_eq!(attempts[1]["end"], json!({"result": "ok"}));
    assert_eq!(row["outcome"], json!({"ok": {"exit_code": 1}}));
    assert_eq!(row["result"], text(&digest(31)));
    let outputs = row["outputs"].as_array().expect("outputs");
    assert_eq!(outputs[0]["path"], "out/0.o");
    assert_eq!(outputs[0]["digest"], text(&out));
    assert_eq!(
        (&outputs[0]["available"], &outputs[1]["available"]),
        (&json!(true), &json!(false))
    );
    assert_eq!(outputs[2]["available"], true, "the empty blob");
    assert_eq!(row["stdout"], Value::Null);
    assert_eq!(row["stderr"]["available"], true);
    assert_eq!(row["usage"]["cpu_user_micros"], 1_200_000);
    assert_eq!(row["usage"]["peak_memory_bytes"], 512_000_000);
    assert_eq!(row["outputs_truncated"], false);
    let action_href = row["blobs"]["action"].as_str().expect("an href");
    assert!(action_href.starts_with(&format!("/v1/blobs/{}?instance=main", text(&action))));
    assert_eq!(
        row["blobs"]["command"],
        Value::Null,
        "not read from the Action yet"
    );
    let command = digest(33);
    server
        .feed([kbf_invocation::Event::Command {
            id: kbf_invocation::InvocationId::new("b"),
            owner: kbf_invocation::Owner::default(),
            action,
            command,
            shape: None,
        }])
        .await;
    let row = server.ok(&path).await;
    assert_eq!(row["command"], text(&command));
    let command_href = row["blobs"]["command"].as_str().expect("an href");
    assert_eq!(
        command_href,
        format!("/v1/blobs/{}?instance=main", text(&command))
    );
    // A lease still held, one that failed, a timeout and a refusal; a target sent.
    server
        .feed([
            queued_target("b", "//pkg:held", at(100), digest(40), 8),
            granted(8, 1, "node-c", at(110)),
            queued("b", "main", at(120), digest(41), 9),
            granted(9, 1, "node-d", at(130)),
            ended(
                9,
                Some((1, LeaseEnd::Failed(Failure::Infra, "disk full".to_owned()))),
                Outcome::Failed {
                    code: 14,
                    message: "the farm failed the action".to_owned(),
                },
                at(140),
            ),
            queued("b", "main", at(150), digest(42), 10),
            ended(10, None, Outcome::TimedOut, at(160)),
            queued("b", "main", at(170), digest(43), 11),
            ended(
                11,
                None,
                Outcome::Refused("no worker has the platform".to_owned()),
                at(180),
            ),
        ])
        .await;
    let rows = server.ok("/v1/invocations/b/actions").await;
    let rows = rows["actions"].as_array().expect("rows");
    assert_eq!(rows[0]["href"], path);
    assert!(rows[0].get("outputs").is_none(), "a page lists rows only");
    assert_eq!(
        rows[1]["target"],
        json!({"value": "//pkg:held", "truncated": false})
    );
    assert_eq!(rows[1]["mnemonic"], Value::Null);
    assert_eq!(rows[1]["attempts"][0]["end"], Value::Null, "held");
    assert_eq!(rows[1]["attempts"][0]["ended"], Value::Null);
    assert_eq!(rows[1]["outcome"], Value::Null, "in flight");
    assert_eq!(
        rows[2]["attempts"][0]["end"],
        json!({"failure": "infra", "reason": "disk full"})
    );
    assert_eq!(
        rows[2]["outcome"],
        json!({"failed": {"code": 14, "message": "the farm failed the action"}})
    );
    assert_eq!(rows[3]["outcome"], json!({"timed_out": {}}));
    assert_eq!(
        rows[4]["outcome"],
        json!({"refused": {"reason": "no worker has the platform"}})
    );
    for path in [
        format!("/v1/invocations/b/actions/{}", text(&digest(99))),
        "/v1/invocations/b/actions/not-a-digest".to_owned(),
        "/v1/invocations/nobody".to_owned(),
    ] {
        assert_eq!(server.get(&path).await.code, 404, "{path}");
    }
    server.stop().await;
}

/// Catches: `/logs` not listing failed actions first (row order kept within each
/// part), listing an action with no logs, or one twice; `/artifacts` not listing every
/// output of every row in order; `/actions?how=` and `?outcome=` keeping the wrong
/// rows.
#[tokio::test]
async fn logs_list_failed_actions_first_and_filters_keep_their_rows() {
    let server = Server::with(OPEN);
    let log = |n| (Some(digest(n)), None);
    server
        .feed([
            hit(
                "b",
                at(0),
                digest(1),
                listing(digest(100), 0, &[digest(50)], log(60)),
            ),
            hit("b", at(1), digest(2), listing(digest(101), 2, &[], log(61))),
            cached(
                "b",
                at(2),
                digest(3),
                listing(digest(102), 0, &[digest(51), digest(52)], (None, None)),
            ),
            queued("b", "main", at(3), digest(4), 1),
            finished(
                1,
                1,
                Outcome::Failed {
                    code: 13,
                    message: "no".to_owned(),
                },
                Some(listing(digest(103), 0, &[], log(63))),
                at(4),
            ),
            hit("b", at(5), digest(5), listing(digest(104), 0, &[], log(64))),
        ])
        .await;
    let logs = server.ok("/v1/invocations/b/logs").await;
    let order: Vec<Value> = logs["logs"]
        .as_array()
        .expect("logs")
        .iter()
        .map(|l| l["action"].clone())
        .collect();
    let want: Vec<Value> = [2, 4, 1, 5]
        .iter()
        .map(|&n| json!(text(&digest(n))))
        .collect();
    assert_eq!(order, want);
    assert_eq!(logs["logs"][0]["failed"], true);
    assert_eq!(logs["logs"][2]["failed"], false);
    assert!(logs.get("next_page_token").is_none());
    let artifacts = server.ok("/v1/invocations/b/artifacts").await;
    let paths: Vec<(Value, Value)> = artifacts["artifacts"]
        .as_array()
        .expect("artifacts")
        .iter()
        .map(|a| (a["action"].clone(), a["path"].clone()))
        .collect();
    let a = |n| json!(text(&digest(n)));
    assert_eq!(
        paths,
        [
            (a(1), json!("out/0.o")),
            (a(3), json!("out/0.o")),
            (a(3), json!("out/1.o"))
        ]
    );
    for (query, rows) in [
        ("how=cache_hit", vec![1, 2, 5]),
        ("how=cache_hit_execute", vec![3]),
        ("how=ran", vec![4]),
        ("outcome=ok", vec![1, 2, 3, 5]),
        ("outcome=failed", vec![4]),
        ("outcome=ok&how=ran", vec![]),
        ("outcome=timed_out", vec![]),
    ] {
        let page = server
            .ok(&format!("/v1/invocations/b/actions?{query}"))
            .await;
        let got: Vec<Value> = page["actions"]
            .as_array()
            .expect("rows")
            .iter()
            .map(|r| r["action"].clone())
            .collect();
        let want: Vec<Value> = rows.iter().map(|&n| a(n)).collect();
        assert_eq!(got, want, "{query}");
    }
    server.stop().await;
}

/// Catches: a query the route does not take served anyway (unknown or repeated
/// keys, a filter on `/logs`, any parameter on the invocation or metrics), a page size
/// outside 1 to 1000, a filter value outside its names, and a token that does not
/// decode: each `400`.
#[tokio::test]
async fn a_query_a_route_does_not_take_is_400() {
    let server = Server::with(OPEN);
    server
        .feed([queued("b", "main", at(0), digest(1), 1)])
        .await;
    for query in [
        "/actions?page_size=0",
        "/actions?page_size=1001",
        "/actions?page_size=x",
        "/actions?outcome=broken",
        "/actions?how=hit",
        "/actions?page_size=2&page_size=3",
        "/actions?sort=time",
        "/actions?page_token=zz",
        "/actions?page_token=3132",
        // `1:0:actions:-+-:99:0` and `1:0:logs:-+-:0:9`: positions past the one row.
        "/actions?page_token=313a303a616374696f6e733a2d2b2d3a39393a30",
        "/logs?page_token=313a303a6c6f67733a2d2b2d3a303a39",
        "/logs?outcome=ok",
        "/artifacts?how=ran",
        "/metrics?page_size=2",
        "/metrics?page_token=00",
        "?page_size=2",
    ] {
        let answer = server.get(&format!("/v1/invocations/b{query}")).await;
        assert_eq!(answer.code, 400, "{query}: {}", answer.json);
    }
    let page = server.ok("/v1/invocations/b/actions?page_size=1000").await;
    assert_eq!(page["actions"].as_array().map(Vec::len), Some(1));
    server.stop().await;
}

/// Catches: the aggregates of section 4.4 wrong: queue time (queued to granted, each
/// lease of a `ran` row; a requeue queues again) and run time (granted to ended) as
/// count, min, max and sum exactly and p50 and p90 within 1/16 above; reruns not
/// counted by reason; usage summed and maxed over the rows that carry it, with
/// `usage_actions`; usage `null`, not zeros, when no row carries it; a joined row's
/// leases counted again.
#[tokio::test]
async fn metrics_aggregate_queue_and_run_times_reruns_and_usage() {
    let server = Server::with(OPEN);
    let mut events = Vec::new();
    for i in 0..10u64 {
        // Queued at 0, granted after (i + 1) ms, run for (i + 1) * 10 ms.
        let (q, r) = ((i + 1) * 1_000, (i + 1) * 10_000);
        let mut l = listing(digest(200 + i), 0, &[], (None, None));
        l.usage = (i < 2).then(|| usage(100 * (i + 1), 1_000 * (i + 1)));
        events.extend([
            queued("m", "main", at(0), digest(i), i),
            granted(i, 1, "n", at(q)),
            finished(i, 1, Outcome::Ok { exit_code: 0 }, Some(l), at(q + r)),
        ]);
    }
    events.extend([
        queued("m", "main", at(0), digest(50), 50),
        granted(50, 1, "n", at(500)),
        requeued(50, 1, RequeueReason::Silent, at(600)),
        granted(50, 2, "n", at(700)),
    ]);
    server.feed(events).await;
    let m = server.ok("/v1/invocations/m/metrics").await;
    let queue = &m["queue_micros"];
    // Ten leases of 1..10 ms, then 500 µs and (queued again at 600) 100 µs.
    assert_eq!(queue["count"], 12);
    assert_eq!(
        (&queue["min"], &queue["max"]),
        (&json!(100), &json!(10_000))
    );
    assert_eq!(queue["sum"], 55_000 + 600);
    let within = |got: &Value, exact: u64| {
        let got = got.as_u64().expect("a number");
        got >= exact && got - exact <= exact / 16
    };
    // Sorted: 100, 500, 1000, ..., 10000; rank 6 of 12 is 4000, rank 11 is 9000.
    assert!(within(&queue["p50"], 4_000), "{queue}");
    assert!(within(&queue["p90"], 9_000), "{queue}");
    let run = &m["run_micros"];
    assert_eq!(
        run["count"], 11,
        "the requeued lease ran 100 µs; the held one has not ended"
    );
    assert_eq!((&run["min"], &run["max"]), (&json!(100), &json!(100_000)));
    assert_eq!(m["reruns"], json!({"silent": 1}));
    assert_eq!(m["usage_actions"], 2);
    assert_eq!(
        m["cpu_micros"],
        json!({"sum": 100 + 1 + 200 + 1, "max": 201})
    );
    assert_eq!(m["peak_memory_bytes"], json!({"sum": 3_000, "max": 2_000}));
    assert_eq!(m["counts"]["execute"], 11);
    server
        .feed([
            queued("j", "main", at(0), digest(1), 60),
            granted(60, 1, "n", at(10)),
            call(
                "j2",
                "",
                "main",
                at(5),
                CallKind::Execute {
                    action: digest(1),
                    answer: kbf_invocation::ExecuteAnswer::Joined {
                        name: "operations/1-61".to_owned(),
                        operation: kbf_types::OperationId(60),
                    },
                },
            ),
        ])
        .await;
    let joined = server.ok("/v1/invocations/j2/metrics").await;
    assert_eq!(joined["queue_micros"]["count"], 0, "{joined}");
    assert_eq!(joined["queue_micros"]["p50"], Value::Null);
    assert_eq!(
        (&joined["usage_actions"], &joined["cpu_micros"]),
        (&json!(0), &Value::Null)
    );
    assert_eq!(joined["peak_memory_bytes"], Value::Null);
    server.stop().await;
}

/// Catches: a server that keeps history answering a record held nowhere (not by the
/// recorder, not in history) with anything but the `no_record` of one that does not:
/// history is asked after the recorder and answers the same.
#[tokio::test]
async fn a_record_held_nowhere_is_no_record_when_history_is_on() {
    use kbf_history::{HistoryRoot, IndexRoot};
    use kbf_objstore::{Capabilities, KeyPrefix, MemoryStore};
    use kbf_server::history::{Options, over};
    use kbf_server::{EpochPolicy, StateStore};
    use kbf_state::fake::{Fake, Flaw};
    use kbf_state::{ManualClock, Store};

    let store: Arc<dyn Store> = Arc::new(Fake::with_flaw(
        Arc::new(ManualClock::default()),
        Flaw::None,
    ));
    let root = HistoryRoot::under(&KeyPrefix::new("kbf/").expect("a prefix")).expect("a root");
    let history = over(
        MemoryStore::new(Capabilities::default()),
        root,
        Arc::clone(&store),
        IndexRoot::default(),
        Options::default(),
    );
    let state = StateStore {
        store,
        epoch: EpochPolicy::Keep,
        members: Default::default(),
        history: Some(history),
    };
    let policy = Policy::from_json(PAGES_ONLY).expect("a policy");
    let server = Server::start_over(policy, kbf_front::DEFAULT_INVOCATION_MEMORY, Some(state));
    let missing = server.get("/v1/invocations/nowhere").await;
    assert_eq!(missing.code, 404, "{}", missing.json);
    assert_eq!(missing.json["error"], "no_record");
    assert_eq!(missing.json["id"], "nowhere");
    server.stop().await;
}
