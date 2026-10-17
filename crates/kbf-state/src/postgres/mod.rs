//! The Postgres backend (feature `postgres`): section 4.3 of `docs/design/architecture-v2.md`,
//! clause by clause.
//!
//! - **C1, C2.** A batch is one `SERIALIZABLE` transaction. Each transaction in it loads
//!   its keys `FOR UPDATE`, judges every precondition against what it loaded, then writes.
//!   A serialization failure runs the batch again.
//! - **C3.** A counter is a row incremented in the writing transaction.
//! - **C4.** Expiry is `expires_us` against the database's `clock_timestamp()`, read
//!   after the head lock and again by every read; a reaper (`reap_expired`) writes the
//!   `Expire` rows. A test injects a clock ([`PostgresStore::with_clock`]) and the same
//!   statements take its reading instead.
//! - **C5.** Revisions come from one head row that a writer locks first and holds to its
//!   commit, so they commit in the order they were taken. [`Store::changes`] reads the
//!   committed head, then the rows up to it, and returns the head as its cursor: nothing
//!   at or below it can still commit. A bare sequence would not do (a later number can
//!   commit first and a poller steps over the earlier one); [`Flaw::SequenceRevisions`]
//!   is that mistake, kept so the suite can show it fails. Polling is the baseline;
//!   this module sends no `NOTIFY`.
//! - **C6.** `synchronous_commit` must be on: a connection where it is not is refused,
//!   so an acknowledged write has been flushed.
//! - **C7.** A batch is one transaction and one commit, however many transactions it
//!   holds; the server's WAL flush groups commits from concurrent callers.
//!
//! [`PostgresStore::claim`] is the claim path: read committed, rows locked
//! `FOR UPDATE SKIP LOCKED`, each claimed through the same version compare-and-set.
//!
//! Limits: the feed table grows until a retention pass trims it (not written yet); the
//! client does not speak TLS, so an `sslmode=require` URL is refused instead of
//! downgraded; calls block, so an async caller uses a blocking task.

#[cfg(any(test, feature = "test-support"))]
mod backend;
mod session;
mod tx;

use std::fmt;
use std::ops::Bound;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use postgres::config::SslMode;
use postgres::error::SqlState;
use postgres::types::ToSql;

#[cfg(any(test, feature = "test-support"))]
pub use backend::PostgresBackend;
use session::{Edge, Pool, RangeShape, Session};
use tx::{Level, Row, Tx, micros};

use crate::clock::{Clock, StoreTime};
use crate::store::{Span, Store, page_of};
use crate::{
    Change, Changes, Commit, CommitStats, Conflict, Direction, Event, Revision, ScanPage,
    ScanRange, StoreError, Txn, Version, Versioned,
};

/// The most times a batch is run again after a serialization failure.
const MAX_ATTEMPTS: usize = 200;

/// A deliberate defect, to show the suite can fail on a Postgres store.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Flaw {
    /// A correct store.
    #[default]
    None,
    /// C5: revisions come from a sequence, outside the transaction, and the feed
    /// cursor is the last row seen, so a later revision that commits first makes the
    /// poller step over the earlier one.
    SequenceRevisions,
    /// The claim waits for a row another transaction holds instead of skipping it.
    ClaimBlocks,
}

/// Why a connection string is not usable.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    /// No URL was given.
    #[error("the postgres state store needs a URL (postgres://...)")]
    Missing,
    /// The URL is not a `postgres://` or `postgresql://` URL.
    #[error("the state store URL must start with postgres:// or postgresql://")]
    NotPostgres,
    /// The URL names no host.
    #[error("the state store URL names no host")]
    NoHost,
    /// The URL asks for TLS, which this client does not speak.
    #[error("the state store URL asks for TLS (sslmode), which this client does not support")]
    TlsUnsupported,
    /// The URL does not parse.
    #[error("the state store URL is not valid: {0}")]
    Invalid(String),
    /// The schema name is not lower-case letters, digits and underscores.
    #[error(
        "the state store schema {0:?} must be 1 to 63 characters of a-z, 0-9 and _, not starting with a digit"
    )]
    BadSchema(String),
}

/// Where the store is and how it connects.
#[derive(Clone)]
pub struct PostgresConfig {
    pg: postgres::Config,
    schema: String,
    max_connections: usize,
}

