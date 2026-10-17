//! The release service over the node's mutual-TLS channel to the frontend.

use futures::StreamExt as _;
use futures::future::BoxFuture;
use kbf_proto::update::FetchRequest;
use kbf_proto::update::release_service_client::ReleaseServiceClient;
use tonic::Code;
use tonic::transport::Channel;

use crate::source::{Chunks, FetchError, Source};

/// A [`Source`] that asks `ReleaseService.Fetch` on the frontend.
#[derive(Clone)]
pub struct ReleaseClient {
    client: ReleaseServiceClient<Channel>,
}

impl ReleaseClient {
    /// A client over `channel`, which connects when it is first used.
    #[must_use]
    pub fn new(channel: Channel) -> Self {
        Self {
            client: ReleaseServiceClient::new(channel),
        }
    }
}

/// `NOT_FOUND` and `INVALID_ARGUMENT` say the frontend will never serve this digest;
/// anything else (an unavailable frontend, a reset connection) may pass.
fn failure(status: &tonic::Status) -> FetchError {
    let why = format!("{}: {}", status.code(), status.message());
    match status.code() {
        Code::NotFound | Code::InvalidArgument => FetchError::Missing(why),
        _ => FetchError::Transport(why),
    }
}

impl Source for ReleaseClient {
    fn fetch(&self, sha256: &str) -> BoxFuture<'_, Result<Chunks, FetchError>> {
        let request = FetchRequest {
            sha256: sha256.to_owned(),
        };
        let mut client = self.client.clone();
        Box::pin(async move {
            let response = client.fetch(request).await.map_err(|s| failure(&s))?;
            let chunks = response
                .into_inner()
                .map(|chunk| chunk.map(|c| c.data).map_err(|s| failure(&s)));
            Ok(Box::pin(chunks) as Chunks)
        })
    }
}

#[cfg(test)]
mod tests {
    use std::pin::Pin;

    use futures::Stream;
    use futures::stream;
    use kbf_proto::update::release_service_server::{ReleaseService, ReleaseServiceServer};
    use kbf_proto::update::{FetchChunk, FetchRequest};
    use tonic::transport::server::TcpIncoming;
    use tonic::transport::{Endpoint, Server};
    use tonic::{Request, Response, Status};

    use super::*;

    /// A release service that answers by the digest it is asked for.
    struct Fake;

    type Fetched = Pin<Box<dyn Stream<Item = Result<FetchChunk, Status>> + Send>>;

    #[tonic::async_trait]
    impl ReleaseService for Fake {
        type FetchStream = Fetched;

        async fn fetch(&self, request: Request<FetchRequest>) -> Result<Response<Fetched>, Status> {
            let chunk = |data: &[u8]| {
                Ok(FetchChunk {
                    data: data.to_vec(),
                })
            };
            match request.into_inner().sha256.as_str() {
                "ok" => Ok(Response::new(Box::pin(stream::iter(vec![
                    chunk(b"ab"),
                    chunk(b"cde"),
                ])))),
                "missing" => Err(Status::not_found("no such file")),
                "invalid" => Err(Status::invalid_argument("not hex")),
                "down" => Err(Status::unavailable("restarting")),
                _ => Ok(Response::new(Box::pin(stream::iter(vec![
                    chunk(b"ab"),
                    Err(Status::internal("disk")),
                ])))),
            }
        }
    }

    async fn client() -> ReleaseClient {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let address = listener.local_addr().expect("address");
        let router = Server::builder().add_service(ReleaseServiceServer::new(Fake));
        // The future itself, not an async block: a line after its `await` would never run.
        tokio::spawn(router.serve_with_incoming(TcpIncoming::from(listener)));
        let channel = Endpoint::from_shared(format!("http://{address}"))
            .expect("url")
            .connect_lazy();
        ReleaseClient::new(channel)
    }

    /// Everything a fetch of `sha256` yields: the chunks, or the failure as its kind and
    /// text.
    async fn run(source: &ReleaseClient, sha256: &str) -> Vec<Result<Vec<u8>, String>> {
        let text = |e: FetchError| match e {
            FetchError::Missing(why) => format!("missing: {why}"),
            FetchError::Transport(why) => format!("transport: {why}"),
        };
        match source.fetch(sha256).await {
            Ok(chunks) => chunks.map(|c| c.map_err(text)).collect().await,
            Err(e) => vec![Err(text(e))],
        }
    }

    /// Catches a stream that is not handed on in order, each failure class mapped to the
    /// wrong kind (a digest the frontend lacks is permanent, an unavailable frontend is
    /// not), a failure in the middle of a stream lost, and a frontend that cannot be
    /// reached taken for anything but a transport error.
    #[tokio::test]
    async fn the_release_service_maps_chunks_and_failures() {
        let source = client().await;
        assert_eq!(
            run(&source, "ok").await,
            vec![Ok(b"ab".to_vec()), Ok(b"cde".to_vec())]
        );
        let missing = run(&source, "missing").await;
        assert_eq!(
            missing,
            vec![Err(
                "missing: Some requested entity was not found: no such file".to_owned()
            )]
        );
        let invalid = run(&source, "invalid").await;
        assert!(
            invalid[0].clone().unwrap_err().starts_with("missing: "),
            "{invalid:?}"
        );
        let down = run(&source, "down").await;
        assert!(
            down[0].clone().unwrap_err().ends_with(": restarting"),
            "{down:?}"
        );
        assert!(
            down[0].clone().unwrap_err().starts_with("transport: "),
            "{down:?}"
        );
        let broken = run(&source, "broken").await;
        assert_eq!(broken[0], Ok(b"ab".to_vec()));
        assert!(
            broken[1].clone().unwrap_err().starts_with("transport: "),
            "{broken:?}"
        );

        // Nothing listens.
        let closed = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let address = closed.local_addr().expect("address");
        drop(closed);
        let channel = Endpoint::from_shared(format!("http://{address}"))
            .expect("url")
            .connect_lazy();
        let gone = run(&ReleaseClient::new(channel), "ok").await;
        assert!(
            gone[0].clone().unwrap_err().starts_with("transport: "),
            "{gone:?}"
        );
    }
}
