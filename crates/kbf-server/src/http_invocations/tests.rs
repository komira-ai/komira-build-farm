use std::collections::BTreeSet;

use kbf_invocation::{
    Call, CallKind, Event, InvocationId, Listing, Metadata, Owner, RecordKey, Records, WallTime,
};
use kbf_types::{Digest, DigestFunction};

use super::pages::{
    Bad, Cursor, Request, Token, Walk, answer, instance_names, reader_names, still_authorized,
};
use super::views::{Draft, digest_text, rfc3339};

fn d(n: u8) -> Digest {
    Digest::new(DigestFunction::Sha256, [n; 32], u64::from(n))
}

/// Records holding invocation `id` with one GetActionResult hit row per instance in
/// `instances`, and its record.
fn records(id: &str, instances: &[&str]) -> Records {
    let mut records = Records::new();
    for (i, instance) in instances.iter().enumerate() {
        let n = u8::try_from(i).expect("few");
        records.apply(Event::Call(Box::new(Call {
            id: InvocationId::new(id),
            minted: false,
            owner: Owner::default(),
            instance: (*instance).to_owned(),
            at: WallTime::from_unix_micros(u64::from(n)),
            metadata: Metadata::default(),
            kind: CallKind::GetActionResult {
                action: d(n),
                hit: Some(Listing {
                    result: Some(d(100 + n)),
                    exit_code: Some(0),
                    outputs: Vec::new(),
                    stdout: Some(d(200 + n)),
                    stderr: None,
                    usage: None,
                }),
            },
        })));
    }
    records
}

fn key(id: &str) -> RecordKey {
    RecordKey::owner(InvocationId::new(id))
}

/// Catches: a wall time written in another zone, without its microseconds, or with
/// the calendar wrong (leap days, the turn of a century, the epoch).
#[test]
fn times_are_rfc_3339_utc_to_the_microsecond() {
    let s = |secs: u64, micros: u64| rfc3339(WallTime::from_unix_micros(secs * 1_000_000 + micros));
    assert_eq!(s(0, 0), "1970-01-01T00:00:00.000000Z");
    assert_eq!(s(951_868_799, 999_999), "2000-02-29T23:59:59.999999Z");
    assert_eq!(s(1_791_709_200, 1), "2026-10-11T09:00:00.000001Z");
    assert_eq!(s(4_107_542_400, 0), "2100-03-01T00:00:00.000000Z");
}

/// Catches: a key other than an accepted id or `<id>~<n>` (n a decimal from 2, no
/// sign, no leading zero) read as a record's key, and those two forms refused.
#[test]
fn a_key_is_an_id_or_an_id_tilde_n() {
    let parse = |path: &str| Request::parse(path, "").map(|r| r.key);
    assert_eq!(parse("/v1/invocations/b-1.x_Y"), Ok(key("b-1.x_Y")));
    assert_eq!(
        parse("/v1/invocations/b~12"),
        Ok(RecordKey::collision(InvocationId::new("b"), 12))
    );
    for bad in [
        "", "~2", "b~", "b~1", "b~0", "b~02", "b~+2", "b~-2", "b~2~3", ".b", "b%7E2", "b<",
    ] {
        let path = format!("/v1/invocations/{bad}");
        assert!(matches!(parse(&path), Err(Bad::NotFound(_))), "{bad:?}");
    }
    let long = format!("/v1/invocations/{}", "a".repeat(129));
    assert!(matches!(parse(&long), Err(Bad::NotFound(_))));
}

/// Catches: a token that does not decode back to its position and filter, one whose
/// text was cut or edited accepted, and one past the record's rows accepted.
#[test]
fn a_token_decodes_to_where_it_was_made_and_nothing_else_decodes() {
    let records = records("b", &["main", "main"]);
    let inv = records.get(&key("b")).expect("held");
    let page = |query: &str| Request::parse("/v1/invocations/b/logs", query);
    let first = answer(
        inv,
        &page("page_size=1").expect("a request"),
        (inv.serial(), &instance_names(inv)),
        true,
        WallTime::default(),
    )
    .expect("a page");
    let token = first.value["next_page_token"]
        .as_str()
        .expect("a token")
        .to_owned();
    let start = Cursor::Walk(Walk::Logs { row: 0, rest: None });
    let filter = Default::default();
    assert_eq!(
        Token::decode(&token, inv, start, filter),
        Ok(Cursor::Walk(Walk::Logs {
            row: 1,
            rest: Some(2)
        })),
        "no row failed: the first page went to the other rows at once"
    );
    for bad in [
        String::new(),
        token[..token.len() - 2].to_owned(),
        hex::encode("2:0:logs:-+-:0:0"),
        hex::encode("1:0:logs:-+-:3:0"),
        hex::encode("1:0:logs:-+-:0:4"),
        hex::encode("1:x:logs:-+-:0:0"),
        hex::encode("1:0:logs:-+-:0"),
        hex::encode("1:0:logs:-+-:a:0"),
        hex::encode([0xff, 0xfe]),
    ] {
        let decoded = Token::decode(&bad, inv, start, filter);
        assert!(
            matches!(decoded, Err(Bad::Invalid(_))),
            "{bad:?}: {decoded:?}"
        );
    }
}

