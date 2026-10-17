//! The conformance suite's view of a Postgres database: every store it opens gets a
//! schema of its own in the one database, dropped when the backend is.
//!
//! Test support. The URL names a server the tests may create and drop schemas on.

use std::process;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::Duration;

use postgres::NoTls;

use super::{Flaw, PostgresConfig, PostgresStore};
use crate::clock::ManualClock;
use crate::conformance::Backend;
use crate::{Conflict, Store, Txn};

/// How long [`PostgresBackend::commit_out_of_order`] waits, in polls of 5 ms, for the
/// second transaction to queue behind the first before it gives up.
const WAIT_POLLS: u32 = 6000;

/// Stores opened by any backend in this process get distinct schemas.
static NEXT_SCHEMA: AtomicU64 = AtomicU64::new(0);

/// Opens stores on one Postgres database, one schema each.
pub struct PostgresBackend {
    url: String,
    flaw: Flaw,
    schemas: Mutex<Vec<String>>,
}

impl PostgresBackend {
    /// A backend on the database at `url`, whose stores carry `flaw`.
    #[must_use]
    pub fn new(url: &str, flaw: Flaw) -> PostgresBackend {
        PostgresBackend {
            url: url.to_owned(),
            flaw,
            schemas: Mutex::new(Vec::new()),
        }
    }

    /// A configuration for a new schema, recorded for dropping.
    ///
    /// # Panics
    /// When the URL does not parse.
    #[must_use]
    pub fn fresh_config(&self) -> PostgresConfig {
        let n = NEXT_SCHEMA.fetch_add(1, Ordering::Relaxed);
        let schema = format!("kbf_t{}_{n}", process::id());
        let config = PostgresConfig::parse(&self.url)
            .and_then(|config| config.with_schema(&schema))
            .expect("the test database URL is a usable postgres:// URL")
            .with_application_name(&schema);
        self.schemas
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(schema);
        config
    }

    /// How many of this store's connections wait on a lock.
    fn waiting(store: &PostgresStore) -> i64 {
        let app = store.config.pg.get_application_name().unwrap_or_default();
        store
            .read(|s| {
                let row = s.client.query_one(
                    "SELECT count(*) FROM pg_stat_activity \
                     WHERE application_name = $1 AND wait_event_type = 'Lock'",
                    &[&app],
                )?;
                Ok(row.get(0))
            })
            .expect("the activity view is readable")
    }
}

impl Drop for PostgresBackend {
    fn drop(&mut self) {
        let schemas = self
            .schemas
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner);
        let Ok(config) = PostgresConfig::parse(&self.url) else {
            return;
        };
        let Ok(mut client) = config.pg.connect(NoTls) else {
            return;
        };
        for schema in schemas.iter() {
            // Best effort: a schema left behind is only clutter in a throwaway database.
            let _ = client.batch_execute(&format!("DROP SCHEMA IF EXISTS {schema} CASCADE"));
        }
    }
}

impl Backend for PostgresBackend {
    type Store = PostgresStore;

    fn open(&self, clock: Arc<ManualClock>) -> PostgresStore {
        PostgresStore::open(&self.fresh_config())
            .expect("the test database opens")
            .with_clock(clock)
            .with_flaw(self.flaw)
    }

    fn restart(&self, store: PostgresStore) -> PostgresStore {
        store.reopen().expect("the test database opens again")
    }

    /// With the commit head, a later revision cannot commit first: the second
    /// transaction queues behind the first on the head lock. The poller is let look
    /// while it waits, then the first commits and the second follows. With
    /// [`Flaw::SequenceRevisions`] nothing queues, and the second really does commit
    /// first.
    fn commit_out_of_order(
        &self,
        store: &PostgresStore,
        first: Txn,
        second: Txn,
        between: &mut dyn FnMut(),
    ) -> Result<(), Conflict> {
        let pending = store.begin(first).expect("the first transaction runs");
        let (first_results, second_result) = if self.flaw == Flaw::SequenceRevisions {
            let second_result = store.txn(second).expect("the second transaction commits");
            between();
            let first_results = pending.commit().expect("the first transaction commits");
            (first_results, second_result)
        } else {
            thread::scope(|scope| {
                let late = scope.spawn(|| store.txn(second));
                let mut polls = 0;
                while Self::waiting(store) == 0 {
                    polls += 1;
                    assert!(
                        polls < WAIT_POLLS,
                        "the second transaction never queued behind the first"
                    );
                    thread::sleep(Duration::from_millis(5));
                }
                between();
                let first_results = pending.commit().expect("the first transaction commits");
                let second_result = late
                    .join()
                    .expect("the second transaction's thread did not panic")
                    .expect("the second transaction commits");
                (first_results, second_result)
            })
        };
        for result in first_results {
            result?;
        }
        second_result?;
        Ok(())
    }
}
