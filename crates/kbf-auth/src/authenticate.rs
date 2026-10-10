//! Authenticators: who is calling.

use std::sync::Arc;

use tonic::codegen::http::request::Parts;
use tonic::{Code, Status};

use crate::BoxFuture;
use crate::metadata::AuthenticationMetadata;

/// Answers who made a call, once per call, before the call reaches a service:
/// Buildbarn's `Authenticator`.
pub trait Authenticator: Send + Sync + 'static {
    /// The caller's metadata, from one call's headers and extensions (peer
    /// certificates and connection info are in the extensions).
    ///
    /// # Errors
    /// UNAUTHENTICATED when the call does not show who it is from, or shows it wrongly;
    /// another code when the authenticator itself cannot answer (a backend it needs is
    /// down), which [`AnyAuthenticator`] does not hide behind UNAUTHENTICATED.
    fn authenticate<'a>(
        &'a self,
        call: &'a Parts,
    ) -> BoxFuture<'a, Result<Arc<AuthenticationMetadata>, Status>>;
}

/// Accepts every call, with fixed metadata: Buildbarn's `allow`.
#[derive(Debug, Default)]
pub struct AllowAuthenticator(Arc<AuthenticationMetadata>);

impl AllowAuthenticator {
    /// Accepts every call as `metadata`.
    #[must_use]
    pub fn new(metadata: AuthenticationMetadata) -> Self {
        Self(Arc::new(metadata))
    }
}

impl Authenticator for AllowAuthenticator {
    fn authenticate<'a>(
        &'a self,
        _call: &'a Parts,
    ) -> BoxFuture<'a, Result<Arc<AuthenticationMetadata>, Status>> {
        Box::pin(std::future::ready(Ok(Arc::clone(&self.0))))
    }
}

/// Refuses every call UNAUTHENTICATED with a fixed message: Buildbarn's `deny`.
#[derive(Debug)]
pub struct DenyAuthenticator(String);

impl DenyAuthenticator {
    /// Refuses every call with `message`.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl Authenticator for DenyAuthenticator {
    fn authenticate<'a>(
        &'a self,
        _call: &'a Parts,
    ) -> BoxFuture<'a, Result<Arc<AuthenticationMetadata>, Status>> {
        Box::pin(std::future::ready(Err(Status::unauthenticated(
            self.0.clone(),
        ))))
    }
}

/// Accepts a call when one of its children does: Buildbarn's `any`.
///
/// The children are asked in order and the first that accepts answers; the rest are
/// not asked. When every child refuses, the call fails with the first error whose code
/// is not UNAUTHENTICATED, if there was one (so a backend failure is not hidden behind
/// "who are you"); otherwise UNAUTHENTICATED with every child's message, in order,
/// joined by `", "`. With no children every call is refused.
pub struct AnyAuthenticator(Vec<Arc<dyn Authenticator>>);

impl AnyAuthenticator {
    /// Accepts a call when one of `children` does.
    #[must_use]
    pub fn new(children: Vec<Arc<dyn Authenticator>>) -> Self {
        Self(children)
    }
}

impl Authenticator for AnyAuthenticator {
    fn authenticate<'a>(
        &'a self,
        call: &'a Parts,
    ) -> BoxFuture<'a, Result<Arc<AuthenticationMetadata>, Status>> {
        Box::pin(async move {
            let mut unauthenticated = Vec::new();
            let mut other: Option<Status> = None;
            for child in &self.0 {
                match child.authenticate(call).await {
                    Ok(metadata) => return Ok(metadata),
                    Err(e) if e.code() == Code::Unauthenticated => {
                        unauthenticated.push(e.message().to_owned());
                    }
                    Err(e) => {
                        other.get_or_insert(e);
                    }
                }
            }
            if let Some(e) = other {
                return Err(e);
            }
            if unauthenticated.is_empty() {
                return Err(Status::unauthenticated(
                    "no authentication policy is configured",
                ));
            }
            Err(Status::unauthenticated(unauthenticated.join(", ")))
        })
    }
}

/// Accepts a call when every child does, with their metadata merged in order
/// ([`AuthenticationMetadata::merge`]): Buildbarn's `all`. The children are asked in
/// order and the first refusal is the call's; the rest are not asked.
pub struct AllAuthenticator(Vec<Arc<dyn Authenticator>>);

impl AllAuthenticator {
    /// Accepts a call when every one of `children` does.
    #[must_use]
    pub fn new(children: Vec<Arc<dyn Authenticator>>) -> Self {
        Self(children)
    }
}

