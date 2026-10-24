//! What a [`Tiered`](crate::Tiered) knows about the objects in its hot tier.

use std::collections::{BTreeMap, BTreeSet};

/// One object in the hot tier.
struct Entry {
    /// The object's size in bytes.
    size: u64,
    /// When it was last put or read, as a position in the order of uses.
    tick: u64,
    /// Whether the cold tier holds a copy: the write-back copied it, or recovery found one
    /// of the same size.
    copied: bool,
}

/// The hot tier's objects, with their use order and the byte count the budget is held to.
///
/// It is a cache of what the two backends hold, rebuilt by listing them when a store is
/// opened. It is not a record of anything the backends do not hold.
#[derive(Default)]
pub(crate) struct State {
    entries: BTreeMap<String, Entry>,
    /// Ticks to keys, least recently used first. Every entry has exactly one tick here.
    order: BTreeMap<u64, String>,
    /// Keys the cold tier holds with other content than the hot tier's. The write-back leaves
    /// them alone until they are deleted.
    clashed: BTreeSet<String>,
    clock: u64,
    bytes: u64,
}

impl State {
    /// Starts tracking an object, as the most recently used. An object already tracked under
    /// the key is replaced, so that every entry keeps exactly one tick and one count of bytes.
    pub(crate) fn insert(&mut self, key: &str, size: u64, copied: bool) {
        if let Some(old) = self.entries.remove(key) {
            self.order.remove(&old.tick);
            self.bytes -= old.size;
        }
        self.clock += 1;
        self.order.insert(self.clock, key.to_owned());
        let entry = Entry {
            size,
            tick: self.clock,
            copied,
        };
        self.entries.insert(key.to_owned(), entry);
        self.bytes += size;
    }

    /// Stops tracking an object, and forgets that it clashed.
    pub(crate) fn remove(&mut self, key: &str) {
        if let Some(entry) = self.entries.remove(key) {
            self.order.remove(&entry.tick);
            self.bytes -= entry.size;
        }
        self.clashed.remove(key);
    }

    /// Makes an object the most recently used, if it is tracked.
    pub(crate) fn touch(&mut self, key: &str) {
        self.clock += 1;
        let clock = self.clock;
        if let Some(entry) = self.entries.get_mut(key) {
            self.order.remove(&entry.tick);
            entry.tick = clock;
            self.order.insert(clock, key.to_owned());
        }
    }

    /// Records whether the cold tier holds a copy of an object. An untracked key is ignored.
    pub(crate) fn set_copied(&mut self, key: &str, copied: bool) {
        if let Some(entry) = self.entries.get_mut(key) {
            entry.copied = copied;
        }
    }

    /// Records that the cold tier holds other content under this key than the hot tier.
    pub(crate) fn set_clashed(&mut self, key: &str) {
        self.clashed.insert(key.to_owned());
    }

    /// Whether an object is tracked.
    pub(crate) fn tracks(&self, key: &str) -> bool {
        self.entries.contains_key(key)
    }

    /// The bytes of the tracked objects.
    pub(crate) fn bytes(&self) -> u64 {
        self.bytes
    }

    /// The keys that clashed, in key order.
    pub(crate) fn clashed(&self) -> Vec<String> {
        self.clashed.iter().cloned().collect()
    }

    /// The tracked objects the cold tier does not hold yet and that did not clash, least
    /// recently used first.
    fn pending_keys(&self) -> impl Iterator<Item = &String> {
        self.order
            .values()
            .filter(move |key| !self.entries[*key].copied)
            .filter(move |key| !self.clashed.contains(*key))
    }

    /// The objects the cold tier does not hold yet and that did not clash, least recently
    /// used first.
    pub(crate) fn pending(&self) -> Vec<String> {
        self.pending_keys().cloned().collect()
    }

    /// Whether an object is tracked and the cold tier does not hold it yet and it did not
    /// clash.
    pub(crate) fn is_pending(&self, key: &str) -> bool {
        let Some(entry) = self.entries.get(key) else {
            return false;
        };
        if entry.copied {
            return false;
        }
        !self.clashed.contains(key)
    }

