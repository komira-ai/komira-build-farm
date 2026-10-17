use std::sync::{Arc, Mutex};

use kbf_map::Map;
use tokio::sync::watch;
use tokio::task::JoinHandle;

/// The newest storage map a process was pushed, and the one it replaced.
///
/// The generation only rises: [`MapHolder::offer`] refuses a map that is not newer than
/// the one held. Shared by `Arc` between the task that receives pushes and the callers
/// that route by the map.
#[derive(Debug)]
pub struct MapHolder {
    held: Mutex<Held>,
    generation: watch::Sender<u64>,
    wanted: watch::Sender<u64>,
}

#[derive(Debug, Default)]
struct Held {
    map: Option<Arc<Map>>,
    previous: Option<Arc<Map>>,
}

impl Default for MapHolder {
    fn default() -> Self {
        Self::new()
    }
}

impl MapHolder {
    /// A holder with no map: generation 0, which is no map's.
    #[must_use]
    pub fn new() -> Self {
        Self {
            held: Mutex::new(Held::default()),
            generation: watch::Sender::new(0),
            wanted: watch::Sender::new(0),
        }
    }

    /// Takes `map` if it is newer than the one held; `false` if it is the same generation
    /// or older, and nothing changes.
    pub fn offer(&self, map: Map) -> bool {
        let generation = map.generation();
        let mut held = self.held.lock().expect("lock");
        if held
            .map
            .as_ref()
            .is_some_and(|have| have.generation() >= generation)
        {
            return false;
        }
        held.previous = held.map.replace(Arc::new(map));
        // Announced under the lock the map is stored under, so a waiter that wakes for the
        // generation reads that map or a newer one.
        self.generation.send_replace(generation);
        true
    }

    /// The newest map, or `None` before the first.
    #[must_use]
    pub fn current(&self) -> Option<Arc<Map>> {
        self.held.lock().expect("lock").map.clone()
    }

    /// The map the newest one replaced: for the routing layer, which reads a blob written
    /// before a join or a drain where it was. `None` before a second map. Whether that
    /// change has settled is not known here.
    #[must_use]
    pub fn previous(&self) -> Option<Arc<Map>> {
        self.held.lock().expect("lock").previous.clone()
    }

    /// The generation of the newest map; 0 before the first.
    #[must_use]
    pub fn generation(&self) -> u64 {
        *self.generation.borrow()
    }

    /// A receiver of the generation of the newest map, notified when a map is taken.
    #[must_use]
    pub fn changes(&self) -> watch::Receiver<u64> {
        self.generation.subscribe()
    }

    /// Calls `apply` with the generation of the newest map now (if there is one) and with
    /// each newer one as it is taken, on a task that ends when the holder is dropped.
    /// For a long-lived client that tags its calls with the generation it routes by:
    /// `holder.follow(move |g| storage.set_map_generation(g))`. A burst of maps is one
    /// call with the last generation.
    pub fn follow(&self, apply: impl Fn(u64) + Send + 'static) -> JoinHandle<()> {
        self.follow_boxed(Box::new(apply))
    }

    /// The body of [`MapHolder::follow`], not generic in the closure: the coverage tool
    /// counts a generic function once per instantiation, and no one test closure
    /// reaches both its branches.
    fn follow_boxed(&self, apply: Box<dyn Fn(u64) + Send>) -> JoinHandle<()> {
        let mut changes = self.generation.subscribe();
        let first = *changes.borrow_and_update();
        if first > 0 {
            apply(first);
        }
        tokio::spawn(async move {
            while changes.changed().await.is_ok() {
                apply(*changes.borrow_and_update());
            }
        })
    }

    /// The newest map of generation `at_least` or later (any map, for 0), waiting for one. Records
    /// `at_least` as wanted ([`MapHolder::asks`]) so the session can ask the frontend for
    /// it: waiting for a push alone would wait forever on one that was lost. Unbounded:
    /// the caller bounds it with a timeout.
    pub async fn reach(&self, at_least: u64) -> Arc<Map> {
        // 0 is no map's generation: asking for "any" is asking for the first.
        let at_least = at_least.max(1);
        self.wanted.send_if_modified(|wanted| {
            let more = at_least > *wanted;
            if more {
                *wanted = at_least;
            }
            more
        });
        let mut changes = self.generation.subscribe();
        // The sender is held by `self`, which this borrows, so it cannot be dropped.
        let _ = changes.wait_for(|generation| *generation >= at_least).await;
        self.current().expect("a generation above 0 is a held map")
    }

    /// A receiver of the newest generation asked for by [`MapHolder::reach`] (0: none).
    /// A session that finds it above [`MapHolder::generation`] asks its frontend for a map.
    #[must_use]
    pub fn asks(&self) -> watch::Receiver<u64> {
        self.wanted.subscribe()
    }
}
