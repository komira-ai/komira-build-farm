//! `ActionCache` through the frontend: a lookup is the owner's `ActionStore.Get`, which
//! applies the closure check; no client writes.

use std::sync::Arc;

use kbf_auth::{Authorizers, authorize};
use kbf_proto::reapi::action_cache_server::ActionCache;
use kbf_proto::reapi::{ActionResult, GetActionResultRequest, UpdateActionResultRequest};
use tonic::{Request, Response, Status};

use super::owners::Owners;
use crate::wire;

/// The `ActionCache` service over the storage nodes. GetActionResult is authorized by
/// [`Authorizers::ac_get`] against the request's instance name.
pub(super) struct ProxyActionCache {
    pub owners: Owners,
    pub authorizers: Arc<Authorizers>,
}

#[tonic::async_trait]
impl ActionCache for ProxyActionCache {
    /// The owner's answer: a hit only if the entry passes its closure check, else
    /// NOT_FOUND.
    async fn get_action_result(
        &self,
        request: Request<GetActionResultRequest>,
    ) -> Result<Response<ActionResult>, Status> {
        let caller = kbf_auth::metadata(&request);
        let request = request.into_inner();
        let call = "/build.bazel.remote.execution.v2.ActionCache/GetActionResult";
        authorize(
            &*self.authorizers.ac_get,
            &caller,
            call,
            &request.instance_name,
        )
        .await?;
        wire::check_digest_function(request.digest_function)?;
        let action = wire::digest(request.action_digest.as_ref())?;
        match self.owners.action_result(&action).await? {
            Some((_, result)) => Ok(Response::new(result)),
            None => Err(Status::not_found(format!(
                "no usable action-cache entry for {action}"
            ))),
        }
    }

    /// Always PERMISSION_DENIED: a client never writes the action cache.
    async fn update_action_result(
        &self,
        _request: Request<UpdateActionResultRequest>,
    ) -> Result<Response<ActionResult>, Status> {
        Err(Status::permission_denied(
            "clients cannot write the action cache; only the daemon that ran an action does",
        ))
    }
}
