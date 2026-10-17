//! The configuration a listener's next handshake uses, and the look at the files that
//! keeps it current.

use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use rustls::ServerConfig;
use tokio::task::JoinHandle;
use tokio::time::{Instant, MissedTickBehavior};

use crate::config::{Built, Material, TlsError, TlsFiles, build, digest, read};

/// What one look at the files did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The files made a configuration, now the one handshakes use.
    Applied,
    /// The files did not make one (the reason is logged); the previous one stays.
    Rejected,
    /// The files are as they were at the last look, whether that one applied or
    /// rejected them.
    Unchanged,
}

/// How many looks ended each way since the source was made.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Reloads {
    /// Looks that applied new files.
    pub applied: u64,
    /// Looks that refused new files.
    pub rejected: u64,
    /// Looks that found nothing new.
    pub unchanged: u64,
}

/// What the last look saw, to log a bad state once and to do no work for an unchanged one.
#[derive(Debug, PartialEq, Eq)]
enum Seen {
    /// The digest of the files' bytes.
    Files([u8; 32]),
    /// A file that could not be read, by the error's text.
    Unreadable(String),
}

struct State {
    config: Arc<ServerConfig>,
    not_after: i64,
    seen: Seen,
}

struct Shared {
    listener: &'static str,
    /// Where the configuration came from. For a fixed source, names for the messages.
    files: TlsFiles,
    /// Whether `files` are there to be read again (not for a fixed source).
    watch: bool,
    interval: Duration,
    state: Mutex<State>,
    applied: AtomicU64,
    rejected: AtomicU64,
    unchanged: AtomicU64,
}

impl Shared {
    fn state(&self) -> MutexGuard<'_, State> {
        // A panic while the lock was held leaves the state as it was before it (a
        // reload assigns last), so the poisoned lock is still good.
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// The TLS configuration of one listener, read from files and re-read as they change.
/// Cloning shares the configuration.
#[derive(Clone)]
pub struct TlsSource {
    shared: Arc<Shared>,
}

impl std::fmt::Debug for TlsSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TlsSource")
            .field("listener", &self.shared.listener)
            .field("files", &self.shared.files)
            .field("interval", &self.shared.interval)
            .finish_non_exhaustive()
    }
}

impl TlsSource {
    /// The source for the listener called `listener` (a label in the metrics) over
    /// `files`, to be looked at every `interval` once an [`Incoming`](crate::Incoming)
    /// uses it. The files are read now, and must make a configuration.
    ///
    /// # Errors
    /// A file cannot be read, or the files do not make a configuration (see
    /// [`TlsError`]).
    pub fn files(
        listener: &'static str,
        files: TlsFiles,
        interval: Duration,
    ) -> Result<Self, TlsError> {
        let material = read(&files)?;
        Self::of(listener, files, true, interval, &material)
    }

    /// The source for the listener called `listener` over PEM text given here: it has no
    /// files to look at again, so it never changes. For a caller that has the bytes and
    /// no file (a test); a server reads files.
    ///
    /// # Errors
    /// The text does not make a configuration (see [`TlsError`]).
    pub fn fixed(
        listener: &'static str,
        cert: &[u8],
        key: &[u8],
        client_ca: Option<&[u8]>,
    ) -> Result<Self, TlsError> {
        let named = |name: &str| PathBuf::from(format!("<{name}>"));
        let files = TlsFiles {
            cert: named("certificate"),
            key: named("key"),
            client_ca: client_ca.map(|_| named("client CA")),
        };
        let material = Material {
            cert: cert.to_vec(),
            key: key.to_vec(),
            client_ca: client_ca.map(<[u8]>::to_vec),
        };
        Self::of(listener, files, false, Duration::ZERO, &material)
    }

    fn of(
        listener: &'static str,
        files: TlsFiles,
        watch: bool,
        interval: Duration,
        material: &Material,
    ) -> Result<Self, TlsError> {
        let Built { config, not_after } = build(&files, material, false)?;
        let seen = Seen::Files(digest(material));
        Ok(Self {
            shared: Arc::new(Shared {
                listener,
                files,
                watch,
                interval,
                state: Mutex::new(State {
                    config,
                    not_after,
                    seen,
                }),
                applied: AtomicU64::new(0),
                rejected: AtomicU64::new(0),
                unchanged: AtomicU64::new(0),
            }),
        })
    }