impl fmt::Debug for PostgresConfig {
    // The connection string carries a password; leave it out.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PostgresConfig")
            .field("schema", &self.schema)
            .field("max_connections", &self.max_connections)
            .finish_non_exhaustive()
    }
}

impl PostgresConfig {
    /// The schema a store lives in unless told otherwise.
    pub const DEFAULT_SCHEMA: &'static str = "kbf_state";

    /// The most connections a store opens unless told otherwise. Writers queue on the
    /// commit head, so more would only wait.
    pub const DEFAULT_MAX_CONNECTIONS: usize = 8;

    /// Reads a `postgres://` URL.
    ///
    /// # Errors
    /// [`ConfigError`] when the URL is missing, is not a Postgres URL, names no host or
    /// asks for TLS.
    pub fn parse(url: &str) -> Result<PostgresConfig, ConfigError> {
        let url = url.trim();
        if url.is_empty() {
            return Err(ConfigError::Missing);
        }
        if !(url.starts_with("postgres://") || url.starts_with("postgresql://")) {
            return Err(ConfigError::NotPostgres);
        }
        let mut pg: postgres::Config = url
            .parse()
            .map_err(|err: postgres::Error| ConfigError::Invalid(err.to_string()))?;
        if pg.get_hosts().is_empty() {
            return Err(ConfigError::NoHost);
        }
        if !matches!(pg.get_ssl_mode(), SslMode::Disable | SslMode::Prefer) {
            return Err(ConfigError::TlsUnsupported);
        }
        if pg.get_application_name().is_none() {
            pg.application_name("kbf-state");
        }
        Ok(PostgresConfig {
            pg,
            schema: Self::DEFAULT_SCHEMA.to_owned(),
            max_connections: Self::DEFAULT_MAX_CONNECTIONS,
        })
    }

    /// Keeps the store's tables in `schema`.
    ///
    /// # Errors
    /// [`ConfigError::BadSchema`] when the name is not plain lower case.
    pub fn with_schema(mut self, schema: &str) -> Result<PostgresConfig, ConfigError> {
        let plain = !schema.is_empty()
            && schema.len() <= 63
            && !schema.starts_with(|c: char| c.is_ascii_digit())
            && schema
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
        if !plain {
            return Err(ConfigError::BadSchema(schema.to_owned()));
        }
        self.schema = schema.to_owned();
        Ok(self)
    }

    /// Names the connections in `pg_stat_activity`.
    #[must_use]
    pub fn with_application_name(mut self, name: &str) -> PostgresConfig {
        self.pg.application_name(name);
        self
    }

    /// Opens at most `n` connections (at least one).
    #[must_use]
    pub fn with_max_connections(mut self, n: usize) -> PostgresConfig {
        self.max_connections = n.max(1);
        self
    }

    /// The schema the store's tables are in.
    #[must_use]
    pub fn schema(&self) -> &str {
        &self.schema
    }
}

/// A failure inside the store, before it is reported as a [`StoreError`].
#[derive(Debug)]
pub(crate) enum Fail {
    Pg(postgres::Error),
    Refused(String),
}

impl From<postgres::Error> for Fail {
    fn from(err: postgres::Error) -> Fail {
        Fail::Pg(err)
    }
}

impl Fail {
    /// A transaction the server rolled back for a reason that running it again cures.
    fn retryable(&self) -> bool {
        match self {
            Fail::Pg(err) => err.code().is_some_and(|code| {
                *code == SqlState::T_R_SERIALIZATION_FAILURE
                    || *code == SqlState::T_R_DEADLOCK_DETECTED
            }),
            Fail::Refused(_) => false,
        }
    }

    fn into_store(self) -> StoreError {
        StoreError::Failed(match self {
            Fail::Pg(err) => describe(&err),
            Fail::Refused(why) => why,
        })
    }
}

/// A row claimed by [`PostgresStore::claim`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Claimed {
    /// The key.
    pub key: String,
    /// The value as it was when claimed.
    pub was: Versioned,
    /// The commit of the transaction the caller built for it.
    pub commit: Commit,
}

/// What the server said, which `Display` for the client's error leaves out ("db error").
fn describe(err: &postgres::Error) -> String {
    if let Some(db) = err.as_db_error() {
        return format!(
            "{}: {} (SQLSTATE {})",
            db.severity(),
            db.message(),
            db.code().code()
        );
    }
    match std::error::Error::source(err) {
        Some(source) => format!("{err}: {source}"),
        None => err.to_string(),
    }
}

