//! Apple's catalogue: the parser and the at-most-daily reader, with an injected fetcher
//! (no network). The documents are made up in the catalogue's shape.

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use kbf_mdm_api::catalogue::{
    CatalogueEntry, CatalogueError, CatalogueFetcher, DailyCatalogue, FetchFuture,
    REFRESH_INTERVAL_MS, RETRY_AFTER_FAILURE_MS, Refresh, parse_pmv,
};
use kbf_mdm_api::names::{Date, OsVersion};

const DOC: &str = r#"{
  "PublicAssetSets": {
    "iOS": [{"ProductVersion": "27.0", "Build": "24A1", "PostingDate": "2026-09-01",
             "ExpirationDate": "2026-12-01", "SupportedDevices": ["iPhone1,1"]}],
    "macOS": [
      {"ProductVersion": "27.0.1", "Build": "26A434", "PostingDate": "2026-09-28",
       "ExpirationDate": "2027-01-06", "SupportedDevices": ["J180dAP", "J274AP"]},
      {"ProductVersion": "26.7.1", "Build": "25G241", "PostingDate": "2026-09-28",
       "ExpirationDate": "2027-01-06"}
    ]
  },
  "AssetSets": {
    "macOS": [
      {"ProductVersion": "27.0", "Build": "26A400", "PostingDate": "2026-09-15",
       "ExpirationDate": "2026-10-25", "SupportedDevices": []}
    ]
  }
}"#;

fn entry(
    version: &str,
    build: &str,
    posted: &str,
    expires: &str,
    devices: &[&str],
    public: bool,
) -> CatalogueEntry {
    CatalogueEntry {
        product_version: OsVersion::parse(version).unwrap(),
        build: build.to_owned(),
        posting_date: Date::parse(posted).unwrap(),
        expiration_date: Date::parse(expires).unwrap(),
        supported_devices: devices.iter().map(|d| (*d).to_owned()).collect(),
        public,
    }
}

/// Catches: reading another platform's list, dropping `AssetSets` (a build a managed
/// Mac may still be held at), losing the public/managed mark, or misreading a date.
#[test]
fn parses_both_macos_lists_with_their_dates() {
    let entries = parse_pmv(DOC.as_bytes()).unwrap();
    assert_eq!(
        entries,
        vec![
            entry(
                "27.0.1",
                "26A434",
                "2026-09-28",
                "2027-01-06",
                &["J180dAP", "J274AP"],
                true
            ),
            entry("26.7.1", "25G241", "2026-09-28", "2027-01-06", &[], true),
            entry("27.0", "26A400", "2026-09-15", "2026-10-25", &[], false),
        ]
    );
}

/// Catches: a document without `AssetSets` refused (it is optional).
#[test]
fn asset_sets_are_optional() {
    let doc = r#"{"PublicAssetSets": {"macOS": []}}"#;
    assert_eq!(parse_pmv(doc.as_bytes()), Ok(vec![]));
}

