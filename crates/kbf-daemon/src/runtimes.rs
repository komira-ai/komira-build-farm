//! The runtimes one daemon runs leases through: [`Runtimes`], a set in which each lease
//! kind has at most one runtime. A Start goes to the runtime that serves its kind; the
//! node report lists every runtime's driver under `drivers`.
//!
//! [`Runtime`] returns `impl Future`, so runtimes of different types cannot share one
//! list as they are. The set holds each behind [`AnyRuntime`], which boxes the futures
//! `run` and `kill` return: one allocation per lease and per kill.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use kbf_proto::reapi::ActionResult;
use kbf_types::{LeaseId, LeaseKind};

use crate::runtime::{Runtime, RuntimeError, Work};

/// A future a runtime behind [`AnyRuntime`] returns.
type Boxed<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// [`Runtime`] with its futures boxed, so that runtimes of different types share a set.
pub(crate) trait AnyRuntime: Send + Sync {
    fn driver(&self) -> &'static str;
    fn serves(&self, kind: &str) -> bool;
    fn run(&self, work: Work) -> Boxed<'_, Result<ActionResult, RuntimeError>>;
    fn kill(&self, lease_id: LeaseId) -> Boxed<'_, ()>;
}

impl<R: Runtime> AnyRuntime for R {
    fn driver(&self) -> &'static str {
        Runtime::driver(self)
    }

    fn serves(&self, kind: &str) -> bool {
        Runtime::serves(self, kind)
    }

    fn run(&self, work: Work) -> Boxed<'_, Result<ActionResult, RuntimeError>> {
        Box::pin(Runtime::run(self, work))
    }

    fn kill(&self, lease_id: LeaseId) -> Boxed<'_, ()> {
        Box::pin(Runtime::kill(self, lease_id))
    }
}

/// The runtimes of one daemon, in the order they were added. Never empty, and no two
/// serve the same lease kind of [`LeaseKind::ALL`]: [`Runtimes::and`] refuses the second.
pub struct Runtimes {
    all: Vec<Arc<dyn AnyRuntime>>,
}

/// Two runtimes of one daemon serve the same lease kind, so a Start of that kind could
/// go to either.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error(
    "drivers {first:?} and {second:?} both serve lease kind {kind}: each kind must have one \
     driver on a node"
)]
pub struct KindServedTwice {
    pub kind: LeaseKind,
    /// The driver already in the set.
    pub first: &'static str,
    /// The driver refused.
    pub second: &'static str,
}

impl Runtimes {
    /// The set of `first` alone.
    #[must_use]
    pub fn new<R: Runtime>(first: Arc<R>) -> Self {
        Self { all: vec![first] }
    }

    /// This set with `next` added after the others.
    ///
    /// # Errors
    /// A kind of [`LeaseKind::ALL`] that `next` serves is served by a runtime already in
    /// the set.
    pub fn and<R: Runtime>(mut self, next: Arc<R>) -> Result<Self, KindServedTwice> {
        for kind in LeaseKind::ALL {
            if !Runtime::serves(&*next, kind.name()) {
                continue;
            }
            if let Some(first) = self.serving(kind.name()) {
                return Err(KindServedTwice {
                    kind,
                    first: first.driver(),
                    second: Runtime::driver(&*next),
                });
            }
        }
        self.all.push(next);
        Ok(self)
    }

    /// Every runtime's driver, in the order the runtimes were added, each once.
    #[must_use]
    pub fn drivers(&self) -> Vec<&'static str> {
        let mut drivers: Vec<&'static str> = Vec::new();
        for driver in self.all.iter().map(|r| r.driver()) {
            if !drivers.contains(&driver) {
                drivers.push(driver);
            }
        }
        drivers
    }

    /// The runtime that serves lease kind `kind`, if any; for a kind outside
    /// [`LeaseKind::ALL`], which two may serve, the first added.
    pub(crate) fn serving(&self, kind: &str) -> Option<&Arc<dyn AnyRuntime>> {
        self.all.iter().find(|r| r.serves(kind))
    }
}

impl<R: Runtime> From<Arc<R>> for Runtimes {
    fn from(runtime: Arc<R>) -> Self {
        Self::new(runtime)
    }
}

impl std::fmt::Debug for Runtimes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Runtimes").field(&self.drivers()).finish()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::FakeRuntime;

    fn fake(kind: LeaseKind, driver: &'static str) -> Arc<FakeRuntime> {
        Arc::new(FakeRuntime::new(Duration::ZERO).serving(kind, driver))
    }

    /// Catches: a set that takes a second runtime for a kind one already serves (a
    /// Start of that kind would go to whichever comes first, silently), and an error
    /// that names the wrong kind or drivers.
    #[test]
    fn a_kind_served_twice_is_refused() {
        let set = Runtimes::new(fake(LeaseKind::Action, "a"));
        let err = set
            .and(fake(LeaseKind::Action, "b"))
            .expect_err("two runtimes serve action");
        assert_eq!(
            err,
            KindServedTwice {
                kind: LeaseKind::Action,
                first: "a",
                second: "b",
            }
        );
        let text = err.to_string();
        assert!(text.contains("\"a\" and \"b\""), "{text}");
        assert!(text.contains("lease kind action:"), "{text}");
    }

    /// Catches: a lookup that answers the first runtime whatever the kind, and drivers
    /// that list only the first runtime's, or one driver twice.
    #[test]
    fn each_kind_finds_its_runtime() {
        let set = Runtimes::new(fake(LeaseKind::Action, "a"))
            .and(fake(LeaseKind::WholeMachine, "b"))
            .expect("distinct kinds")
            .and(fake(LeaseKind::Vm, "a"))
            .expect("distinct kinds");
        let driver = |kind: LeaseKind| set.serving(kind.name()).map(|r| r.driver());
        assert_eq!(driver(LeaseKind::Action), Some("a"));
        assert_eq!(driver(LeaseKind::WholeMachine), Some("b"));
        assert_eq!(driver(LeaseKind::Vm), Some("a"));
        assert_eq!(set.serving("gpu").map(|r| r.driver()), None);
        assert_eq!(set.drivers(), ["a", "b"]);
        assert_eq!(format!("{set:?}"), r#"Runtimes(["a", "b"])"#);
    }
}