fn to_time(us: i64) -> StoreTime {
    StoreTime::ZERO.saturating_add(Duration::from_micros(u64::try_from(us).unwrap_or(0)))
}

fn from_time(time: StoreTime) -> i64 {
    micros(time.since(StoreTime::ZERO))
}

fn versioned(row: &Row) -> Versioned {
    Versioned {
        value: row.value.clone(),
        version: Version(u64::try_from(row.version).unwrap_or(0)),
        expires_at: row.expires_us.map(to_time),
    }
}

/// The state store on Postgres.
pub struct PostgresStore {
    pool: Pool,
    config: PostgresConfig,
    clock: Option<Arc<dyn Clock>>,
    flaw: Flaw,
    txns: AtomicU64,
    syncs: AtomicU64,
}

/// A batch run in a database transaction that is not committed yet. Test support: it is
/// how a test holds one commit back while another goes through.
#[doc(hidden)]
pub struct Pending<'s> {
    store: &'s PostgresStore,
    tx: Tx<'s>,
    results: Vec<Result<Commit, Conflict>>,
}

impl Pending<'_> {
    /// Commits, and returns the result of each transaction.
    ///
    /// # Errors
    /// [`StoreError`] when the commit fails.
    pub fn commit(self) -> Result<Vec<Result<Commit, Conflict>>, StoreError> {
        let Pending { store, tx, results } = self;
        let wrote = tx.dirty();
        tx.commit().map_err(Fail::into_store)?;
        store.count(&results, wrote);
        Ok(results)
    }
}

impl PostgresStore {
    /// Connects, checks that the server makes commits durable, and creates the tables
    /// if they are not there. Expiry is judged by the database's clock.
    ///
    /// # Errors
    /// [`StoreError`] when the server cannot be reached or does not flush commits
    /// (`synchronous_commit` is not on).
    pub fn open(config: &PostgresConfig) -> Result<PostgresStore, StoreError> {
        let pool = Pool::open(
            config.pg.clone(),
            config.schema.clone(),
            config.max_connections,
        )
        .map_err(Fail::into_store)?;
        Ok(PostgresStore {
            pool,
            config: config.clone(),
            clock: None,
            flaw: Flaw::None,
            txns: AtomicU64::new(0),
            syncs: AtomicU64::new(0),
        })
    }

    /// Judges expiry by `clock` instead of the database's. For tests.
    #[must_use]
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> PostgresStore {
        self.clock = Some(clock);
        self
    }

    /// Plants a deliberate defect. For tests.
    #[must_use]
    pub fn with_flaw(mut self, flaw: Flaw) -> PostgresStore {
        self.flaw = flaw;
        self
    }

    /// The configuration this store was opened with.
    #[must_use]
    pub fn config(&self) -> &PostgresConfig {
        &self.config
    }

    /// Opens the same database again with the same clock and flaw, dropping this
    /// store's connections: a crash of the frontend, for a test.
    ///
    /// # Errors
    /// [`StoreError`] as for [`PostgresStore::open`].
    pub fn reopen(self) -> Result<PostgresStore, StoreError> {
        let mut store = PostgresStore::open(&self.config)?.with_flaw(self.flaw);
        store.clock = self.clock;
        Ok(store)
    }

    fn over(&self) -> Option<i64> {
        self.clock.as_ref().map(|clock| from_time(clock.now()))
    }

