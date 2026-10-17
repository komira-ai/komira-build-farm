//! Shared value types for kbf: the content digest (function, hash, size), an image named
//! by manifest digest, platform properties, QoS levels, lease identifiers and kinds, node
//! phases, farm time, the vocabulary of scheduled work (operations, workers, request
//! vectors, control-log records), and the `StateMachine` trait and `Effect` enum that the
//! pure cores implement and emit, the fleet rollout record with its per-node steps, and the
//! enrollment vocabulary (tokens, node ids, roles, the reasons a frontend refuses a node).
//!
//! This is a pure crate: no async runtime, network, clock, randomness or hashed
//! collections. The layering test in `kbf-it` and the lists in `clippy.toml` enforce it.
//! Every value here is plain data: two processes that build a value from the same
//! inputs get equal values that order and format identically.

#![deny(clippy::disallowed_methods, clippy::disallowed_types)]

mod digest;
mod enroll;
mod image;
mod lease;
mod phase;
mod platform;
mod qos;
mod rollout;
mod state;
mod time;
mod work;

pub use digest::{Digest, DigestFunction, ParseDigestError};
pub use enroll::{
    CaPin, NodeId, NodeIdError, PartFault, RefusalCode, RefusalReason, Role, RoleError, RoleSet,
    RoleSetError, Token, TokenError, TokenId, TokenSecret,
};
pub use image::{ImageError, ImageRef};
pub use lease::{LeaseId, LeaseKind};
pub use phase::NodePhase;
pub use platform::{Platform, PlatformError};
pub use qos::{CustomQos, Qos, QosError};
pub use rollout::{
    Actor, IllegalStep, NodeProgress, NodeStep, Rollout, RolloutId, RolloutState, Selector,
    Strategy, StrategyError,
};
pub use state::{Effect, StateMachine};
pub use time::FarmTime;
pub use work::{
    ActionKey, Answer, ControlRecord, Failure, FarmFault, FarmRun, FencePolicy, LeaseGrant,
    LostWhy, MemoryKill, MemoryRun, OperationId, Outcome, Refusal, RefusalRecord, Resources,
    ResultRecord, StartLease, WaiterId, Waiting, WorkerId,
};
