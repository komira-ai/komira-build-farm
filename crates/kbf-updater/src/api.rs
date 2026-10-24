//! The wire model of the updater's API: the roll request, the roll as the poller sees it, and
//! the closed set of error codes.
//!
//! The request is `deny_unknown_fields` and has no source, URL or asset field: the updater
//! installs a version from the one pinned apt suite and nothing else, so a body that names
//! another source is a `bad_request`, not an ignored field.

use serde::{Deserialize, Serialize};

use crate::version::{Version, VersionError};

/// The longest request body accepted, in bytes (the proxy route's limit as well).
pub const MAX_BODY: usize = 4096;

/// The longest `idempotency_key`, in bytes.
pub const MAX_KEY: usize = 128;

/// What can be rolled. One variant today; a closed enum so an unknown package is a parse
/// error rather than a string that reaches a host.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Package {
    Kbf,
}

/// The `"all"` of a hosts field, as a one-variant enum so that only that word parses.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum All {
    All,
}

/// Which hosts a roll covers: every host of the inventory, or the listed inventory aliases.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Hosts {
    All(All),
    Aliases(Vec<String>),
}

impl Default for Hosts {
    fn default() -> Hosts {
        Hosts::All(All::All)
    }
}

/// `POST /v1/updater/rolls`, validated.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RollRequest {
    pub package: Package,
    pub version: Version,
    pub hosts: Hosts,
    pub idempotency_key: String,
}

/// The body as it arrives: the version is still text, so that a bad one is reported as
/// `bad_version` and not as an unreadable body.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Raw {
    package: Package,
    version: String,
    #[serde(default)]
    hosts: Hosts,
    idempotency_key: String,
}

impl RollRequest {
    /// Read and validate a request body. Nothing is run and no host is named yet: the
    /// aliases are checked against the inventory by [`crate::Updater::start`].
    pub fn from_json(body: &[u8]) -> Result<RollRequest, ApiError> {
        if body.len() > MAX_BODY {
            return Err(ApiError::new(
                ErrorCode::BadRequest,
                format!("the body is longer than {MAX_BODY} bytes"),
            ));
        }
        let raw: Raw = serde_json::from_slice(body)
            .map_err(|e| ApiError::new(ErrorCode::BadRequest, format!("unreadable body: {e}")))?;
        let version = Version::parse(&raw.version)
            .map_err(|e: VersionError| ApiError::new(ErrorCode::BadVersion, e.to_string()))?;
        let key = &raw.idempotency_key;
        let key_ok = !key.is_empty()
            && key.len() <= MAX_KEY
            && key
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
        if !key_ok {
            return Err(ApiError::new(
                ErrorCode::BadRequest,
                format!("idempotency_key must be 1..={MAX_KEY} bytes of letters, digits and . _ -"),
            ));
        }
        if matches!(&raw.hosts, Hosts::Aliases(a) if a.is_empty()) {
            return Err(ApiError::new(
                ErrorCode::BadRequest,
                "hosts is an empty list; name aliases or use \"all\"",
            ));
        }
        Ok(RollRequest {
            package: raw.package,
            version,
            hosts: raw.hosts,
            idempotency_key: raw.idempotency_key,
        })
    }
}

/// The closed set of error codes, for a failed request and for a failed roll alike.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// The body is not a valid request.
    BadRequest,
    /// The version fails the rule of [`crate::version`].
    BadVersion,
    /// A host alias is not in the inventory.
    UnknownAlias,
    /// Another roll is running or interrupted.
    RollActive,
    /// The idempotency key was used with a different body.
    KeyReused,
    /// No such roll.
    UnknownRoll,
    /// The roll has finished, so it cannot be aborted.
    RollTerminal,
    /// The roll record cannot be read or written, so the updater will not act on it.
    StateLost,
    /// No such route.
    NotFound,
    /// The route exists, with another method.
    MethodNotAllowed,
    /// The body is longer than [`MAX_BODY`].
    BodyTooLarge,
    /// The request carries an `Authorization` header: the proxy must strip it, so one that
    /// arrives means the proxy failed, and the updater will not act on the request.
    CredentialsPresent,
    /// The cluster does not know the host (safe-to-stop answered 404).
    UnknownMember,
    /// The host may not be stopped, and did not become stoppable within the bound.
    UnsafeToStop,
    /// Safe-to-stop could not be asked or answered within the bound.
    SafeToStopUnavailable,
    /// The host-side install refused or failed.
    InstallFailed,
    /// The host did not run the target within its bound.
    HealthTimeout,
    /// The host's swap was abandoned.
    SwapAbandoned,
    /// A daemon host is not the node the inventory lists for it, or lists none; nothing was
    /// drained.
    NodeIdentity,
    /// The frontend could not drain the host's node, or did not answer; the node may be left
    /// drained.
    DrainFailed,
    /// The host runs the target, but the frontend could not return its node to placement; the
    /// node is left drained.
    UndrainFailed,
}

