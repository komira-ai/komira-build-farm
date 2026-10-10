//! The tower layer that authenticates every call of a tonic server.

use std::fmt;
use std::sync::Arc;
use std::task::{Context, Poll};

use tonic::codegen::{Service, http};
use tower_layer::Layer;

use crate::BoxFuture;
use crate::authenticate::Authenticator;

/// Runs an [`Authenticator`] on every call a tonic server takes, before routing:
/// Buildbarn's authenticating interceptor. Add it with
/// `Server::builder().layer(AuthenticateLayer::new(..))`; it applies to every service
/// of that server and to no other server.
///
/// A call is authenticated once, when it arrives (for a streaming call, once for the
/// whole stream, not per message). A refusal answers the call with the
/// authenticator's own status, and the service never sees it; it is logged with the
/// call's path. An accepted call carries its metadata to the service as an
/// `Arc<AuthenticationMetadata>` in the request's extensions, which
/// [`crate::metadata`] reads.
#[derive(Clone)]
pub struct AuthenticateLayer {
    authenticator: Arc<dyn Authenticator>,
}

impl AuthenticateLayer {
    /// A layer that asks `authenticator`.
    #[must_use]
    pub fn new(authenticator: Arc<dyn Authenticator>) -> Self {
        Self { authenticator }
    }
}

impl fmt::Debug for AuthenticateLayer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuthenticateLayer").finish_non_exhaustive()
    }
}

impl<S> Layer<S> for AuthenticateLayer {
    type Service = Authenticate<S>;

    fn layer(&self, inner: S) -> Authenticate<S> {
        Authenticate {
            inner,
            authenticator: Arc::clone(&self.authenticator),
        }
    }
}

/// The service [`AuthenticateLayer`] wraps around a server's routes.
#[derive(Clone)]
pub struct Authenticate<S> {
    inner: S,
    authenticator: Arc<dyn Authenticator>,
}

impl<S> fmt::Debug for Authenticate<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Authenticate").finish_non_exhaustive()
    }
}

impl<S, B, R> Service<http::Request<B>> for Authenticate<S>
where
    S: Service<http::Request<B>, Response = http::Response<R>> + Clone + Send + 'static,
    S::Future: Send,
    B: Send + 'static,
    R: Default,
{
    type Response = http::Response<R>;
    type Error = S::Error;
    type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: http::Request<B>) -> Self::Future {
        // The service polled ready is the one that takes the call; a clone stays.
        let ready = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, ready);
        let authenticator = Arc::clone(&self.authenticator);
        Box::pin(async move {
            let (mut parts, body) = request.into_parts();
            match authenticator.authenticate(&parts).await {
                Ok(metadata) => {
                    parts.extensions.insert(metadata);
                    inner.call(http::Request::from_parts(parts, body)).await
                }
                Err(status) => {
                    tracing::warn!(
                        call = parts.uri.path(),
                        error = %status.message(),
                        "REAPI call not authenticated"
                    );
                    Ok(status.into_http())
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;
    use std::sync::Mutex;

    use serde_json::json;
    use tonic::{Code, Status};

    use super::*;
    use crate::authenticate::{AllowAuthenticator, DenyAuthenticator};
    use crate::metadata::{AuthenticationMetadata, metadata};

    /// Records the public metadata of every call it serves, and answers 200 with a
    /// body of `served`.
    #[derive(Clone, Default)]
    struct Inner(Arc<Mutex<Vec<Option<serde_json::Value>>>>);

    impl Service<http::Request<()>> for Inner {
        type Response = http::Response<String>;
        type Error = Infallible;
        type Future = std::future::Ready<Result<Self::Response, Infallible>>;

        fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, request: http::Request<()>) -> Self::Future {
            let md = metadata(&tonic::Request::from_http(request));
            self.0.lock().expect("calls").push(md.public().cloned());
            std::future::ready(Ok(http::Response::new("served".to_owned())))
        }
    }

    async fn call(layer: &AuthenticateLayer, inner: &Inner) -> http::Response<String> {
        let mut service = layer.layer(inner.clone());
        std::future::poll_fn(|cx| service.poll_ready(cx))
            .await
            .expect("ready");
        let request = http::Request::builder()
            .uri("/pkg.Service/Method")
            .body(())
            .expect("request");
        service.call(request).await.expect("infallible")
    }

    /// Catches: a refused call that reaches the service, or that is answered with
    /// another status than the authenticator's; and an accepted call whose metadata
    /// does not reach the service.
    #[tokio::test]
    async fn a_refusal_never_reaches_the_service_and_an_accept_carries_its_metadata() {
        let inner = Inner::default();
        let deny = AuthenticateLayer::new(Arc::new(DenyAuthenticator::new("nobody")));
        let refused = call(&deny, &inner).await;
        let status = Status::from_header_map(refused.headers()).expect("a gRPC status");
        assert_eq!(
            (status.code(), status.message()),
            (Code::Unauthenticated, "nobody")
        );
        assert!(refused.body().is_empty());
        assert!(inner.0.lock().expect("calls").is_empty());

        let md = AuthenticationMetadata::new(Some(json!({"user": "ci"})), None);
        let allow = AuthenticateLayer::new(Arc::new(AllowAuthenticator::new(md)));
        let served = call(&allow, &inner).await;
        assert_eq!(served.body(), "served");
        assert_eq!(
            *inner.0.lock().expect("calls"),
            [Some(json!({"user": "ci"}))]
        );

        assert_eq!(format!("{allow:?}"), "AuthenticateLayer { .. }");
        assert_eq!(format!("{:?}", allow.layer(())), "Authenticate { .. }");
    }
}
