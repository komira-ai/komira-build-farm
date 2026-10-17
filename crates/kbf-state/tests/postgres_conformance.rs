//! The Postgres backend against a real server: the suite C1 to C8, the defects the suite
//! must reject, the startup refusals, the claim path and the parts that are
//! Postgres's alone (the database clock, one transaction per batch).
//!
//! The tests that need a server are `#[ignore]`d so `cargo test --workspace` stays
//! hermetic; the `state-postgres` job in ci.yml runs them with `--include-ignored`
//! against a service container. `KBF_TEST_POSTGRES_URL` names a database they may create
//! and drop schemas in. Waits are on conditions; no assertion depends on elapsed time.
#![cfg(feature = "postgres")]

use std::collections::BTreeSet;
use std::thread;
use std::time::Duration;

use kbf_state::conformance::{
    c1_linearizable_cas, c2_atomic_txn, c3_monotonic_counter, c4_store_clock_expiry,
    c5_ordered_feed, c6_durable_ack, c7_group_commit, c8_bounded_range_scan,
};
use kbf_state::postgres::{ConfigError, Flaw, PostgresBackend, PostgresConfig, PostgresStore};
use kbf_state::{Cond, Event, Revision, Store, StoreError, StoreTime, Txn};
use postgres::{Client, NoTls};

const URL_VAR: &str = "KBF_TEST_POSTGRES_URL";

fn url() -> String {
    std::env::var(URL_VAR).unwrap_or_else(|_| {
        panic!("{URL_VAR} must name a postgres:// URL the tests may create schemas in")
    })
}

/// The test URL with a server setting for every connection it opens.
fn url_setting(name: &str, value: &str) -> String {
    let base = url();
    let sep = if base.contains('?') { '&' } else { '?' };
    format!("{base}{sep}options=-c%20{name}%3D{value}")
}

fn backend(flaw: Flaw) -> PostgresBackend {
    PostgresBackend::new(&url(), flaw)
}

// ---- the suite -------------------------------------------------------------------------

#[test]
#[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the state-postgres CI job runs it"]
fn c1_cas_is_linearizable() {
    c1_linearizable_cas(&backend(Flaw::None));
}

#[test]
#[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the state-postgres CI job runs it"]
fn c2_transactions_are_atomic() {
    c2_atomic_txn(&backend(Flaw::None));
}

#[test]
#[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the state-postgres CI job runs it"]
fn c3_counters_are_monotonic() {
    c3_monotonic_counter(&backend(Flaw::None));
}

#[test]
#[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the state-postgres CI job runs it"]
fn c4_expiry_follows_the_store_clock() {
    c4_store_clock_expiry(&backend(Flaw::None));
}

#[test]
#[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the state-postgres CI job runs it"]
fn c5_the_feed_skips_no_row() {
    c5_ordered_feed(&backend(Flaw::None));
}

#[test]
#[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the state-postgres CI job runs it"]
fn c6_acknowledged_writes_survive_a_restart() {
    c6_durable_ack(&backend(Flaw::None));
}

#[test]
#[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the state-postgres CI job runs it"]
fn c7_a_burst_is_one_group() {
    c7_group_commit(&backend(Flaw::None));
}

#[test]
#[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the state-postgres CI job runs it"]
fn c8_a_range_scan_is_bounded_and_resumes() {
    c8_bounded_range_scan(&backend(Flaw::None));
}

// Catches revisions taken from a sequence: the later one commits first, the poller moves
// past the earlier one, and never sees it. This is what makes the C5 run above a check
// of the commit head rather than a pass by construction.
#[test]
#[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the state-postgres CI job runs it"]
#[should_panic(expected = "C5 interleaved")]
fn sequence_revisions_fail_c5() {
    c5_ordered_feed(&backend(Flaw::SequenceRevisions));
}

// ---- Postgres's own parts ----------------------------------------------------------------

// Catches a connection that is allowed to acknowledge a commit before it is flushed.
#[test]
#[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the state-postgres CI job runs it"]
fn a_server_that_does_not_flush_commits_is_refused() {
    for setting in ["off", "local", "remote_write"] {
        let config = PostgresConfig::parse(&url_setting("synchronous_commit", setting))
            .expect("the URL parses");
        let Err(StoreError::Failed(why)) = PostgresStore::open(&config) else {
            panic!("synchronous_commit={setting} must refuse to open");
        };
        assert!(
            why.contains("synchronous_commit") && why.contains(setting),
            "the refusal names the setting and its value, said {why:?}"
        );
    }
}