impl ErrorCode {
    /// The HTTP status a request refused with this code is answered with.
    pub fn status(self) -> u16 {
        match self {
            ErrorCode::BadRequest
            | ErrorCode::BadVersion
            | ErrorCode::UnknownAlias
            | ErrorCode::CredentialsPresent => 400,
            ErrorCode::UnknownRoll | ErrorCode::NotFound => 404,
            ErrorCode::MethodNotAllowed => 405,
            ErrorCode::BodyTooLarge => 413,
            ErrorCode::RollActive | ErrorCode::KeyReused | ErrorCode::RollTerminal => 409,
            ErrorCode::StateLost => 503,
            // Failure codes of a roll; they are never the status of a request.
            ErrorCode::UnknownMember
            | ErrorCode::UnsafeToStop
            | ErrorCode::SafeToStopUnavailable
            | ErrorCode::InstallFailed
            | ErrorCode::HealthTimeout
            | ErrorCode::SwapAbandoned
            | ErrorCode::NodeIdentity
            | ErrorCode::DrainFailed
            | ErrorCode::UndrainFailed => 500,
        }
    }
}

/// A refused request: a code from the closed set, one line of text for a person, and for
/// `roll_active` the id of the roll in the way.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, thiserror::Error)]
#[error("{code:?}: {message}")]
pub struct ApiError {
    pub code: ErrorCode,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub roll_id: Option<String>,
}

impl ApiError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> ApiError {
        ApiError {
            code,
            message: message.into(),
            roll_id: None,
        }
    }

    pub(crate) fn with_roll(mut self, id: &str) -> ApiError {
        self.roll_id = Some(id.to_owned());
        self
    }
}

/// The state of a roll as a whole.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RollState {
    Running,
    Done,
    Failed,
    Aborted,
    /// An install was in progress at a restart and its outcome could not be learned. Blocks
    /// new rolls until an operator aborts it.
    Interrupted,
}

impl RollState {
    /// Done, failed or aborted: nothing more will happen to the roll.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            RollState::Done | RollState::Failed | RollState::Aborted
        )
    }
}

/// Where one host is in its turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Pending,
    Checking,
    Installing,
    WaitingHealthy,
    Done,
    Failed,
}

/// One host of a roll.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostRoll {
    pub alias: String,
    pub phase: Phase,
    /// How many times an install was started on this host.
    pub attempts: u32,
    /// One line of why, or null. Never raw ssh or apt output.
    pub detail: Option<String>,
}

/// Why a roll failed: the host it stopped at, and a code from the closed set.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Failure {
    pub alias: String,
    pub code: ErrorCode,
    pub detail: String,
}

