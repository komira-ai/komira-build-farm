use std::sync::{Arc, Mutex};

use kbf_map::Map;
use tokio::sync::watch;

use crate::MapSource;

/// A [`MapSource`] over a cell a test sets, for the tests of whatever reads a source.
#[derive(Debug)]
pub struct FakeMapSource {
    map: Mutex<Arc<Map>>,
    generation: watch::Sender<u64>,
}

impl FakeMapSource {
    /// A source holding `map`.
    #[must_use]
    pub fn new(map: Map) -> Self {
        let generation = map.generation();
        Self {
            map: Mutex::new(Arc::new(map)),
            generation: watch::Sender::new(generation),
        }
    }

    /// The source now holds `map`, and its receivers are notified. Not checked against the
    /// map held before: a test may go back to see what a reader does about it.
    pub fn set(&self, map: Map) {
        let generation = map.generation();
        let mut held = self.map.lock().expect("lock");
        *held = Arc::new(map);
        // Announced under the lock the map is stored under, as the trait says.
        self.generation.send_replace(generation);
    }
}

impl MapSource for FakeMapSource {
    fn current(&self) -> Option<Arc<Map>> {
        Some(Arc::clone(&self.map.lock().expect("lock")))
    }

    fn changes(&self) -> watch::Receiver<u64> {
        self.generation.subscribe()
    }
}