// Catches expiry judged by anything but the database: with no injected clock a key with a
// zero time to live is expired the moment it is read, and one with an hour is not.
#[test]
#[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the state-postgres CI job runs it"]
fn the_database_clock_judges_expiry() {
    let backend = backend(Flaw::None);
    let store = PostgresStore::open(&backend.fresh_config()).expect("opens");
    store
        .txn(
            Txn::new()
                .put_ttl("d/zero", "z", Duration::ZERO, Cond::Any)
                .put_ttl("d/hour", "h", Duration::from_secs(3600), Cond::Any),
        )
        .expect("reachable")
        .expect("commits");
    assert!(
        store.get("d/zero").expect("reachable").is_none(),
        "a key with no time to live is expired at its first read"
    );
    let hour = store
        .get("d/hour")
        .expect("reachable")
        .expect("an hour has not passed");
    let expires = hour.expires_at.expect("a ttl key reports its expiry");
    assert!(
        expires > store.now(),
        "the expiry lies ahead of the database's now"
    );
    assert!(
        store.now().since(StoreTime::ZERO) > Duration::from_secs(1_700_000_000),
        "the database's now is the wall clock, not the origin"
    );
    assert_eq!(
        store.reap_expired().expect("reaps"),
        1,
        "only d/zero is due"
    );
    let rows = store.changes(Revision::ZERO, 100).expect("feed").rows;
    let expired: Vec<&str> = rows
        .iter()
        .filter(|row| row.event == Event::Expire)
        .map(|row| row.key.as_str())
        .collect();
    assert_eq!(expired, ["d/zero"]);
}

// Catches a batch that commits its transactions one by one: every row a batch writes
// carries the id of the one database transaction that wrote it.
#[test]
#[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the state-postgres CI job runs it"]
fn a_batch_is_one_database_transaction() {
    let backend = backend(Flaw::None);
    let config = backend.fresh_config();
    let store = PostgresStore::open(&config).expect("opens");
    let batch: Vec<Txn> = (0..300)
        .map(|i| Txn::new().put(format!("g/{i:03}"), "x", Cond::Any))
        .collect();
    let results = store.apply_batch(batch).expect("commits");
    assert!(results.iter().all(Result::is_ok));

    let mut raw = Client::connect(&url(), NoTls).expect("connects");
    let schema = config.schema();
    let kv: i64 = raw
        .query_one(
            &format!("SELECT count(DISTINCT xmin::text) FROM {schema}.kv"),
            &[],
        )
        .expect("reads xmin")
        .get(0);
    let feed: i64 = raw
        .query_one(
            &format!("SELECT count(DISTINCT xmin::text) FROM {schema}.feed"),
            &[],
        )
        .expect("reads xmin")
        .get(0);
    assert_eq!(
        (kv, feed),
        (1, 1),
        "300 transactions in one batch were written by more than one database transaction"
    );
}

// ---- the claim path --------------------------------------------------------------------

fn seed_queue(store: &PostgresStore, n: usize) {
    let seed: Vec<Txn> = (0..n)
        .map(|i| Txn::new().put(format!("q/{i:03}"), "job", Cond::Any))
        .collect();
    for result in store.apply_batch(seed).expect("seeds") {
        result.expect("a queue entry commits");
    }
}

/// The transaction that claims a queue entry: delete it on its version, write a lease.
fn take(key: &str, was: &kbf_state::Versioned) -> Option<Txn> {
    Some(Txn::new().delete(key, Cond::Version(was.version)).put(
        key.replacen("q/", "lease/", 1),
        "held",
        Cond::Absent,
    ))
}

// Catches a claim that hands one row to two claimers, or loses one.
#[test]
#[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the state-postgres CI job runs it"]
fn concurrent_claimers_get_disjoint_rows() {
    let backend = backend(Flaw::None);
    let store = PostgresStore::open(&backend.fresh_config()).expect("opens");
    seed_queue(&store, 40);

    let per_claimer: Vec<Vec<String>> = thread::scope(|scope| {
        let claimers: Vec<_> = (0..4)
            .map(|_| {
                let store = &store;
                scope.spawn(move || {
                    let mut mine = Vec::new();
                    loop {
                        let got = store.claim("q/", 3, take).expect("claims");
                        if got.is_empty() {
                            return mine;
                        }
                        mine.extend(got.into_iter().map(|claimed| claimed.key));
                    }
                })
            })
            .collect();
        claimers
            .into_iter()
            .map(|c| c.join().expect("a claimer did not panic"))
            .collect()
    });
    let all: Vec<&String> = per_claimer.iter().flatten().collect();
    let distinct: BTreeSet<&String> = all.iter().copied().collect();
    assert_eq!(all.len(), 40, "every queue entry is claimed");
    assert_eq!(distinct.len(), 40, "no queue entry is claimed twice");
    assert!(store.scan_prefix("q/").expect("scan").is_empty());
    assert_eq!(store.scan_prefix("lease/").expect("scan").len(), 40);
}

fn lock_row(raw: &mut Client, schema: &str, key: &str) {
    raw.batch_execute("BEGIN").expect("begins");
    raw.query(
        &format!("SELECT 1 FROM {schema}.kv WHERE key = $1 FOR UPDATE"),
        &[&key],
    )
    .expect("locks the row");
}

