//! What the server knows about each node for operators: the fleet view behind
//! `GET /v1/nodes` ([`crate::api`]), and where each node is in placement (serving,
//! cordoned, draining; see `kbf_sched::Cordon`).
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
    /// Whether placement may use it, and where its drain is.
    pub placement: PlacementView,
}

/// Where a node is in placement. Serialized with its `state` as a tag.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum PlacementView {
    /// Placement may offer it leases.
    Serving,
    /// An operator cordoned it: no new lease; its leases run on.
    Cordoned,
    /// Cordoned, and its leases are waited for until the deadline.
    Draining {
        /// When the drain pauses if leases still run: milliseconds since the Unix
        /// epoch, server clock.
        deadline_unix_ms: u64,
        /// The leases it still holds, as `term.seq`.
        leases: Vec<String>,
    },
    /// Cordoned, and it holds no lease: it may be taken out of service.
    Drained,
    /// The deadline passed while leases still ran. They run on; nothing proceeds
    /// until an operator drains it again or uncordons it.
    DrainPaused {
        /// The deadline that passed (milliseconds since the Unix epoch).
        deadline_unix_ms: u64,
        /// The leases it still holds, as `term.seq`.
        leases: Vec<String>,
    },
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

/// The body of `GET /v1/nodes`: the server that answers, and every node registered
/// since it started, in node-id order.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct NodesView {
    /// The server process that answers.
    pub server: ServerView,
    /// The nodes.
    pub nodes: Vec<NodeView>,
}

impl NodesView {
    /// `nodes`, as this server build lists them.
    #[must_use]
    pub fn of_this_build(nodes: Vec<NodeView>) -> Self {
        Self {
            server: ServerView::this_build(),
            nodes,
        }
    }
}

/// The `kbf-server` build that answers, so a deploy can check which commit runs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ServerView {
    /// As `--version` and the start line print it: [`crate::SERVER_VERSION`].
    pub version: String,
    /// The commit alone: [`crate::BUILD_COMMIT`].
    pub commit: String,
}

impl ServerView {
    /// This build.
    #[must_use]
    pub fn this_build() -> Self {
        Self {
            version: crate::SERVER_VERSION.to_owned(),
            commit: crate::BUILD_COMMIT.to_owned(),
        }
    }
}