/// Catches (mutant "skip the re-check": `still_authorized` always `Ok`): a page built
/// from a record that gained an instance name after the reader was allowed on its
/// names, or from another record made under the key since.
#[test]
fn a_record_that_changed_since_its_authorization_is_not_read() {
    let records = records("b", &["ci", "other"]);
    let inv = records.get(&key("b")).expect("held");
    let names = instance_names(inv);
    assert_eq!(names, ["ci", "other"]);
    assert_eq!(still_authorized(inv, inv.serial(), &names), Ok(()));
    let fewer = ["ci".to_owned()];
    assert!(matches!(
        still_authorized(inv, inv.serial(), &fewer),
        Err(Bad::Changed(_))
    ));
    assert!(matches!(
        still_authorized(inv, inv.serial() + 1, &names),
        Err(Bad::Gone)
    ));
    let request = Request::parse("/v1/invocations/b", "").expect("a request");
    let refused = answer(
        inv,
        &request,
        (inv.serial(), &fewer),
        false,
        WallTime::default(),
    );
    assert!(matches!(refused, Err(Bad::Changed(_))));
    assert_eq!(digest_text(&d(1)), format!("{}-1", "01".repeat(32)));
}

/// Catches (mutant "the record's instances only": `reader_names` without the rows'):
/// a row whose instance the record does not hold left out of the names a reader is
/// asked about, so a page of it is served on the record's names alone (the second
/// layer: the recorder never sets such a row, `kbf-invocation`'s `tests/bound.rs`).
/// The row is made here, as the recorder cannot. And a name listed twice, or out of
/// order (`still_authorized` compares lists), and the empty name for a record that
/// holds none.
#[test]
fn rows_instances_are_asked_about_too() {
    let records = records("b", &["ci", "ci/main"]);
    let inv = records.get(&key("b")).expect("held");
    let mut rows = inv.rows().to_vec();
    rows[0].instance = "secret".to_owned();
    assert_eq!(
        reader_names(inv.instances(), &rows),
        ["ci", "ci/main", "secret"]
    );
    assert_eq!(instance_names(inv), ["ci", "ci/main"]);
    assert_eq!(reader_names(&BTreeSet::new(), &rows[..1]), ["secret"]);
    assert_eq!(reader_names(&BTreeSet::new(), &[]), [""]);
}

/// Catches: a blob's `available` set at the wrong place in the answer, or left `null`
/// when the index answered; and an answer the index did not answer (fewer answers)
/// changed anywhere but there.
#[test]
fn availability_is_filled_where_each_blob_sits() {
    let records = records("b", &["main"]);
    let inv = records.get(&key("b")).expect("held");
    let request = Request::parse(
        &format!("/v1/invocations/b/actions/{}", digest_text(&d(0))),
        "",
    )
    .expect("a request");
    let draft: Draft = answer(
        inv,
        &request,
        (inv.serial(), &instance_names(inv)),
        true,
        WallTime::default(),
    )
    .expect("a row");
    assert_eq!(
        draft
            .blobs
            .iter()
            .map(|(at, _)| at.as_str())
            .collect::<Vec<_>>(),
        ["/stdout"]
    );
    let value = draft.fill(&[true]);
    assert_eq!(value["stdout"]["available"], true);
    assert_eq!(value["stdout"]["digest"], digest_text(&d(200)));
    let draft = answer(
        inv,
        &request,
        (inv.serial(), &instance_names(inv)),
        true,
        WallTime::default(),
    )
    .expect("a row");
    assert_eq!(
        draft.fill(&[])["stdout"]["available"],
        serde_json::Value::Null
    );
}

/// Catches: a refusal of the page module answered with another code than its own:
/// `404` for what is not there, `400` for a request that cannot be served, `503` for
/// a record that changed while it was read.
#[test]
fn refusals_keep_their_meaning() {
    use axum::http::StatusCode;
    for (bad, code) in [
        (Bad::Gone, StatusCode::NOT_FOUND),
        (Bad::NotFound("no row".to_owned()), StatusCode::NOT_FOUND),
        (Bad::Invalid("bad".to_owned()), StatusCode::BAD_REQUEST),
        (
            Bad::Changed("again".to_owned()),
            StatusCode::SERVICE_UNAVAILABLE,
        ),
    ] {
        let answer = super::bad_answer(&bad, "b", WallTime::from_unix_micros(0));
        assert_eq!(answer.status(), code, "{bad:?}");
    }
}