fn claim_past_a_held_row(flaw: Flaw) {
    // Three seconds is how long a claim that waits is allowed to wait before the server
    // gives up on it; a claim that skips never waits.
    let backend = PostgresBackend::new(&url_setting("lock_timeout", "3000"), flaw);
    let config = backend.fresh_config();
    let store = PostgresStore::open(&config).expect("opens").with_flaw(flaw);
    seed_queue(&store, 5);

    let mut other = Client::connect(&url(), NoTls).expect("connects");
    lock_row(&mut other, config.schema(), "q/000");
    let got = store
        .claim("q/", 10, take)
        .expect("a held row must not stall the claim");
    let keys: Vec<&str> = got.iter().map(|c| c.key.as_str()).collect();
    assert_eq!(
        keys,
        ["q/001", "q/002", "q/003", "q/004"],
        "the held row is skipped"
    );

    other.batch_execute("COMMIT").expect("releases the row");
    let got = store.claim("q/", 10, take).expect("claims");
    let keys: Vec<&str> = got.iter().map(|c| c.key.as_str()).collect();
    assert_eq!(
        keys,
        ["q/000"],
        "the skipped row is still there once released"
    );
}

// Catches a claim that waits behind a row another session holds.
#[test]
#[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the state-postgres CI job runs it"]
fn a_claim_skips_a_row_another_session_holds() {
    claim_past_a_held_row(Flaw::None);
}

#[test]
#[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the state-postgres CI job runs it"]
#[should_panic(expected = "lock timeout")]
fn a_claim_that_waits_stalls_behind_a_held_row() {
    claim_past_a_held_row(Flaw::ClaimBlocks);
}

// Catches a claim that writes over a row whose version moved on, and one that claims a
// row its caller declined.
#[test]
#[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the state-postgres CI job runs it"]
fn a_lost_version_or_a_declined_row_is_not_claimed() {
    let backend = backend(Flaw::None);
    let store = PostgresStore::open(&backend.fresh_config()).expect("opens");
    seed_queue(&store, 3);

    let got = store
        .claim("q/", 10, |key, was| match key {
            "q/000" => Some(
                Txn::new().delete(key, Cond::Version(kbf_state::Version(was.version.0 + 1000))),
            ),
            "q/001" => None,
            _ => take(key, was),
        })
        .expect("claims");
    let keys: Vec<&str> = got.iter().map(|c| c.key.as_str()).collect();
    assert_eq!(keys, ["q/002"]);
    let left: Vec<String> = store
        .scan_prefix("q/")
        .expect("scan")
        .into_iter()
        .map(|(k, _)| k)
        .collect();
    assert_eq!(left, ["q/000", "q/001"], "the other two stay queued");
}

// ---- configuration (no server) ------------------------------------------------------------

// Catches `--state-store=postgres` accepted with no URL behind it.
#[test]
fn a_missing_url_is_rejected() {
    assert_eq!(PostgresConfig::parse("").unwrap_err(), ConfigError::Missing);
    assert_eq!(
        PostgresConfig::parse("  ").unwrap_err(),
        ConfigError::Missing
    );
}

#[test]
fn only_postgres_urls_are_accepted() {
    for url in ["sqlite:///tmp/x.db", "mysql://h/db", "localhost:5432"] {
        assert_eq!(
            PostgresConfig::parse(url).unwrap_err(),
            ConfigError::NotPostgres,
            "{url}"
        );
    }
    assert_eq!(
        PostgresConfig::parse("postgres:///kbf").unwrap_err(),
        ConfigError::NoHost
    );
}

// Catches a TLS request quietly downgraded to a plain connection.
#[test]
fn a_tls_url_is_refused_not_downgraded() {
    assert_eq!(
        PostgresConfig::parse("postgres://u@db.example/kbf?sslmode=require").unwrap_err(),
        ConfigError::TlsUnsupported
    );
    PostgresConfig::parse("postgres://u@db.example/kbf?sslmode=prefer")
        .expect("prefer connects plain");
    PostgresConfig::parse("postgres://u@db.example/kbf?sslmode=disable")
        .expect("disable connects plain");
}

#[test]
fn schema_names_are_plain() {
    let config = PostgresConfig::parse("postgres://u@db.example/kbf").expect("parses");
    assert_eq!(config.schema(), "kbf_state", "the default schema");
    for bad in ["", "1abc", "Has-Dash", "UPPER", "a b", "x;drop"] {
        assert_eq!(
            config.clone().with_schema(bad).unwrap_err(),
            ConfigError::BadSchema(bad.to_owned())
        );
    }
    let config = config.with_schema("farm_2").expect("a plain name");
    assert_eq!(config.schema(), "farm_2");
}

#[test]
fn the_password_stays_out_of_debug_output() {
    let config = PostgresConfig::parse("postgres://u:hunter2@db.example/kbf").expect("parses");
    assert!(!format!("{config:?}").contains("hunter2"));
}
