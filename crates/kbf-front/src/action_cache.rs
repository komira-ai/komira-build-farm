//! `ActionCache`: lookups through the closure check; no client writes.

use std::sync::Arc;

use kbf_objstore::ObjectStore;
use kbf_proto::reapi::action_cache_server::ActionCache;
use kbf_proto::reapi::{ActionResult, GetActionResultRequest, UpdateActionResultRequest};
use tonic::{Request, Response, Status};

use crate::cache::Cache;
use crate::meta_log::MetaLog;
use crate::wire;

/// The `ActionCache` service over a [`Cache`].
#[derive(Debug)]
pub struct ActionCacheService<M, O> {
    cache: Arc<Cache<M, O>>,
}

impl<M, O> ActionCacheService<M, O> {
    /// The service over `cache`.
    pub const fn new(cache: Arc<Cache<M, O>>) -> Self {
        Self { cache }
    }
}

#[tonic::async_trait]
impl<M: MetaLog, O: ObjectStore + 'static> ActionCache for ActionCacheService<M, O> {
    /// A hit only when the entry passes the closure check: its result and every blob
    /// it needs are present and reachable. Anything else is NOT_FOUND, so the action
    /// runs again rather than handing out a result whose files are gone. Outputs are
    /// never inlined (REAPI lets the server decline).
    async fn get_action_result(
        &self,
        request: Request<GetActionResultRequest>,
    ) -> Result<Response<ActionResult>, Status> {
        let request = request.into_inner();
        wire::check_digest_function(request.digest_function)?;
        let action = wire::digest(request.action_digest.as_ref())?;
        match self.cache.action_result(&action).await? {
            Some(result) => Ok(Response::new(result)),
            None => Err(Status::not_found(format!(
                "no usable action-cache entry for {action}"
            ))),
        }
    }

    /// Always PERMISSION_DENIED, before the request is read: a client never writes the
    /// action cache (RFC 3.5, 16.2). The daemon that ran an action writes its result
    /// through [`Cache::write_action_result`].
    async fn update_action_result(
        &self,
        _request: Request<UpdateActionResultRequest>,
    ) -> Result<Response<ActionResult>, Status> {
        Err(Status::permission_denied(
            "clients cannot write the action cache; only the daemon that ran an action does",
        ))
    }
}