    fn count(&self, results: &[Result<Commit, Conflict>], wrote: bool) {
        let applied = results.iter().filter(|result| result.is_ok()).count();
        self.txns.fetch_add(applied as u64, Ordering::Relaxed);
        if wrote {
            self.syncs.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn read<T>(
        &self,
        f: impl FnOnce(&mut Session) -> Result<T, postgres::Error>,
    ) -> Result<T, StoreError> {
        let mut session = self.pool.checkout().map_err(Fail::into_store)?;
        f(&mut session).map_err(|err| Fail::Pg(err).into_store())
    }

    /// Runs `attempt` until it is not rolled back for a serialization failure.
    fn retrying<T>(&self, mut attempt: impl FnMut() -> Result<T, Fail>) -> Result<T, StoreError> {
        for _ in 0..MAX_ATTEMPTS {
            match attempt() {
                Ok(done) => return Ok(done),
                Err(fail) if fail.retryable() => {}
                Err(fail) => return Err(fail.into_store()),
            }
        }
        Err(StoreError::Failed(format!(
            "the transaction failed to serialize {MAX_ATTEMPTS} times"
        )))
    }

    fn start(&self, level: Level) -> Result<Tx<'_>, Fail> {
        let session = self.pool.checkout()?;
        Tx::begin(session, level, self.over(), self.flaw)
    }

    fn run_batch(&self, batch: &[Txn]) -> Result<(Tx<'_>, Vec<Result<Commit, Conflict>>), Fail> {
        let mut tx = self.start(Level::Serializable)?;
        let mut results = Vec::with_capacity(batch.len());
        for txn in batch {
            results.push(tx.apply(txn)?);
        }
        Ok((tx, results))
    }

    /// Runs `txn` to the point of commit and holds it there. Test support.
    ///
    /// # Errors
    /// [`StoreError`] when the database fails.
    #[doc(hidden)]
    pub fn begin(&self, txn: Txn) -> Result<Pending<'_>, StoreError> {
        let batch = [txn];
        let (tx, results) = self.retrying(|| self.run_batch(&batch))?;
        Ok(Pending {
            store: self,
            tx,
            results,
        })
    }

    /// Claims up to `limit` live rows under `prefix`, in key order, skipping rows another
    /// transaction holds. `make` turns each row into the transaction that claims it
    /// (typically: delete the queue entry on its version, write a lease); `None` passes
    /// the row over, and a transaction whose precondition fails is a lost race and is
    /// passed over too. All claims commit together.
    ///
    /// The transaction runs read committed: after the commit-head lock every statement
    /// reads the latest committed rows, so there is no serialization failure to retry.
    ///
    /// # Errors
    /// [`StoreError`] when the database fails; nothing is claimed then.
    pub fn claim(
        &self,
        prefix: &str,
        limit: usize,
        mut make: impl FnMut(&str, &Versioned) -> Option<Txn>,
    ) -> Result<Vec<Claimed>, StoreError> {
        self.claim_in(prefix, limit, &mut make)
            .map_err(Fail::into_store)
    }

    fn claim_in(
        &self,
        prefix: &str,
        limit: usize,
        make: &mut dyn FnMut(&str, &Versioned) -> Option<Txn>,
    ) -> Result<Vec<Claimed>, Fail> {
        let mut tx = self.start(Level::ReadCommitted)?;
        let take = i64::try_from(limit).unwrap_or(i64::MAX);
        let skip_locked = self.flaw != Flaw::ClaimBlocks;
        let rows = tx.claim_rows(prefix, take, skip_locked)?;
        for (key, row) in &rows {
            tx.remember(key.clone(), row.clone());
        }
        let mut claimed = Vec::new();
        for (key, row) in rows {
            let was = versioned(&row);
            let Some(txn) = make(&key, &was) else {
                continue;
            };
            if let Ok(commit) = tx.apply(&txn)? {
                claimed.push(Claimed { key, was, commit });
            }
        }
        let wrote = tx.dirty();
        tx.commit()?;
        if wrote {
            self.syncs.fetch_add(1, Ordering::Relaxed);
        }
        self.txns.fetch_add(claimed.len() as u64, Ordering::Relaxed);
        Ok(claimed)
    }
}

impl Store for PostgresStore {
    fn apply_batch(&self, batch: Vec<Txn>) -> Result<Vec<Result<Commit, Conflict>>, StoreError> {
        if batch.is_empty() {
            return Ok(Vec::new());
        }
        // The commit is inside the retry: a serializable transaction can also be refused
        // when it commits.
        self.retrying(|| {
            let (tx, results) = self.run_batch(&batch)?;
            let wrote = tx.dirty();
            tx.commit()?;
            self.count(&results, wrote);
            Ok(results)
        })
    }

    fn get(&self, key: &str) -> Result<Option<Versioned>, StoreError> {
        let over = self.over();
        self.read(|s| {
            let row = s.client.query_opt(&s.st.get, &[&key, &over])?;
            Ok(row.map(|row| versioned(&Row::from_row(&row, 0))))
        })
    }

