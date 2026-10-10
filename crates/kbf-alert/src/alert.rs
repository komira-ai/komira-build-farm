//! What an alert says, and the events the book emits about it. Pure.

#![deny(clippy::disallowed_methods, clippy::disallowed_types)]

use serde::{Deserialize, Serialize};

/// Wall time in milliseconds since the Unix epoch, supplied by the caller.
///
/// Events carry it to the receiver and across a restart in the outbox, so it is wall
/// time, not a process-relative clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct UnixMillis(pub u64);

/// How urgent an alert is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// Worth knowing; nothing is failing.
    Info,
    /// Something degrades and will fail if left alone.
    Warning,
    /// Something is failing now.
    Critical,
}

/// The identity of an alert: one rule about one subject.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Key {
    /// The rule that fired, for example `node-disconnected`.
    pub rule: String,
    /// What it fired about, for example a node id.
    pub subject: String,
}

/// An alert whose fields are empty.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AlertError {
    /// The named field is empty or only whitespace.
    #[error("alert {0} is empty")]
    Empty(&'static str),
}

/// One problem, as an operator reads it: what is wrong and the exact fix.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Alert {
    /// The rule that fired.
    pub rule: String,
    /// What it fired about.
    pub subject: String,
    /// How urgent it is.
    pub severity: Severity,
    /// What is wrong, in one line.
    pub summary: String,
    /// The exact fix: the command to run or the change to make.
    pub fix: String,
}

impl Alert {
    /// An alert; every text field must say something.
    ///
    /// # Errors
    /// [`AlertError::Empty`] names the first empty field.
    pub fn new(
        rule: impl Into<String>,
        subject: impl Into<String>,
        severity: Severity,
        summary: impl Into<String>,
        fix: impl Into<String>,
    ) -> Result<Self, AlertError> {
        let alert = Self {
            rule: rule.into(),
            subject: subject.into(),
            severity,
            summary: summary.into(),
            fix: fix.into(),
        };
        for (name, text) in [
            ("rule", &alert.rule),
            ("subject", &alert.subject),
            ("summary", &alert.summary),
            ("fix", &alert.fix),
        ] {
            if text.trim().is_empty() {
                return Err(AlertError::Empty(name));
            }
        }
        Ok(alert)
    }

    /// The rule and subject that identify it.
    #[must_use]
    pub fn key(&self) -> Key {
        Key {
            rule: self.rule.clone(),
            subject: self.subject.clone(),
        }
    }
}

/// Whether an event opens or closes an alert.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Transition {
    /// The problem was seen on enough checks in a row.
    Raised,
    /// The problem was absent on enough checks in a row.
    Resolved,
}

/// What the book tells a notifier: an alert opened or closed.
///
/// It serializes flat, as the webhook body carries it:
/// `{"transition":"raised","rule":..,"subject":..,"severity":..,"summary":..,"fix":..,
/// "first_seen":..,"last_seen":..,"resolved_at":null}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    /// Raised or resolved.
    pub transition: Transition,
    /// The alert's text as of its last bad check.
    #[serde(flatten)]
    pub alert: Alert,
    /// The first bad check of the run of bad checks that raised it.
    pub first_seen: UnixMillis,
    /// The last bad check.
    pub last_seen: UnixMillis,
    /// The good check that resolved it; `None` on a raise.
    pub resolved_at: Option<UnixMillis>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches: an alert built with no fix (the operator is told something is wrong
    /// and not what to do), or with a blank key part.
    #[test]
    fn an_alert_needs_every_text_field() {
        let ok = Alert::new("r", "s", Severity::Info, "sum", "fix").expect("valid");
        assert_eq!(
            ok.key(),
            Key {
                rule: "r".into(),
                subject: "s".into()
            }
        );
        assert_eq!(
            Alert::new(" ", "s", Severity::Info, "sum", "fix"),
            Err(AlertError::Empty("rule"))
        );
        assert_eq!(
            Alert::new("r", "", Severity::Info, "sum", "fix"),
            Err(AlertError::Empty("subject"))
        );
        assert_eq!(
            Alert::new("r", "s", Severity::Info, "", "fix"),
            Err(AlertError::Empty("summary"))
        );
        let err = Alert::new("r", "s", Severity::Info, "sum", "\n").expect_err("no fix");
        assert_eq!(err, AlertError::Empty("fix"));
        assert_eq!(err.to_string(), "alert fix is empty");
    }

    /// Catches: a change to the wire shape a webhook receiver parses (a nested
    /// `alert` object, renamed fields, capitalized enum values).
    #[test]
    fn an_event_serializes_flat() {
        let event = Event {
            transition: Transition::Resolved,
            alert: Alert::new("disk", "node-1", Severity::Critical, "full", "rm x").expect("ok"),
            first_seen: UnixMillis(1),
            last_seen: UnixMillis(2),
            resolved_at: Some(UnixMillis(3)),
        };
        let json = serde_json::to_string(&event).expect("serializes");
        assert_eq!(
            json,
            r#"{"transition":"resolved","rule":"disk","subject":"node-1","severity":"critical","summary":"full","fix":"rm x","first_seen":1,"last_seen":2,"resolved_at":3}"#
        );
        let back: Event = serde_json::from_str(&json).expect("parses");
        assert_eq!(back, event);
    }
}
