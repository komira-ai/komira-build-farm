//! `kbf.worker.v1.Segments`: the handlers of [`crate::Storage::segments`], each call
//! admitted first.

use kbf_proto::worker::segments_server::Segments;
use kbf_proto::worker::{
    BeginRequest, BeginResponse, CommitRequest, CommitResponse, LocateRequest, LocateResponse,
    PutRangeRequest, PutRangeResponse, ReadRangeRequest, ReadRangeResponse,
};
use tonic::{Request, Response, Status};

use super::{Endpoint, StorageService, generation};

#[tonic::async_trait]
impl Segments for StorageService {
    async fn begin(
        &self,
        request: Request<BeginRequest>,
    ) -> Result<Response<BeginResponse>, Status> {
        self.admission.check(&request, Endpoint::Segments)?;
        self.check_map(generation::requested(&request)?).await?;
        self.segments.begin(request).await
    }

    async fn put_range(
        &self,
        request: Request<PutRangeRequest>,
    ) -> Result<Response<PutRangeResponse>, Status> {
        self.admission.check(&request, Endpoint::Segments)?;
        self.check_map(generation::requested(&request)?).await?;
        self.segments.put_range(request).await
    }

    async fn commit(
        &self,
        request: Request<CommitRequest>,
    ) -> Result<Response<CommitResponse>, Status> {
        self.admission.check(&request, Endpoint::Segments)?;
        self.check_map(generation::requested(&request)?).await?;
        self.segments.commit(request).await
    }

    async fn locate(
        &self,
        request: Request<LocateRequest>,
    ) -> Result<Response<LocateResponse>, Status> {
        self.admission.check(&request, Endpoint::Segments)?;
        self.check_map(generation::requested(&request)?).await?;
        self.segments.locate(request).await
    }

    async fn read_range(
        &self,
        request: Request<ReadRangeRequest>,
    ) -> Result<Response<ReadRangeResponse>, Status> {
        self.admission.check(&request, Endpoint::Segments)?;
        self.check_map(generation::requested(&request)?).await?;
        self.segments.read_range(request).await
    }
}
