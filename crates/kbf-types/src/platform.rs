//! Platform properties: what an action requires of the worker that runs it.

use std::collections::BTreeMap;
use std::collections::btree_map::Entry;

/// The platform an action requires: a map from property name to value.
///
/// Platform properties are part of the REAPI action digest, so equal platforms must
/// encode to identical bytes. REAPI requires properties sorted by name and then by
/// value, comparing UTF-8 bytes. A `Platform` keeps its properties in a `BTreeMap`,
/// whose `String` order is exactly that byte order, so [`Platform::canonical`] yields
/// the REAPI canonical form however the properties were supplied.
///
/// REAPI lets a server reject a name that appears twice; kbf does, so each name has
/// exactly one value and the sort by value never comes into play.
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Platform {
    properties: BTreeMap<String, String>,
}

/// Why a set of properties is not a valid [`Platform`].
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PlatformError {
    /// A property has an empty name.
    #[error("platform property name is empty")]
    EmptyName,
    /// A property name appears more than once.
    #[error("platform property {0:?} appears more than once")]
    DuplicateName(String),
}

impl Platform {
    /// An empty platform: the action runs on any worker.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Builds a platform from `(name, value)` pairs in any order.
    ///
    /// Fails on an empty name or a repeated name, even if the repeat has the same value:
    /// a client that sends a name twice has a bug worth surfacing.
    pub fn from_properties<I, N, V>(properties: I) -> Result<Self, PlatformError>
    where
        I: IntoIterator<Item = (N, V)>,
        N: Into<String>,
        V: Into<String>,
    {
        let mut platform = Self::new();
        for (name, value) in properties {
            platform.insert(name, value)?;
        }
        Ok(platform)
    }

    /// Adds one property. Fails on an empty name or a name already present.
    pub fn insert(
        &mut self,
        name: impl Into<String>,
        value: impl Into<String>,
    ) -> Result<(), PlatformError> {
        let name = name.into();
        if name.is_empty() {
            return Err(PlatformError::EmptyName);
        }
        match self.properties.entry(name) {
            Entry::Occupied(e) => Err(PlatformError::DuplicateName(e.key().clone())),
            Entry::Vacant(e) => {
                e.insert(value.into());
                Ok(())
            }
        }
    }

    /// The value of property `name`, if present.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&str> {
        self.properties.get(name).map(String::as_str)
    }

    /// The number of properties.
    #[must_use]
    pub fn len(&self) -> usize {
        self.properties.len()
    }

    /// Whether the platform has no properties.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.properties.is_empty()
    }

    /// The properties in canonical order: sorted by name, comparing UTF-8 bytes.
    ///
    /// This is the order to use whenever a platform is encoded into anything that is
    /// hashed or compared, such as an REAPI `Action`.
    pub fn canonical(&self) -> impl ExactSizeIterator<Item = (&str, &str)> + '_ {
        self.properties
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches: a canonical form in insertion order, or in any order other than UTF-8
    /// byte order. Either would give one platform two action digests and split the
    /// cache. The names are chosen so that insertion order, reverse order and a
    /// case-insensitive order all differ from byte order.
    #[test]
    fn canonical_form_is_sorted_by_bytes() {
        let p = Platform::from_properties([
            ("pool", "linux"),
            ("OSFamily", "Linux"),
            ("cpu", "x86_64"),
            ("ISA", "x86-64-v3"),
            ("zone", "b"),
        ])
        .unwrap();
        let names: Vec<&str> = p.canonical().map(|(n, _)| n).collect();
        assert_eq!(names, ["ISA", "OSFamily", "cpu", "pool", "zone"]);
        assert_eq!(p.canonical().len(), 5);
    }

    /// Catches: a platform whose canonical form or equality depends on the order the
    /// client sent the properties in.
    #[test]
    fn order_of_input_does_not_matter() {
        let a = Platform::from_properties([("b", "2"), ("a", "1"), ("c", "3")]).unwrap();
        let b = Platform::from_properties([("c", "3"), ("b", "2"), ("a", "1")]).unwrap();
        assert_eq!(a, b);
        assert!(a.canonical().eq(b.canonical()));
        assert_eq!(a.get("a"), Some("1"));
        assert_eq!(a.get("d"), None);
    }

    /// Catches: a repeated name that silently overwrites (the last value wins) or an
    /// empty name accepted, either of which hides a client bug.
    #[test]
    fn rejects_empty_and_repeated_names() {
        assert_eq!(
            Platform::from_properties([("", "x")]),
            Err(PlatformError::EmptyName)
        );
        assert_eq!(
            Platform::from_properties([("pool", "a"), ("pool", "a")]),
            Err(PlatformError::DuplicateName("pool".to_owned()))
        );
        let empty = Platform::from_properties::<_, String, String>([]).unwrap();
        assert!(empty.is_empty());
        assert_eq!(empty, Platform::new());
    }
}
