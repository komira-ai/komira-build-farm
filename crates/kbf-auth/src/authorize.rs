//! Authorizers: whether a caller may do a kind of call on an instance name.

use std::fmt;
use std::sync::Arc;

use tonic::Status;

use crate::BoxFuture;
use crate::metadata::AuthenticationMetadata;

/// Whether `metadata` may make a kind of call on each of some instance names:
/// Buildbarn's `Authorizer`.
pub trait Authorizer: Send + Sync + 'static {
    /// One answer per instance name, in order: `Ok` allows it. It may wait (a remote
    /// authorizer will), so a caller must not hold a contended lock across it.
    fn authorize<'a>(
        &'a self,
        metadata: &'a AuthenticationMetadata,
        instance_names: &'a [&'a str],
    ) -> BoxFuture<'a, Vec<Result<(), Status>>>;
}

/// The refusal of a static authorizer, Buildbarn's.
fn permission_denied() -> Status {
    Status::permission_denied("Permission denied")
}

/// Allows everything: Buildbarn's `allow`.
#[derive(Debug)]
pub struct AllowAuthorizer;

impl Authorizer for AllowAuthorizer {
    fn authorize<'a>(
        &'a self,
        _metadata: &'a AuthenticationMetadata,
        instance_names: &'a [&'a str],
    ) -> BoxFuture<'a, Vec<Result<(), Status>>> {
        Box::pin(std::future::ready(vec![Ok(()); instance_names.len()]))
    }
}

/// Refuses everything PERMISSION_DENIED: Buildbarn's `deny`.
#[derive(Debug)]
pub struct DenyAuthorizer;

impl Authorizer for DenyAuthorizer {
    fn authorize<'a>(
        &'a self,
        _metadata: &'a AuthenticationMetadata,
        instance_names: &'a [&'a str],
    ) -> BoxFuture<'a, Vec<Result<(), Status>>> {
        let refused = instance_names.iter().map(|_| Err(permission_denied()));
        Box::pin(std::future::ready(refused.collect()))
    }
}

/// Allows an instance name that one of its prefixes is a prefix of, whole path
/// components at a time: Buildbarn's `instanceNamePrefix`. `a` allows `a` and `a/b`
/// but not `ab`; the empty prefix allows every name, the empty one included. Whoever
/// calls is not looked at.
#[derive(Debug)]
pub struct InstanceNamePrefixAuthorizer {
    prefixes: Vec<String>,
}

/// Why an instance-name prefix is refused.
#[derive(Debug, thiserror::Error)]
#[error(
    "instance name prefix {prefix:?} has an empty path component (a leading, trailing or \
     doubled `/`)"
)]
pub struct PrefixError {
    /// The prefix.
    pub prefix: String,
}

impl InstanceNamePrefixAuthorizer {
    /// Allows the names under any of `prefixes`.
    ///
    /// # Errors
    /// A prefix other than the empty one has an empty path component.
    pub fn new(prefixes: Vec<String>) -> Result<Self, PrefixError> {
        if let Some(bad) = prefixes
            .iter()
            .find(|p| !p.is_empty() && p.split('/').any(str::is_empty))
        {
            return Err(PrefixError {
                prefix: bad.clone(),
            });
        }
        Ok(Self { prefixes })
    }

    /// Whether `name` is under one of the prefixes.
    #[must_use]
    pub fn allows(&self, name: &str) -> bool {
        self.prefixes.iter().any(|p| under(name, p))
    }
}

/// Whether `prefix` is `name`'s first whole path components.
fn under(name: &str, prefix: &str) -> bool {
    prefix.is_empty()
        || name
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

impl Authorizer for InstanceNamePrefixAuthorizer {
    fn authorize<'a>(
        &'a self,
        _metadata: &'a AuthenticationMetadata,
        instance_names: &'a [&'a str],
    ) -> BoxFuture<'a, Vec<Result<(), Status>>> {
        let answers = instance_names.iter().map(|name| {
            if self.allows(name) {
                Ok(())
            } else {
                Err(permission_denied())
            }
        });
        Box::pin(std::future::ready(answers.collect()))
    }
}

/// One authorizer per kind of REAPI call. Which calls each one covers is in
/// `docs/reapi-auth.md`; in short:
///
/// - `capabilities`: GetCapabilities.
/// - `cas_get`: reads of the CAS (BatchReadBlobs, GetTree, ByteStream Read, SplitBlob,
///   GetChunkMapping).
/// - `cas_put`: writes to the CAS (BatchUpdateBlobs, ByteStream Write and
///   QueryWriteStatus, SpliceBlob, RegisterChunkMapping).
/// - `cas_find_missing`: FindMissingBlobs.
/// - `ac_get`: GetActionResult. UpdateActionResult has no authorizer: clients never
///   write the action cache, whatever the policy.
/// - `execute`: Execute, and WaitExecution against the operation's instance name.
pub struct Authorizers {
    /// GetCapabilities.
    pub capabilities: Arc<dyn Authorizer>,
    /// Reads of the CAS.
    pub cas_get: Arc<dyn Authorizer>,
    /// Writes to the CAS.
    pub cas_put: Arc<dyn Authorizer>,
    /// FindMissingBlobs.
    pub cas_find_missing: Arc<dyn Authorizer>,
    /// GetActionResult.
    pub ac_get: Arc<dyn Authorizer>,
    /// Execute and WaitExecution.
    pub execute: Arc<dyn Authorizer>,
}