    fn scan_prefix(&self, prefix: &str) -> Result<Vec<(String, Versioned)>, StoreError> {
        let over = self.over();
        self.read(|s| {
            let rows = s.client.query(&s.st.scan, &[&prefix, &over])?;
            Ok(rows
                .iter()
                .map(|row| (row.get(0), versioned(&Row::from_row(row, 1))))
                .collect())
        })
    }

    /// C8 in one statement, so the page is one snapshot of committed transactions. The
    /// statement for the span's shape asks for one row past the limit to learn whether a
    /// resume token is due.
    fn scan_range(&self, scan: ScanRange<'_>) -> Result<ScanPage, StoreError> {
        let Some(span) = Span::of(&scan) else {
            return Ok(ScanPage {
                rows: Vec::new(),
                resume: None,
            });
        };
        let shape = RangeShape {
            lower_included: span.lower.included,
            upper: Edge::of(&span.upper),
            reverse: scan.direction == Direction::Reverse,
        };
        let over = self.over();
        let take = i64::try_from(scan.limit.get().saturating_add(1)).unwrap_or(i64::MAX);
        let mut params: Vec<&(dyn ToSql + Sync)> = vec![&over, &take, &span.lower.key];
        if let Bound::Included(key) | Bound::Excluded(key) = &span.upper {
            params.push(key);
        }
        let rows = self.read(|s| s.client.query(&s.st.ranges[shape.index()], &params))?;
        let rows = rows
            .iter()
            .map(|row| (row.get(0), versioned(&Row::from_row(row, 1))))
            .collect();
        Ok(page_of(rows, scan.limit))
    }

    fn changes(&self, after: Revision, limit: usize) -> Result<Changes, StoreError> {
        let sequence = self.flaw == Flaw::SequenceRevisions;
        let after_i = i64::try_from(after.0).unwrap_or(i64::MAX);
        let take = i64::try_from(limit).unwrap_or(i64::MAX);
        let (head, rows) = self.read(|s| {
            // The head first: every revision up to it has committed, so the rows read
            // next include all of them.
            let head: i64 = if sequence {
                i64::MAX
            } else {
                s.client.query_one(&s.st.head_read, &[])?.get(0)
            };
            let rows = s.client.query(&s.st.changes, &[&after_i, &head, &take])?;
            Ok((head, rows))
        })?;
        let mut out = Vec::with_capacity(rows.len());
        for row in &rows {
            let rev: i64 = row.get(0);
            let kind: i16 = row.get(2);
            let value: Option<Vec<u8>> = row.get(3);
            let event = match kind {
                0 => Event::Put(value.unwrap_or_default()),
                1 => Event::Delete,
                2 => Event::Expire,
                other => {
                    return Err(StoreError::Failed(format!("unknown feed row kind {other}")));
                }
            };
            out.push(Change {
                revision: Revision(u64::try_from(rev).unwrap_or(0)),
                key: row.get(1),
                event,
            });
        }
        let last = out.last().map_or(after, |change| change.revision);
        // A page that is not full has reached the head; a full one resumes after its
        // last row.
        let cursor = if out.len() < limit && !sequence {
            Revision(u64::try_from(head).unwrap_or(0)).max(after)
        } else {
            last
        };
        Ok(Changes { rows: out, cursor })
    }

    fn reap_expired(&self) -> Result<usize, StoreError> {
        self.retrying(|| {
            let mut tx = self.start(Level::Serializable)?;
            let reaped = tx.reap()?;
            let wrote = tx.dirty();
            tx.commit()?;
            if wrote {
                self.syncs.fetch_add(1, Ordering::Relaxed);
            }
            Ok(reaped)
        })
    }

    fn commit_stats(&self) -> CommitStats {
        CommitStats {
            txns: self.txns.load(Ordering::Relaxed),
            syncs: self.syncs.load(Ordering::Relaxed),
        }
    }

    fn now(&self) -> StoreTime {
        if let Some(clock) = &self.clock {
            return clock.now();
        }
        // The store trait cannot report a failure here; a database that cannot be
        // reached reads as the origin, and the next call that can fail says why.
        self.read(|s| {
            let us: i64 = s.client.query_one(&s.st.now, &[&None::<i64>])?.get(0);
            Ok(to_time(us))
        })
        .unwrap_or(StoreTime::ZERO)
    }
}
