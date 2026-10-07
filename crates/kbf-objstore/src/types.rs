//! The values the [`ObjectStore`](crate::ObjectStore) trait speaks in: keys, prefixes,
//! ranges, list pages, capabilities and errors.

use std::fmt;

/// Longest key S3 accepts, in bytes.
const MAX_KEY_BYTES: usize = 1024;

/// The key of one object inside one bucket.
///
/// kbf names every key itself, so keys are restricted to characters that need no
/// escaping anywhere: ASCII letters, digits, `-`, `_`, `.`, `~` and `/` as a separator.
/// Segments may not be empty, `.` or `..`, so no URL normalisation between kbf and the
/// store can turn one key into another. That rules out a whole class of signing and
/// encoding mismatches with S3 servers.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ObjectKey(String);

/// A key prefix for [`ObjectStore::list`](crate::ObjectStore::list): empty, or the
/// characters of a key (see [`ObjectKey`]), optionally ending in `/`.
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct KeyPrefix(String);

/// Why a key or prefix was refused.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum KeyError {
    /// The key is empty.
    #[error("an object key may not be empty")]
    Empty,
    /// The key is longer than S3 allows.
    #[error("object key has {0} bytes; at most 1024 are allowed")]
    TooLong(usize),
    /// The key holds a character outside `A-Z a-z 0-9 - _ . ~ /`.
    #[error("object key {key:?} has {ch:?}; only A-Z a-z 0-9 - _ . ~ / are allowed")]
    Character {
        /// The refused key.
        key: String,
        /// The first refused character.
        ch: char,
    },
    /// A `/`-separated segment is empty, `.` or `..`.
    #[error("object key {0:?} has an empty, `.` or `..` segment")]
    Segment(String),
}

impl ObjectKey {
    /// Checks and wraps a key.
    pub fn new(key: impl Into<String>) -> Result<Self, KeyError> {
        let key = key.into();
        if key.is_empty() {
            return Err(KeyError::Empty);
        }
        check_chars(&key)?;
        if key
            .split('/')
            .any(|s| s.is_empty() || s == "." || s == "..")
        {
            return Err(KeyError::Segment(key));
        }
        Ok(Self(key))
    }

    /// The key as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ObjectKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl KeyPrefix {
    /// Checks and wraps a prefix. The empty prefix lists the whole bucket.
    pub fn new(prefix: impl Into<String>) -> Result<Self, KeyError> {
        let prefix = prefix.into();
        if prefix.is_empty() {
            return Ok(Self(prefix));
        }
        // A prefix ending in `/` is a key with a trailing separator; check the rest as
        // a key so that any key the prefix can produce is itself valid.
        ObjectKey::new(prefix.strip_suffix('/').unwrap_or(&prefix))?;
        Ok(Self(prefix))
    }

    /// The prefix as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The key `<prefix><name>`.
    pub fn key(&self, name: &str) -> Result<ObjectKey, KeyError> {
        ObjectKey::new(format!("{}{name}", self.0))
    }

    /// The prefix `<prefix><name>`.
    pub fn child(&self, name: &str) -> Result<KeyPrefix, KeyError> {
        KeyPrefix::new(format!("{}{name}", self.0))
    }

    /// Whether `key` starts with this prefix.
    #[must_use]
    pub fn matches(&self, key: &ObjectKey) -> bool {
        key.0.starts_with(&self.0)
    }
}

fn check_chars(key: &str) -> Result<(), KeyError> {
    if key.len() > MAX_KEY_BYTES {
        return Err(KeyError::TooLong(key.len()));
    }
    if let Some(ch) = key
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~' | '/')))
    {
        return Err(KeyError::Character {
            key: key.to_owned(),
            ch,
        });
    }
    Ok(())
}

/// A byte range to read: `len` bytes from `offset`.
///
/// A read returns the bytes of `[offset, offset + len)` that exist: a range running past
/// the end of the object is cut at the end, as S3 does. A range starting at or past the
/// end is [`ObjectStoreError::InvalidRange`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ByteRange {
    offset: u64,
    len: u64,
}

impl ByteRange {
    /// The range `[offset, offset + len)`, or `None` if `len` is zero or the end
    /// overflows `u64`.
    #[must_use]
    pub fn new(offset: u64, len: u64) -> Option<Self> {
        (len > 0 && offset.checked_add(len).is_some()).then_some(Self { offset, len })
    }

    /// The first byte.
    #[must_use]
    pub fn offset(self) -> u64 {
        self.offset
    }

    /// The number of bytes asked for.
    #[must_use]
    pub fn size(self) -> u64 {
        self.len
    }

    /// The last byte asked for (inclusive), as an HTTP `Range` names it.
    #[must_use]
    pub fn last(self) -> u64 {
        self.offset + self.len - 1
    }
}

/// How many keys one [`ObjectStore::list`](crate::ObjectStore::list) call may return:
/// 1 to 1000, S3's page limit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PageSize(u16);

impl PageSize {
    /// The largest page S3 serves.
    pub const MAX: PageSize = PageSize(1000);

    /// A page size of `n`, or `None` outside 1 to 1000.
    #[must_use]
    pub fn new(n: u16) -> Option<Self> {
        (1..=1000).contains(&n).then_some(Self(n))
    }