impl Authenticator for AllAuthenticator {
    fn authenticate<'a>(
        &'a self,
        call: &'a Parts,
    ) -> BoxFuture<'a, Result<Arc<AuthenticationMetadata>, Status>> {
        Box::pin(async move {
            let mut merged = AuthenticationMetadata::default();
            for child in &self.0 {
                merged = merged.merge(&*child.authenticate(call).await?);
            }
            Ok(Arc::new(merged))
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use serde_json::{Value, json};
    use tonic::codegen::http;

    use super::*;

    fn call() -> Parts {
        http::Request::new(()).into_parts().0
    }

    fn allow(public: Value) -> Arc<dyn Authenticator> {
        Arc::new(AllowAuthenticator::new(AuthenticationMetadata::new(
            Some(public),
            None,
        )))
    }

    fn deny(message: &str) -> Arc<dyn Authenticator> {
        Arc::new(DenyAuthenticator::new(message))
    }

    /// Fails with a fixed status and counts how often it is asked.
    struct Failing(Code, AtomicUsize);

    impl Authenticator for Failing {
        fn authenticate<'a>(
            &'a self,
            _call: &'a Parts,
        ) -> BoxFuture<'a, Result<Arc<AuthenticationMetadata>, Status>> {
            self.1.fetch_add(1, Ordering::SeqCst);
            Box::pin(std::future::ready(Err(Status::new(self.0, "backend down"))))
        }
    }

    async fn run(a: &dyn Authenticator) -> Result<Arc<AuthenticationMetadata>, Status> {
        a.authenticate(&call()).await
    }

    /// Catches: a deny that accepts (mutant "deny allows"), or refuses with a code
    /// other than UNAUTHENTICATED or without its configured message.
    #[tokio::test]
    async fn deny_refuses_unauthenticated_with_its_message() {
        let e = run(&*deny("go away")).await.expect_err("denied");
        assert_eq!(e.code(), Code::Unauthenticated);
        assert_eq!(e.message(), "go away");
    }

    /// Catches: an `any` that behaves as `all` (mutant: one refusing child refuses the
    /// call), one that asks children after the first that accepts, and one that
    /// answers with a later child's metadata.
    #[tokio::test]
    async fn any_takes_the_first_child_that_accepts() {
        let after = Arc::new(Failing(Code::Unavailable, AtomicUsize::new(0)));
        let any = AnyAuthenticator::new(vec![
            deny("no token"),
            allow(json!("first")),
            allow(json!("second")),
            after.clone(),
        ]);
        let md = run(&any).await.expect("accepted");
        assert_eq!(md.public(), Some(&json!("first")));
        assert_eq!(after.1.load(Ordering::SeqCst), 0, "asked after an accept");
    }

    /// Catches: an `any` that hides a backend failure behind UNAUTHENTICATED, one that
    /// keeps the last such failure instead of the first, and one that, when every
    /// child said UNAUTHENTICATED, keeps only one child's message.
    #[tokio::test]
    async fn any_refusal_prefers_the_first_non_unauthenticated_error() {
        let any = AnyAuthenticator::new(vec![
            deny("no token"),
            Arc::new(Failing(Code::Unavailable, AtomicUsize::new(0))),
            Arc::new(Failing(Code::Internal, AtomicUsize::new(0))),
        ]);
        let e = run(&any).await.expect_err("refused");
        assert_eq!(e.code(), Code::Unavailable);

        let any = AnyAuthenticator::new(vec![deny("no token"), deny("no certificate")]);
        let e = run(&any).await.expect_err("refused");
        assert_eq!(e.code(), Code::Unauthenticated);
        assert_eq!(e.message(), "no token, no certificate");

        let e = run(&AnyAuthenticator::new(Vec::new()))
            .await
            .expect_err("no child accepts");
        assert_eq!(e.code(), Code::Unauthenticated);
    }

    /// Catches: an `all` that behaves as `any` (one accepting child is enough), one
    /// that asks children after a refusal, and one that merges in the wrong order.
    #[tokio::test]
    async fn all_needs_every_child_and_merges_in_order() {
        let all = AllAuthenticator::new(vec![
            allow(json!({"user": "a", "team": "x"})),
            allow(json!({"user": "b"})),
        ]);
        let md = run(&all).await.expect("accepted");
        assert_eq!(md.public(), Some(&json!({"user": "b", "team": "x"})));

        let after = Arc::new(Failing(Code::Unavailable, AtomicUsize::new(0)));
        let all = AllAuthenticator::new(vec![allow(json!("a")), deny("no token"), after.clone()]);
        let e = run(&all).await.expect_err("refused");
        assert_eq!((e.code(), e.message()), (Code::Unauthenticated, "no token"));
        assert_eq!(after.1.load(Ordering::SeqCst), 0, "asked after a refusal");
    }
}
