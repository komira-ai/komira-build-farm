//! The gate's own audit log and alerts (S5.2): every erase, enforcement, grant and
//! profile install, and every refused or discarded erase request, is written here by
//! the gate itself, so a compromised `kbf-server` cannot hide one.
//!
//! The audit log is JSON lines, appended and synced per event. Alerts go to an
//! [`AlertSink`]; the binary's sink logs them at error level under the target
//! `kbf_mdm_gate::alert` (the host's journal), independent of `kbf-server`.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;

use serde::Serialize;

use crate::clock::format_rfc3339;

/// One thing the gate did or refused.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Event {
    /// RFC 3339, UTC.
    pub at: String,
    /// The verb (`erase`, `grant-admin`, `scheduled-erase`, ...).
    pub verb: String,
    pub serial: String,
    /// `accepted`, `refused`, `held`, `discarded`, `erased`, `granted`, ...
    pub outcome: String,
    /// The signer, lease, build or reason, as the verb has them.
    pub detail: String,
    /// Whether the event is also an alert.
    pub alert: bool,
}

impl Event {
    pub fn new(
        at: i64,
        verb: &str,
        serial: &str,
        outcome: &str,
        detail: impl Into<String>,
        alert: bool,
    ) -> Self {
        Self {
            at: format_rfc3339(at),
            verb: verb.to_owned(),
            serial: serial.to_owned(),
            outcome: outcome.to_owned(),
            detail: detail.into(),
            alert,
        }
    }
}

/// Where alerts go.
pub trait AlertSink: Send + Sync + 'static {
    fn alert(&self, event: &Event);
}

/// Alerts as error-level log lines on the gate's host.
#[derive(Clone, Copy, Debug, Default)]
pub struct LogAlerts;

impl AlertSink for LogAlerts {
    fn alert(&self, event: &Event) {
        tracing::error!(
            target: "kbf_mdm_gate::alert",
            verb = %event.verb,
            serial = %event.serial,
            outcome = %event.outcome,
            detail = %event.detail,
            "kbf-mdm-gate alert"
        );
    }
}

/// The audit log and the alert sink.
pub struct Journal {
    audit: Mutex<File>,
    alerts: Box<dyn AlertSink>,
}

impl Journal {
    /// Opens (creating if needed) the audit log at `path` for appending.
    ///
    /// # Errors
    /// The file cannot be opened.
    pub fn open(path: &Path, alerts: Box<dyn AlertSink>) -> std::io::Result<Self> {
        let audit = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self {
            audit: Mutex::new(audit),
            alerts,
        })
    }

    /// Alerts (if the event is an alert), then appends the event to the audit log and
    /// syncs it.
    ///
    /// # Errors
    /// The audit log cannot be written.
    pub fn record(&self, event: &Event) -> std::io::Result<()> {
        if event.alert {
            self.alerts.alert(event);
        }
        let mut line = serde_json::to_vec(event).map_err(std::io::Error::other)?;
        line.push(b'\n');
        let mut audit = self
            .audit
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        audit.write_all(&line)?;
        audit.sync_data()
    }
}

/// Test material: an alert sink that keeps what it is sent.
#[cfg(test)]
pub(crate) mod recording {
    use std::sync::{Arc, Mutex};

    use super::{AlertSink, Event};

    #[derive(Clone, Default)]
    pub struct Recorded(pub Arc<Mutex<Vec<Event>>>);

    impl Recorded {
        /// `verb outcome serial` of each alert.
        pub fn summary(&self) -> Vec<String> {
            let events = self.0.lock().unwrap();
            events
                .iter()
                .map(|e| format!("{} {} {}", e.verb, e.outcome, e.serial))
                .collect()
        }
    }

    impl AlertSink for Recorded {
        fn alert(&self, event: &Event) {
            self.0.lock().unwrap().push(event.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::recording::Recorded;
    use super::*;

    #[test]
    fn events_are_appended_and_alerts_sent() {
        let dir = crate::testkit::scratch("journal");
        let path = dir.join("audit.log");
        let alerts = Recorded::default();
        let journal = Journal::open(&path, Box::new(alerts.clone())).unwrap();
        journal
            .record(&Event::new(
                1_800_000_000,
                "erase",
                "S1",
                "refused",
                "no touch",
                true,
            ))
            .unwrap();
        journal
            .record(&Event::new(60, "status", "S1", "accepted", "", false))
            .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<serde_json::Value> = text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["at"], "2027-01-15T08:00:00Z");
        assert_eq!(lines[1]["verb"], "status");
        assert_eq!(alerts.summary(), ["erase refused S1"]);
        // Reopening appends.
        let again = Journal::open(&path, Box::new(LogAlerts)).unwrap();
        again
            .record(&Event::new(0, "erase", "S2", "erased", "", true))
            .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap().lines().count(), 3);
        assert!(Journal::open(&dir, Box::new(LogAlerts)).is_err());
    }
}
