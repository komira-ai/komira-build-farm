//! What authentication found out about a caller.

use std::fmt;
use std::sync::{Arc, LazyLock};

use serde_json::Value;

/// Who a caller is, as an [`crate::Authenticator`] found: Buildbarn's
/// `AuthenticationMetadata`, without its tracing attributes (not built yet).
///
/// - `public` may be shown: kbf logs it when it refuses a call and puts it on the
///   Execute trace.
/// - `private` is for authorizers only and is never logged; `Debug` leaves it out.
///
/// Both are any JSON value, or absent.
#[derive(Default, PartialEq)]
pub struct AuthenticationMetadata {
    public: Option<Value>,
    private: Option<Value>,
}

/// The metadata of a call no authenticator ran on.
static EMPTY: LazyLock<Arc<AuthenticationMetadata>> = LazyLock::new(Arc::default);

impl fmt::Debug for AuthenticationMetadata {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuthenticationMetadata")
            .field("public", &self.public)
            .finish_non_exhaustive()
    }
}

impl AuthenticationMetadata {
    /// Metadata with these parts.
    #[must_use]
    pub const fn new(public: Option<Value>, private: Option<Value>) -> Self {
        Self { public, private }
    }

    /// The part that may be shown.
    #[must_use]
    pub const fn public(&self) -> Option<&Value> {
        self.public.as_ref()
    }

    /// The part only authorizers read.
    #[must_use]
    pub const fn private(&self) -> Option<&Value> {
        self.private.as_ref()
    }

    /// The public part as one line of JSON, `-` when there is none: how logs and the
    /// Execute trace show the caller.
    #[must_use]
    pub fn public_display(&self) -> String {
        self.public
            .as_ref()
            .map_or_else(|| "-".to_owned(), Value::to_string)
    }

    /// `self` with `later` merged over it, as Buildbarn's `all` merges its children's
    /// metadata in order: for each part, when both are JSON objects their keys are
    /// merged and `later`'s value wins for a key both have (one level deep, not
    /// recursively); otherwise `later`'s part replaces `self`'s when it has one.
    #[must_use]
    pub fn merge(self, later: &Self) -> Self {
        Self {
            public: merge_part(self.public, later.public.as_ref()),
            private: merge_part(self.private, later.private.as_ref()),
        }
    }
}

fn merge_part(earlier: Option<Value>, later: Option<&Value>) -> Option<Value> {
    match (earlier, later) {
        (Some(Value::Object(mut earlier)), Some(Value::Object(later))) => {
            for (key, value) in later {
                earlier.insert(key.clone(), value.clone());
            }
            Some(Value::Object(earlier))
        }
        (earlier, None) => earlier,
        (_, Some(later)) => Some(later.clone()),
    }
}

/// The metadata [`crate::AuthenticateLayer`] put on `request`, or empty metadata when
/// no layer ran (as Buildbarn's `AuthenticationMetadataFromContext` returns an empty
/// default). An authorizer that denies still denies on empty metadata.
#[must_use]
pub fn metadata<T>(request: &tonic::Request<T>) -> Arc<AuthenticationMetadata> {
    request
        .extensions()
        .get::<Arc<AuthenticationMetadata>>()
        .map_or_else(|| Arc::clone(&EMPTY), Arc::clone)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn md(public: Value, private: Value) -> AuthenticationMetadata {
        AuthenticationMetadata::new(Some(public), Some(private))
    }

    /// Catches: a merge where the earlier value wins a shared key, one that drops keys
    /// only the earlier side has, one that merges recursively, and one where an absent
    /// later part erases the earlier one.
    #[test]
    fn a_later_part_wins_key_by_key_one_level_deep() {
        let first = md(
            json!({"user": "a", "team": "x", "deep": {"k": 1, "j": 2}}),
            json!("secret"),
        );
        let second = md(json!({"user": "b", "deep": {"k": 3}}), json!({"p": 1}));
        let merged = first.merge(&second);
        assert_eq!(
            merged.public(),
            Some(&json!({"user": "b", "team": "x", "deep": {"k": 3}}))
        );
        assert_eq!(merged.private(), Some(&json!({"p": 1})));

        let kept = md(json!({"user": "a"}), json!(1)).merge(&AuthenticationMetadata::default());
        assert_eq!(kept.public(), Some(&json!({"user": "a"})));
        assert_eq!(kept.private(), Some(&json!(1)));
    }

    /// Catches: a `Debug` that prints the private part, which would put it in any log
    /// line that formats the metadata.
    #[test]
    fn debug_shows_the_public_part_only() {
        let shown = format!("{:?}", md(json!({"user": "a"}), json!("hunter2")));
        assert!(shown.contains("\"user\""), "{shown}");
        assert!(!shown.contains("hunter2"), "{shown}");
        assert_eq!(AuthenticationMetadata::default().public_display(), "-");
        assert_eq!(
            md(json!({"user": "a"}), json!(0)).public_display(),
            r#"{"user":"a"}"#
        );
    }

    /// Catches: a request no layer ran on that reads as anything but empty metadata,
    /// and one with metadata that reads as empty.
    #[test]
    fn a_request_without_metadata_reads_as_empty() {
        let bare = tonic::Request::new(());
        assert_eq!(*metadata(&bare), AuthenticationMetadata::default());
        let mut carried = tonic::Request::new(());
        let md = Arc::new(md(json!("u"), json!("p")));
        carried.extensions_mut().insert(Arc::clone(&md));
        assert_eq!(metadata(&carried), md);
    }
}