/// A roll as the poller sees it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Roll {
    pub id: String,
    pub state: RollState,
    pub hosts: Vec<HostRoll>,
    pub failure: Option<Failure>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(extra: &str) -> String {
        format!(
            "{{\"package\":\"kbf\",\"version\":\"1.2.3-1\",\"idempotency_key\":\"run-9\"{extra}}}"
        )
    }

    fn code(body: &str) -> ErrorCode {
        RollRequest::from_json(body.as_bytes())
            .expect_err("refused")
            .code
    }

    /// Catches a request that does not parse in the shape the workflow sends, and a default
    /// that is not "all".
    #[test]
    fn a_request_reads_with_all_or_aliases_and_defaults_to_all() {
        let all = RollRequest::from_json(body(",\"hosts\":\"all\"").as_bytes()).expect("all");
        assert_eq!(all.hosts, Hosts::All(All::All));
        assert_eq!(all.version.as_str(), "1.2.3-1");
        let some = RollRequest::from_json(body(",\"hosts\":[\"a\",\"b\"]").as_bytes()).expect("l");
        assert_eq!(some.hosts, Hosts::Aliases(vec!["a".into(), "b".into()]));
        let none = RollRequest::from_json(body("").as_bytes()).expect("default");
        assert_eq!(none.hosts, Hosts::default());
    }

    /// Catches a field that would widen what is installed: a source, URL or asset field
    /// (the v1 schema has none), any unknown field, and a hosts word other than "all".
    #[test]
    fn unknown_fields_and_other_sources_are_refused() {
        for extra in [
            ",\"source\":\"https://x.example/pkg.deb\"",
            ",\"url\":\"https://x.example\"",
            ",\"asset\":\"kbf.deb\"",
            ",\"x\":1",
            ",\"hosts\":\"some\"",
            ",\"hosts\":7",
        ] {
            assert_eq!(code(&body(extra)), ErrorCode::BadRequest, "{extra}");
        }
        let other = "{\"package\":\"curl\",\"version\":\"1\",\"idempotency_key\":\"k\"}";
        assert_eq!(code(other), ErrorCode::BadRequest);
        assert_eq!(code("{"), ErrorCode::BadRequest);
        assert_eq!(code("{\"package\":\"kbf\"}"), ErrorCode::BadRequest);
    }

    /// Catches a bad version reported as a generic parse error (a caller cannot tell it from
    /// a typo in a field name), and a version that is not checked at the door.
    #[test]
    fn a_bad_version_is_bad_version() {
        for v in ["-1", "1:2", "1\\n", "", "x"] {
            let b =
                format!("{{\"package\":\"kbf\",\"version\":\"{v}\",\"idempotency_key\":\"k\"}}");
            assert_eq!(code(&b), ErrorCode::BadVersion, "{v}");
        }
    }

    /// Catches an idempotency key that could carry a path, a newline or a megabyte into the
    /// record and the audit log, and empty lists that would roll nothing.
    #[test]
    fn keys_and_host_lists_are_bounded() {
        for key in [
            "",
            "a b",
            "a/b",
            "a\\n",
            "k\u{e9}",
            &"k".repeat(MAX_KEY + 1),
        ] {
            let b =
                format!("{{\"package\":\"kbf\",\"version\":\"1\",\"idempotency_key\":\"{key}\"}}");
            assert_eq!(code(&b), ErrorCode::BadRequest, "{key:?}");
        }
        let longest = "k".repeat(MAX_KEY);
        let ok =
            format!("{{\"package\":\"kbf\",\"version\":\"1\",\"idempotency_key\":\"{longest}\"}}");
        assert!(RollRequest::from_json(ok.as_bytes()).is_ok());
        assert_eq!(code(&body(",\"hosts\":[]")), ErrorCode::BadRequest);
    }

    /// Catches a body limit that is off by one or missing.
    #[test]
    fn the_body_limit_is_inclusive() {
        let pad = " ".repeat(MAX_BODY - body("").len());
        let at = format!("{}{pad}", body(""));
        assert_eq!(at.len(), MAX_BODY);
        assert!(RollRequest::from_json(at.as_bytes()).is_ok());
        let over = format!("{at} ");
        assert_eq!(code(&over), ErrorCode::BadRequest);
    }

    /// Catches a code whose wire name or status drifts: callers and the workflow match on both.
    #[test]
    fn codes_have_stable_names_and_statuses() {
        let all = [
            (ErrorCode::BadRequest, "bad_request", 400),
            (ErrorCode::BadVersion, "bad_version", 400),
            (ErrorCode::UnknownAlias, "unknown_alias", 400),
            (ErrorCode::RollActive, "roll_active", 409),
            (ErrorCode::KeyReused, "key_reused", 409),
            (ErrorCode::UnknownRoll, "unknown_roll", 404),
            (ErrorCode::RollTerminal, "roll_terminal", 409),
            (ErrorCode::StateLost, "state_lost", 503),
            (ErrorCode::NotFound, "not_found", 404),
            (ErrorCode::MethodNotAllowed, "method_not_allowed", 405),
            (ErrorCode::BodyTooLarge, "body_too_large", 413),
            (ErrorCode::CredentialsPresent, "credentials_present", 400),
            (ErrorCode::UnknownMember, "unknown_member", 500),
            (ErrorCode::UnsafeToStop, "unsafe_to_stop", 500),
            (
                ErrorCode::SafeToStopUnavailable,
                "safe_to_stop_unavailable",
                500,
            ),
            (ErrorCode::InstallFailed, "install_failed", 500),
            (ErrorCode::HealthTimeout, "health_timeout", 500),
            (ErrorCode::SwapAbandoned, "swap_abandoned", 500),
            (ErrorCode::NodeIdentity, "node_identity", 500),
            (ErrorCode::DrainFailed, "drain_failed", 500),
            (ErrorCode::UndrainFailed, "undrain_failed", 500),
        ];
        for (c, name, status) in all {
            assert_eq!(
                serde_json::to_string(&c).expect("json"),
                format!("\"{name}\"")
            );
            assert_eq!(c.status(), status, "{name}");
        }
    }

    /// Catches an error body that leaks an absent roll id as null, and one that loses the id
    /// a 409 must carry.
    #[test]
    fn an_error_serializes_with_the_roll_id_only_when_set() {
        let plain = ApiError::new(ErrorCode::BadRequest, "no");
        assert_eq!(
            serde_json::to_string(&plain).expect("json"),
            "{\"code\":\"bad_request\",\"message\":\"no\"}"
        );
        let busy = ApiError::new(ErrorCode::RollActive, "busy").with_roll("roll-3");
        assert!(
            serde_json::to_string(&busy)
                .expect("json")
                .ends_with("\"roll_id\":\"roll-3\"}")
        );
        assert_eq!(busy.to_string(), "RollActive: busy");
    }

    /// Catches terminal states drifting: only done, failed and aborted end a roll; an
    /// interrupted roll blocks new ones.
    #[test]
    fn which_states_are_terminal() {
        assert!(!RollState::Running.is_terminal());
        assert!(!RollState::Interrupted.is_terminal());
        assert!(RollState::Done.is_terminal());
        assert!(RollState::Failed.is_terminal());
        assert!(RollState::Aborted.is_terminal());
    }
}