    /// The configuration a handshake that starts now uses.
    #[must_use]
    pub fn config(&self) -> Arc<ServerConfig> {
        Arc::clone(&self.shared.state().config)
    }

    /// Looks at the files now: reads each once, and applies them if they differ from the
    /// last look's and make a configuration. The polling task calls this every interval.
    pub fn reload(&self) -> Outcome {
        let shared = &*self.shared;
        if !shared.watch {
            shared.unchanged.fetch_add(1, Ordering::Relaxed);
            return Outcome::Unchanged;
        }
        let material = read(&shared.files);
        let seen = match &material {
            Ok(material) => Seen::Files(digest(material)),
            Err(e) => Seen::Unreadable(e.to_string()),
        };
        let mut state = shared.state();
        if state.seen == seen {
            shared.unchanged.fetch_add(1, Ordering::Relaxed);
            return Outcome::Unchanged;
        }
        // Remembered whether or not it applies, so a bad state is reported once and a
        // second look at it does no work.
        state.seen = seen;
        match material.and_then(|m| build(&shared.files, &m, true)) {
            Ok(built) => {
                state.config = built.config;
                state.not_after = built.not_after;
                shared.applied.fetch_add(1, Ordering::Relaxed);
                tracing::info!(listener = shared.listener, "reloaded the TLS files");
                Outcome::Applied
            }
            Err(e) => {
                shared.rejected.fetch_add(1, Ordering::Relaxed);
                let listener = shared.listener;
                tracing::warn!(listener, error = %e, "TLS files unusable; previous ones kept");
                Outcome::Rejected
            }
        }
    }

    /// How the looks since the source was made ended.
    #[must_use]
    pub fn reloads(&self) -> Reloads {
        Reloads {
            applied: self.shared.applied.load(Ordering::Relaxed),
            rejected: self.shared.rejected.load(Ordering::Relaxed),
            unchanged: self.shared.unchanged.load(Ordering::Relaxed),
        }
    }

    /// Starts looking at the files every interval, until the returned guard is dropped.
    /// Must be called inside a Tokio runtime.
    pub(crate) fn poll(&self) -> Poller {
        let source = self.clone();
        let interval = self.shared.interval;
        if !self.shared.watch {
            return Poller(None);
        }
        Poller(Some(tokio::spawn(async move {
            let mut ticks = tokio::time::interval_at(Instant::now() + interval, interval);
            ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);
            loop {
                ticks.tick().await;
                let source = source.clone();
                // File reads and a key parse: off the runtime's threads.
                let _ = tokio::task::spawn_blocking(move || source.reload()).await;
            }
        })))
    }
}

/// Stops the polling task when dropped.
pub(crate) struct Poller(Option<JoinHandle<()>>);

impl Drop for Poller {
    fn drop(&mut self) {
        self.0.iter().for_each(JoinHandle::abort);
    }
}

/// Appends the reload counters and the leaf expiry of each of `sources` to `out`, in the
/// Prometheus text format. Nothing for no sources.
pub fn render_metrics<'a>(sources: impl IntoIterator<Item = &'a TlsSource>, out: &mut String) {
    let sources: Vec<&TlsSource> = sources.into_iter().collect();
    if sources.is_empty() {
        return;
    }
    out.push_str(
        "# HELP kbf_tls_reloads_total Looks at a listener's TLS files, by outcome\n\
         # TYPE kbf_tls_reloads_total counter\n",
    );
    for source in &sources {
        let (listener, reloads) = (source.shared.listener, source.reloads());
        for (outcome, n) in [
            ("applied", reloads.applied),
            ("rejected", reloads.rejected),
            ("unchanged", reloads.unchanged),
        ] {
            let _ = writeln!(
                out,
                "kbf_tls_reloads_total{{listener=\"{listener}\",outcome=\"{outcome}\"}} {n}"
            );
        }
    }
    out.push_str(
        "# HELP kbf_tls_leaf_not_after_seconds When the certificate a listener serves stops \
         being valid, in seconds since the epoch\n\
         # TYPE kbf_tls_leaf_not_after_seconds gauge\n",
    );
    for source in &sources {
        let _ = writeln!(
            out,
            "kbf_tls_leaf_not_after_seconds{{listener=\"{}\"}} {}",
            source.shared.listener,
            source.shared.state().not_after
        );
    }
}
