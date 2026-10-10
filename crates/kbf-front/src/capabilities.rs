//! `Capabilities`: what the cache tells a client before it sends anything.
//!
//! The answer is static, so the cache keeps serving while execution is paused (RFC
//! section 3.5). A front that also serves `Execution` advertises it.

use std::sync::Arc;

use kbf_auth::{Authorizers, authorize};
use kbf_proto::build::bazel::semver::SemVer;
use kbf_proto::reapi::capabilities_server::Capabilities;
use kbf_proto::reapi::{
    self, ActionCacheUpdateCapabilities, CacheCapabilities, ExecutionCapabilities,
    GetCapabilitiesRequest, PriorityCapabilities, ServerCapabilities,
};
use tonic::{Request, Response, Status};

use crate::MAX_BATCH_TOTAL_BYTES;

/// The `Capabilities` service. GetCapabilities is authorized by
/// [`Authorizers::capabilities`] against the request's instance name; a refusal fails
/// the call (it does not answer with empty capabilities).
#[derive(Clone, Debug)]
pub struct CapabilitiesService {
    execution: bool,
    authorizers: Arc<Authorizers>,
}

impl CapabilitiesService {
    /// Capabilities of a front that serves the cache only, every call allowed.
    #[must_use]
    pub fn cache_only() -> Self {
        Self::new(false, Arc::new(Authorizers::allow_all()))
    }

    /// Capabilities of a front that serves the cache and `Execution`, every call
    /// allowed.
    #[must_use]
    pub fn with_execution() -> Self {
        Self::new(true, Arc::new(Authorizers::allow_all()))
    }

    /// Capabilities of a front that serves `Execution` too if `execution`, each call
    /// authorized by `authorizers`.
    #[must_use]
    pub const fn new(execution: bool, authorizers: Arc<Authorizers>) -> Self {
        Self {
            execution,
            authorizers,
        }
    }
}

/// The capabilities the cache half serves.
///
/// - SHA-256 only.
/// - `update_enabled: false`: only daemons write the action cache (RFC 3.5, 16.2).
/// - One cache priority range, `[0, 0]`: retention follows the last touch alone
///   (RFC 9.1), so a priority is accepted and changes nothing.
/// - Batch calls carry at most [`MAX_BATCH_TOTAL_BYTES`] of blob data.
/// - No compressors yet: the RFC's zstd is advertised only once reads and writes
///   decode it, since a client that sees it will send it.
/// - REAPI 2.0 to 2.3.
/// - With `execution`: execution enabled, SHA-256. No priorities and no node
///   properties are advertised yet; QoS is a header, never a property (RFC 4.5).
#[must_use]
pub fn server_capabilities(execution: bool) -> ServerCapabilities {
    let sha256 = reapi::digest_function::Value::Sha256 as i32;
    let version = |minor| SemVer {
        major: 2,
        minor,
        patch: 0,
        prerelease: String::new(),
    };
    ServerCapabilities {
        cache_capabilities: Some(CacheCapabilities {
            digest_functions: vec![sha256],
            action_cache_update_capabilities: Some(ActionCacheUpdateCapabilities {
                update_enabled: false,
            }),
            cache_priority_capabilities: Some(PriorityCapabilities {
                priorities: vec![reapi::priority_capabilities::PriorityRange {
                    min_priority: 0,
                    max_priority: 0,
                }],
            }),
            max_batch_total_size_bytes: MAX_BATCH_TOTAL_BYTES as i64,
            symlink_absolute_path_strategy: reapi::symlink_absolute_path_strategy::Value::Allowed
                as i32,
            ..CacheCapabilities::default()
        }),
        execution_capabilities: execution.then(|| ExecutionCapabilities {
            digest_function: sha256,
            exec_enabled: true,
            digest_functions: vec![sha256],
            ..ExecutionCapabilities::default()
        }),
        deprecated_api_version: Some(version(0)),
        low_api_version: Some(version(0)),
        high_api_version: Some(version(3)),
    }
}

#[tonic::async_trait]
impl Capabilities for CapabilitiesService {
    async fn get_capabilities(
        &self,
        request: Request<GetCapabilitiesRequest>,
    ) -> Result<Response<ServerCapabilities>, Status> {
        let caller = kbf_auth::metadata(&request);
        let instance = &request.get_ref().instance_name;
        let call = "/build.bazel.remote.execution.v2.Capabilities/GetCapabilities";
        authorize(&*self.authorizers.capabilities, &caller, call, instance).await?;
        Ok(Response::new(server_capabilities(self.execution)))
    }
}