/// Catches: a malformed document or entry accepted, so a build goes missing or an
/// expiry is misread, instead of the whole document being refused.
#[test]
fn a_malformed_document_is_refused_whole() {
    assert!(matches!(
        parse_pmv(b"not json"),
        Err(CatalogueError::Json(_))
    ));
    for doc in [
        r#"{}"#,
        r#"{"PublicAssetSets": {}}"#,
        r#"{"PublicAssetSets": {"macOS": {}}}"#,
    ] {
        assert_eq!(
            parse_pmv(doc.as_bytes()),
            Err(CatalogueError::NotAList("PublicAssetSets")),
            "{doc}"
        );
    }
    let doc = r#"{"PublicAssetSets": {"macOS": []}, "AssetSets": {"macOS": 1}}"#;
    assert_eq!(
        parse_pmv(doc.as_bytes()),
        Err(CatalogueError::NotAList("AssetSets"))
    );

    let good = r#""ProductVersion": "27.0", "Build": "26A1", "PostingDate": "2026-09-01", "ExpirationDate": "2026-12-01""#;
    let cases = [
        (
            r#""Build": "26A1", "PostingDate": "2026-09-01", "ExpirationDate": "2026-12-01""#,
            "no string ProductVersion",
        ),
        (
            r#""ProductVersion": 27, "Build": "26A1", "PostingDate": "2026-09-01", "ExpirationDate": "2026-12-01""#,
            "no string ProductVersion",
        ),
        (
            r#""ProductVersion": "27.x", "Build": "26A1", "PostingDate": "2026-09-01", "ExpirationDate": "2026-12-01""#,
            "is not a macOS version",
        ),
        (
            r#""ProductVersion": "27.0", "PostingDate": "2026-09-01", "ExpirationDate": "2026-12-01""#,
            "no string Build",
        ),
        (
            r#""ProductVersion": "27.0", "Build": "", "PostingDate": "2026-09-01", "ExpirationDate": "2026-12-01""#,
            "is not ASCII letters",
        ),
        (
            r#""ProductVersion": "27.0", "Build": "26A1/", "PostingDate": "2026-09-01", "ExpirationDate": "2026-12-01""#,
            "is not ASCII letters",
        ),
        (
            r#""ProductVersion": "27.0", "Build": "26A1", "ExpirationDate": "2026-12-01""#,
            "no string PostingDate",
        ),
        (
            r#""ProductVersion": "27.0", "Build": "26A1", "PostingDate": "2026-9-01", "ExpirationDate": "2026-12-01""#,
            "is not a date",
        ),
        (
            r#""ProductVersion": "27.0", "Build": "26A1", "PostingDate": "2026-09-01""#,
            "no string ExpirationDate",
        ),
        (
            r#""ProductVersion": "27.0", "Build": "26A1", "PostingDate": "2026-09-01", "ExpirationDate": "2026-02-30""#,
            "is not a date",
        ),
    ];
    let devices = [
        (
            format!(r#"{good}, "SupportedDevices": "J274AP""#),
            "is not a list",
        ),
        (
            format!(r#"{good}, "SupportedDevices": ["J274AP", 1]"#),
            "holds a non-string",
        ),
    ];
    let all = cases
        .iter()
        .map(|(e, want)| ((*e).to_owned(), *want))
        .chain(devices.iter().map(|(e, want)| (e.clone(), *want)));
    for (fields, want) in all {
        // The bad entry is second in the managed list: its position and set are named.
        let doc = format!(
            r#"{{"PublicAssetSets": {{"macOS": [{{{good}}}]}}, "AssetSets": {{"macOS": [{{{good}}}, {{{fields}}}]}}}}"#
        );
        match parse_pmv(doc.as_bytes()) {
            Err(CatalogueError::Entry {
                set,
                index,
                problem,
            }) => {
                assert_eq!((set, index), ("AssetSets", 1), "{fields}");
                assert!(problem.contains(want), "{problem} lacks {want}");
            }
            other => panic!("{fields}: {other:?}"),
        }
    }
    // The error names the set and entry for the operator.
    let e = CatalogueError::Entry {
        set: "AssetSets",
        index: 1,
        problem: "p".to_owned(),
    };
    assert_eq!(e.to_string(), "macOS entry 1 of AssetSets: p");
    assert_eq!(
        CatalogueError::NotAList("AssetSets").to_string(),
        "the catalogue's AssetSets.macOS is missing or not a list"
    );
}

/// A fetcher that returns queued answers and counts its calls.
struct Fake {
    answers: Mutex<Vec<Result<Vec<u8>, String>>>,
    calls: AtomicUsize,
}

impl Fake {
    fn new(answers: Vec<Result<&str, &str>>) -> Self {
        let mut answers: Vec<_> = answers
            .into_iter()
            .map(|a| a.map(|d| d.as_bytes().to_vec()).map_err(str::to_owned))
            .collect();
        answers.reverse();
        Self {
            answers: Mutex::new(answers),
            calls: AtomicUsize::new(0),
        }
    }
}

impl CatalogueFetcher for &Fake {
    fn fetch(&self) -> FetchFuture<'_> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let answer = self
            .answers
            .lock()
            .unwrap()
            .pop()
            .expect("an answer is queued");
        Box::pin(async move { answer })
    }
}

/// Catches: reading more than once a day (Apple asks for at most once), never
/// re-reading, a failed read that drops the previous catalogue, and a failure that
/// blocks the next try for a whole day.
#[tokio::test]
async fn reads_at_most_once_a_day_and_keeps_the_last_good_catalogue() {
    let empty = r#"{"PublicAssetSets": {"macOS": []}}"#;
    let fake = Fake::new(vec![Ok(DOC), Err("offline"), Ok("garbage"), Ok(empty)]);
    let mut cat = DailyCatalogue::new(&fake);
    assert_eq!(cat.current(), None);

    let t0 = 1_000;
    assert_eq!(cat.refresh(t0).await, Refresh::Updated);
    let first = cat.current().unwrap().clone();
    assert_eq!((first.fetched_at_unix_ms, first.entries.len()), (t0, 3));

    // Not again within the day.
    assert_eq!(
        cat.refresh(t0 + REFRESH_INTERVAL_MS - 1).await,
        Refresh::NotDue
    );
    assert_eq!(fake.calls.load(Ordering::SeqCst), 1);

    // A day later it reads again; a failure keeps the old catalogue.
    let t1 = t0 + REFRESH_INTERVAL_MS;
    assert_eq!(
        cat.refresh(t1).await,
        Refresh::Failed("fetch failed: offline".to_owned())
    );
    assert_eq!(cat.current(), Some(&first));

    // After a failure it retries within the hour, not the day.
    assert_eq!(
        cat.refresh(t1 + RETRY_AFTER_FAILURE_MS - 1).await,
        Refresh::NotDue
    );
    let t2 = t1 + RETRY_AFTER_FAILURE_MS;
    match cat.refresh(t2).await {
        Refresh::Failed(e) => assert!(e.contains("not JSON"), "{e}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(cat.current(), Some(&first));

    let t3 = t2 + RETRY_AFTER_FAILURE_MS;
    assert_eq!(cat.refresh(t3).await, Refresh::Updated);
    assert_eq!(cat.current().unwrap().fetched_at_unix_ms, t3);
    assert!(cat.current().unwrap().entries.is_empty());
    assert_eq!(fake.calls.load(Ordering::SeqCst), 4);
}
