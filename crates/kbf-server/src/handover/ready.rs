//! Telling systemd the frontend serves, once it does.

use std::ffi::OsString;
use std::sync::Arc;
use std::time::Duration;

use super::notify;
use crate::health::Checks;

/// How often the probes are asked again while one fails.
const LOOK: Duration = Duration::from_millis(100);

/// What a frontend does once it serves: wait for its own probes (the storage node, the
/// state store, the map) to pass, then send `READY=1` to the service manager.
pub(crate) struct Ready {
    pub checks: Arc<Checks>,
    /// `$NOTIFY_SOCKET`, if the manager set it.
    pub notify: Option<OsString>,
}

impl Ready {
    pub async fn run(self) {
        while !self.checks.failing().await.is_empty() {
            tokio::time::sleep(LOOK).await;
        }
        tracing::info!("the frontend is ready");
        let sent = self.notify.as_deref().map(notify::ready);
        if let Some(Err(error)) = sent {
            tracing::warn!(%error, "the service manager could not be told the frontend is ready");
        }
    }
}

#[cfg(test)]
mod tests;
