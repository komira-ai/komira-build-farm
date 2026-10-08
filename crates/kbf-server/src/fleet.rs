//! What the server knows about each node for operators: the fleet view behind
//! `GET /v1/nodes` ([`crate::api`]).
//!
//! The software a node runs arrives in `NodeStatus` (`docs/design/fleet-updates.md`
//! section 3.1). The server keeps the newest one per node, from the node's current
//! stream only, **in memory**: like the scheduler's state it is gone after a restart,
//! and each daemon sends it again after its next `Welcome`.

use kbf_proto::worker::NodeStatus;
use serde::Serialize;

/// One node, as `GET /v1/nodes` lists it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct NodeView {
    /// The node id its daemon registered with.
    pub node_id: String,
    /// Whether its newest stream is still open.
    pub connected: bool,
    /// The newest software status it sent; `None` if it sent none (a daemon that
    /// predates `NodeStatus`).
    pub software: Option<SoftwareView>,
}

/// A node's newest `NodeStatus`, and when the server received it. An empty string or
/// list is a value the node could not read.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SoftwareView {
    /// `macOS`, or the Linux `os-release` `NAME`.
    pub os_name: String,
    /// The OS version.
    pub os_version: String,
    /// The OS build (Mac; Linux where `os-release` has a `BUILD_ID`).
    pub os_build: String,
    /// The kernel release (Linux).
    pub kernel: String,
    /// The `kbf-daemon` version.
    pub daemon_version: String,
    /// Every installed Xcode build (Mac).
    pub xcode_builds: Vec<String>,
    /// When the server received it: milliseconds since the Unix epoch, server clock.
    pub received_at_unix_ms: u64,
}

impl SoftwareView {
    /// `status`, received at `received_at_unix_ms`.
    #[must_use]
    pub fn new(status: NodeStatus, received_at_unix_ms: u64) -> Self {
        Self {
            os_name: status.os_name,
            os_version: status.os_version,
            os_build: status.os_build,
            kernel: status.kernel,
            daemon_version: status.daemon_version,
            xcode_builds: status.xcode_builds,
            received_at_unix_ms,
        }
    }
}

/// The body of `GET /v1/nodes`: every node registered since the server started, in
/// node-id order.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct NodesView {
    /// The nodes.
    pub nodes: Vec<NodeView>,
}