    /// The size as a number.
    #[must_use]
    pub fn get(self) -> u16 {
        self.0
    }
}

/// Where the next [`ObjectStore::list`](crate::ObjectStore::list) page starts. Opaque:
/// only the store that issued it can read it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListToken(pub(crate) String);

/// One object in a list page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObjectInfo {
    /// The object's key.
    pub key: ObjectKey,
    /// The object's size in bytes.
    pub size: u64,
}

/// One page of a listing, in ascending key order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListPage {
    /// The objects on this page.
    pub objects: Vec<ObjectInfo>,
    /// Where the next page starts, or `None` if this is the last page.
    pub next: Option<ListToken>,
}

/// What a store says it can do. The conformance suite checks every claim.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Capabilities {
    /// `put_new` refuses to replace an existing key ([`ObjectStoreError::AlreadyExists`]).
    /// Without it, `put_new` on an existing key replaces it; kbf names keys so that this
    /// never changes the bytes, but the claim is checked when made.
    pub conditional_put: bool,
    /// Object Lock: `put_new` with a retention date makes an object that `delete`
    /// refuses until that date. Required of the store that holds `audit` and of no other.
    pub object_lock: bool,
}

/// Errors from an [`ObjectStore`](crate::ObjectStore).
#[derive(Debug, thiserror::Error)]
pub enum ObjectStoreError {
    /// No object has this key.
    #[error("object {0} not found")]
    NotFound(ObjectKey),
    /// `put_new` on a key that exists, on a store with conditional writes.
    #[error("object {0} already exists")]
    AlreadyExists(ObjectKey),
    /// The range starts at or past the end of the object.
    #[error("range {range:?} starts past the end of object {key}")]
    InvalidRange {
        /// The object read.
        key: ObjectKey,
        /// The range asked for.
        range: ByteRange,
    },
    /// The object is under Object Lock retention and may not be deleted or replaced yet.
    #[error("object {0} is under retention")]
    Locked(ObjectKey),
    /// The request needs a capability the store does not claim (for example a retention
    /// date on a store without Object Lock). Refused rather than silently weakened.
    #[error("the store does not support {0}")]
    Unsupported(&'static str),
    /// The store answered with an S3 error.
    #[error("store answered {status} {code}: {message}")]
    Service {
        /// The HTTP status.
        status: u16,
        /// The S3 error code (`AccessDenied`, `NoSuchBucket`, ...), or empty.
        code: String,
        /// The store's message.
        message: String,
    },
    /// The store answered something that breaks the S3 contract kbf relies on, such as
    /// the whole object for a ranged read. Never retried into success.
    #[error("store broke the protocol: {0}")]
    Protocol(String),
    /// The request did not complete (connect, timeout, reset).
    #[error("request to the store failed")]
    Transport(#[source] Box<dyn std::error::Error + Send + Sync>),
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches: a key check that lets through a character needing URL escaping, an
    /// empty or dot segment, or an over-long key; any of these lets kbf's key and the
    /// key the store signs or stores drift apart.
    #[test]
    fn keys_refuse_what_needs_escaping_or_normalising() {
        for good in ["a", "cas/ab/cd.seg", "x-y_z.~1", "a/b/c"] {
            assert!(ObjectKey::new(good).is_ok(), "{good}");
        }
        assert_eq!(ObjectKey::new(""), Err(KeyError::Empty));
        for bad in ["a b", "a%2f", "a+b", "é", "a?b", "a#b"] {
            assert!(
                matches!(ObjectKey::new(bad), Err(KeyError::Character { .. })),
                "{bad}"
            );
        }
        for bad in ["/a", "a/", "a//b", "a/./b", "a/../b", ".", ".."] {
            assert!(
                matches!(ObjectKey::new(bad), Err(KeyError::Segment(_))),
                "{bad}"
            );
        }
        assert_eq!(
            ObjectKey::new("a".repeat(1025)),
            Err(KeyError::TooLong(1025))
        );
        assert!(ObjectKey::new("a".repeat(1024)).is_ok());
    }

    /// Catches: a prefix that can produce invalid keys, or `matches` testing something
    /// other than a string prefix (a prefix `p/` must not match `p-other`).
    #[test]
    fn prefixes_join_and_match() {
        let p = KeyPrefix::new("run/1/").unwrap();
        assert_eq!(p.key("x").unwrap().as_str(), "run/1/x");
        assert!(p.matches(&ObjectKey::new("run/1/x").unwrap()));
        assert!(!p.matches(&ObjectKey::new("run/10").unwrap()));
        assert!(KeyPrefix::new("").is_ok());
        assert!(KeyPrefix::new("/").is_err());
        assert!(KeyPrefix::new("a//").is_err());
    }

    /// Catches: a zero-length or overflowing range, or an off-by-one in the inclusive
    /// end that an HTTP `Range` header carries.
    #[test]
    fn ranges() {
        assert_eq!(ByteRange::new(0, 0), None);
        assert_eq!(ByteRange::new(u64::MAX, 1), None);
        let r = ByteRange::new(10, 5).unwrap();
        assert_eq!((r.offset(), r.size(), r.last()), (10, 5, 14));
        assert_eq!(PageSize::new(0), None);
        assert_eq!(PageSize::new(1001), None);
        assert_eq!(PageSize::new(1000), Some(PageSize::MAX));
    }
}
