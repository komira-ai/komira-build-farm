//! The vocabulary of the update client: what the frontend offers and orders, and what the
//! node says back. These are plain values; the wire messages become them at the edge of
//! the daemon (`kbf-daemon`'s `update_link`), so nothing here knows protobuf.

/// The most bytes one component's tarball may be.
pub const MAX_COMPONENT_BYTES: u64 = 512 << 20;

/// The most bytes one component's attestation may be.
pub const MAX_ATTESTATION_BYTES: usize = 256 << 10;

/// The components a node installs, by name. An offer naming another is refused.
pub const COMPONENT_NAMES: [&str; 2] = ["kbf", "kbf-daemon"];

/// How many times the size of an offer the disk must have free before the client starts
/// fetching it: the tarballs land in the inbox and the helper unpacks them beside it.
pub const FREE_DISK_FACTOR: u64 = 3;

/// One file of a release, as the offer names it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Component {
    /// `kbf` or `kbf-daemon`.
    pub name: String,
    /// The SHA-256 of the tarball, as 64 lowercase hex digits; also the key the
    /// frontend serves it by.
    pub sha256: String,
    /// The tarball's length in bytes.
    pub size: u64,
    /// The build attestation of the tarball (a Sigstore bundle line), carried in the
    /// offer. The client copies it; the helper verifies it.
    pub attestation: Vec<u8>,
}

/// The release the frontend wants this node to have.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Offer {
    /// Grows with every change of the desired release; the node acts on a generation
    /// greater than any it has seen.
    pub generation: u64,
    /// The 40-digit commit the release was built from.
    pub commit: String,
    /// The platform the tarballs are for (`linux-x86_64`, `darwin-arm64`, ...).
    pub platform: String,
    /// What to fetch.
    pub components: Vec<Component>,
}

/// What an order asks the helper to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verb {
    /// Switch to the staged release and restart.
    Activate,
    /// Keep the release that was switched to.
    Confirm,
    /// Go back to the previous release.
    Revert,
}

impl Verb {
    /// The word the order file carries.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Activate => "activate",
            Self::Confirm => "confirm",
            Self::Revert => "revert",
        }
    }
}

/// The frontend's order that this node change now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Order {
    /// From a counter that only grows; a node refuses one below the newest it has seen.
    pub order_id: u64,
    /// The epoch of the frontend that sent it; the node takes only its Welcome's.
    pub epoch: u64,
    /// `None` when the message named no verb.
    pub verb: Option<Verb>,
    /// The release the order is about.
    pub commit: String,
    /// How long the helper waits for a confirmation before it reverts.
    pub window_s: u32,
}

/// Where the node stands, as the frontend is told.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// Nothing offered, or an offer not yet staged by the helper.
    Idle,
    /// The client is fetching an offer.
    Fetching,
    /// The helper has the offered release staged.
    Staged,
    /// The helper switched to a release and awaits its confirmation.
    ActivatedUnconfirmed,
    /// The helper switched and the release is confirmed.
    Confirmed,
    /// The helper went back to the previous release.
    Reverted,
    /// An offer or an order was refused, or the helper's status cannot be read; the
    /// reason is in [`Status::refusal`].
    Refused,
    /// This node updates by hand: no update directory, or no helper has reported.
    Manual,
}

/// The facts of a staged release the frontend plans by.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Staged {
    /// The wire protocol of the staged build.
    pub protocol: u32,
    /// The oldest protocol it still speaks.
    pub min_protocol: u32,
    /// The state schema it reads.
    pub state_schema: u32,
}

/// What the node reports after every change and after each Welcome.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Status {
    /// The commit this process was built from (its own word, not the helper's).
    pub running_commit: String,
    /// The release the helper says is installed.
    pub installed_commit: Option<String>,
    /// The release the helper says is staged.
    pub staged_commit: Option<String>,
    /// Where the node stands.
    pub state: State,
    /// Why the node is [`State::Refused`]; empty otherwise.
    pub refusal: String,
    /// The newest order this node accepted; 0 before any.
    pub acked_order_id: u64,
    /// The newest offer generation this node has acted on or refused; 0 before any.
    pub offered_generation: u64,
    /// Whether a helper has reported through the status file.
    pub helper_present: bool,
    /// The staged release's protocol facts, when the helper reported them.
    pub staged: Option<Staged>,
    /// Whether the node is in the update gate: it takes no lease until the update
    /// settles. Not on the wire; it makes a change of gate wake the session.
    pub gated: bool,
}

impl Status {
    /// The status of a node with no update directory: it updates by hand.
    #[must_use]
    pub fn manual(running_commit: &str) -> Self {
        Self {
            running_commit: running_commit.to_owned(),
            installed_commit: None,
            staged_commit: None,
            state: State::Manual,
            refusal: String::new(),
            acked_order_id: 0,
            offered_generation: 0,
            helper_present: false,
            staged: None,
            gated: false,
        }
    }
}

/// Whether `text` is exactly `len` lowercase hexadecimal digits.
#[must_use]
pub fn is_hex(text: &str, len: usize) -> bool {
    text.len() == len && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}