impl fmt::Debug for Authorizers {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Authorizers").finish_non_exhaustive()
    }
}

impl Authorizers {
    /// Every authorizer allows: what a server with no policy runs.
    #[must_use]
    pub fn allow_all() -> Self {
        let allow: Arc<dyn Authorizer> = Arc::new(AllowAuthorizer);
        Self {
            capabilities: Arc::clone(&allow),
            cas_get: Arc::clone(&allow),
            cas_put: Arc::clone(&allow),
            cas_find_missing: Arc::clone(&allow),
            ac_get: Arc::clone(&allow),
            execute: allow,
        }
    }
}

/// Whether `metadata` may make `call` on `instance`, asking `authorizer` (Buildbarn's
/// `AuthorizeSingleInstanceName`). A refusal keeps the authorizer's code, its message
/// prefixed `Authorization: ` (Buildbarn's wrap), and is logged with the call, the
/// instance name and the caller's public metadata, never the private part.
///
/// # Errors
/// The authorizer refuses, or answers for no instance name (refused INTERNAL: an
/// authorizer that says nothing allows nothing).
pub async fn authorize(
    authorizer: &dyn Authorizer,
    metadata: &AuthenticationMetadata,
    call: &'static str,
    instance: &str,
) -> Result<(), Status> {
    let answer = authorizer
        .authorize(metadata, &[instance])
        .await
        .into_iter()
        .next()
        .unwrap_or_else(|| Err(Status::internal("the authorizer gave no answer")));
    answer.map_err(|e| {
        tracing::warn!(
            call,
            instance,
            public = %metadata.public_display(),
            error = %e.message(),
            "REAPI call refused"
        );
        Status::new(e.code(), format!("Authorization: {}", e.message()))
    })
}

#[cfg(test)]
mod tests {
    use tonic::Code;

    use super::*;

    fn prefixes(p: &[&str]) -> InstanceNamePrefixAuthorizer {
        InstanceNamePrefixAuthorizer::new(p.iter().map(|s| (*s).to_owned()).collect())
            .expect("valid prefixes")
    }

    /// Catches: a byte-prefix match (`a` allowing `ab`), an empty prefix that allows
    /// only the empty name, and a prefix that does not allow itself.
    #[test]
    fn prefixes_match_whole_path_components() {
        let a = prefixes(&["a", "x/y"]);
        for allowed in ["a", "a/b", "a/b/c", "x/y", "x/y/z"] {
            assert!(a.allows(allowed), "{allowed}");
        }
        for refused in ["ab", "", "b", "x", "x/yz", "b/a"] {
            assert!(!a.allows(refused), "{refused}");
        }
        let all = prefixes(&[""]);
        for name in ["", "a", "a/b"] {
            assert!(all.allows(name), "{name}");
        }
        assert!(!prefixes(&[]).allows(""));
    }

    /// Catches: a prefix with an empty component accepted, which could never match a
    /// well-formed name and so silently allows less than it reads.
    #[test]
    fn a_prefix_with_an_empty_component_is_refused() {
        for bad in ["/a", "a/", "a//b", "/"] {
            let e = InstanceNamePrefixAuthorizer::new(vec![bad.to_owned()]).expect_err(bad);
            assert_eq!(e.prefix, bad);
            assert!(e.to_string().contains("empty path component"), "{e}");
        }
    }

    /// Catches: a deny that allows (mutant "deny allows"), answers that are not one
    /// per instance name, and a refusal not wrapped as Buildbarn wraps it.
    #[tokio::test]
    async fn authorize_wraps_a_refusal_and_keeps_its_code() {
        let md = AuthenticationMetadata::default();
        assert_eq!(DenyAuthorizer.authorize(&md, &["a", "b"]).await.len(), 2);
        assert!(matches!(
            AllowAuthorizer.authorize(&md, &["a", "b"]).await.as_slice(),
            [Ok(()), Ok(())]
        ));
        let e = authorize(&DenyAuthorizer, &md, "Execute", "a")
            .await
            .expect_err("denied");
        assert_eq!(e.code(), Code::PermissionDenied);
        assert_eq!(e.message(), "Authorization: Permission denied");
        authorize(&AllowAuthorizer, &md, "Execute", "a")
            .await
            .expect("allowed");
        let p = prefixes(&["ci"]);
        authorize(&p, &md, "Execute", "ci/main")
            .await
            .expect("under ci");
        let e = authorize(&p, &md, "Execute", "cid")
            .await
            .expect_err("not ci");
        assert_eq!(e.code(), Code::PermissionDenied);
    }

    /// Answers for no instance name at all.
    struct Silent;

    impl Authorizer for Silent {
        fn authorize<'a>(
            &'a self,
            _metadata: &'a AuthenticationMetadata,
            _instance_names: &'a [&'a str],
        ) -> BoxFuture<'a, Vec<Result<(), Status>>> {
            Box::pin(std::future::ready(Vec::new()))
        }
    }

    /// Catches: an authorizer that gives no answer read as allowing.
    #[tokio::test]
    async fn an_authorizer_that_says_nothing_allows_nothing() {
        let e = authorize(&Silent, &AuthenticationMetadata::default(), "Execute", "a")
            .await
            .expect_err("no answer");
        assert_eq!(e.code(), Code::Internal);
        assert!(format!("{:?}", Authorizers::allow_all()).starts_with("Authorizers"));
    }
}
