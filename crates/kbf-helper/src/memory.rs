//! What the helper remembers between cycles, in `helper.json` beside `state.json`
//! (root-owned: the inbox is the service user's, so nothing the helper must trust is kept
//! there).

use kbf_installer::CommitId;
use serde::{Deserialize, Serialize};

/// What the last thing the helper did came to, as `status.json` reports it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Event {
    #[default]
    Idle,
    Confirmed,
    Reverted,
    Refused,
}

/// What a staged release says about itself, for the frontend's ordering decision.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StagedFacts {
    pub commit: CommitId,
    pub protocol: u32,
    pub min_protocol: u32,
    pub state_schema: u32,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Memory {
    /// The newest order acted on (acted on includes refused).
    pub acked_order_id: u64,
    pub event: Event,
    /// Why the last offer or order was refused, or what was reverted.
    pub refusal: String,
    /// The newest inbox entry looked at, so a refused offer is not staged again every
    /// cycle.
    pub handled_entry: String,
    /// The unit must be restarted: the links moved and the restart is not done. Written
    /// before the restart, so a crash between the two restarts on the next cycle.
    pub restart_due: bool,
    pub staged: Option<StagedFacts>,
}