    /// The least recently used object the cold tier holds that is not in `skip`, if the
    /// tracked bytes are above `budget`.
    pub(crate) fn next_victim(&self, budget: u64, skip: &BTreeSet<String>) -> Option<String> {
        if self.bytes <= budget {
            return None;
        }
        self.order
            .values()
            .filter(|key| self.entries[*key].copied)
            .find(|key| !skip.contains(key.as_str()))
            .cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skipping(keys: &[&str]) -> BTreeSet<String> {
        keys.iter().map(|key| String::from(*key)).collect()
    }

    /// Asserts that the tracked bytes, the pending list and the clash list follow inserts,
    /// touches, clashes and removals, and that removing an untracked key changes nothing.
    #[test]
    fn bytes_and_pending_follow_the_objects() {
        let mut state = State::default();
        state.insert("a", 10, false);
        state.insert("b", 20, true);
        state.insert("c", 5, false);
        assert_eq!(state.bytes(), 35);
        assert_eq!(state.pending(), ["a", "c"]);
        state.touch("a");
        assert_eq!(state.pending(), ["c", "a"]);
        state.set_clashed("c");
        assert_eq!(state.pending(), ["a"]);
        assert_eq!(state.clashed(), ["c"]);
        state.remove("c");
        assert_eq!(state.bytes(), 30);
        assert_eq!(state.clashed(), Vec::<String>::new());
        state.remove("nothing");
        assert_eq!(state.bytes(), 30);
    }

    /// Asserts that inserting a key that is already tracked replaces its entry: the bytes are
    /// the new object's, the key is listed once in the use order, and removing it leaves
    /// nothing behind.
    #[test]
    fn inserting_a_tracked_key_replaces_its_entry() {
        let mut state = State::default();
        state.insert("a", 10, true);
        state.insert("b", 5, false);
        state.insert("a", 3, false);
        assert_eq!(state.bytes(), 8);
        assert_eq!(state.pending(), ["b", "a"]);
        state.remove("a");
        assert_eq!(state.bytes(), 5);
        assert_eq!(state.pending(), ["b"]);
        assert!(state.tracks("b"));
        assert!(!state.tracks("a"));
    }

    /// Asserts that marking or touching an untracked key changes nothing, and that
    /// marking a tracked one moves it between the pending list and the copied ones.
    #[test]
    fn untracked_keys_are_ignored() {
        let mut state = State::default();
        state.set_copied("nothing", true);
        state.touch("nothing");
        assert_eq!(state.bytes(), 0);
        state.insert("a", 1, false);
        state.set_copied("a", true);
        assert_eq!(state.pending(), Vec::<String>::new());
        state.set_copied("a", false);
        assert_eq!(state.pending(), ["a"]);
    }

    /// Asserts that a victim is named only above the budget, is the least recently used
    /// object the cold tier holds, and is never one in the skip set or one it lacks.
    #[test]
    fn the_victim_is_the_oldest_copied_object_above_the_budget() {
        let mut state = State::default();
        state.insert("a", 10, true);
        state.insert("b", 10, false);
        state.insert("c", 10, true);
        let none = skipping(&[]);
        let a = skipping(&["a"]);
        let a_and_c = skipping(&["a", "c"]);
        let b = skipping(&["b"]);
        assert_eq!(state.next_victim(30, &none), None);
        assert_eq!(state.next_victim(25, &none), Some("a".to_owned()));
        assert_eq!(state.next_victim(25, &a), Some("c".to_owned()));
        assert_eq!(state.next_victim(25, &a_and_c), None);
        assert_eq!(state.next_victim(25, &b), Some("a".to_owned()));
    }

    /// Asserts that an object is pending only while it is tracked, the cold tier is not known
    /// to hold it and it has not clashed.
    #[test]
    fn an_object_is_pending_until_copied_clashed_or_removed() {
        let mut state = State::default();
        state.insert("a", 1, false);
        state.insert("b", 1, true);
        state.insert("c", 1, false);
        state.set_clashed("c");
        assert!(state.is_pending("a"));
        assert!(!state.is_pending("b"));
        assert!(!state.is_pending("c"));
        assert!(!state.is_pending("nothing"));
        state.remove("a");
        assert!(!state.is_pending("a"));
    }
}
