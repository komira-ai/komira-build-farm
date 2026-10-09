//! The webhook notifier: delivers the outbox by HTTP POST from its own thread, and
//! keeps the outbox in a file. The crate's one impure module.

use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use reqwest::Url;
use serde::Serialize;

use crate::alert::Event;
use crate::outbox::{Backoff, Outbox, OutboxError};

/// The outbox's file name inside the state directory.
pub const OUTBOX_FILE: &str = "alert-outbox.json";

/// Where and how to deliver.
#[derive(Debug, Clone)]
pub struct WebhookConfig {
    /// The `http://` URL each event is POSTed to.
    pub url: String,
    /// Sent as `Authorization: Bearer <token>` when set.
    pub bearer_token: Option<String>,
    /// The longest one attempt may take, connect to last byte.
    pub timeout: Duration,
    /// The wait between failed attempts.
    pub backoff: Backoff,
}

/// The notifier cannot start.
#[derive(Debug, thiserror::Error)]
pub enum WebhookError {
    /// The outbox file cannot be read or written.
    #[error("{}: {source}", path.display())]
    Io {
        /// The file.
        path: PathBuf,
        /// What failed.
        source: io::Error,
    },
    /// The outbox file holds something this build cannot read.
    #[error("{}: {source}", path.display())]
    Outbox {
        /// The file.
        path: PathBuf,
        /// What is wrong with it.
        source: OutboxError,
    },
    /// The URL does not parse, or is not plain `http`.
    #[error("webhook url {url:?}: {reason}")]
    Url {
        /// The URL as given.
        url: String,
        /// Why it is refused.
        reason: String,
    },
    /// The backoff's longest wait does not fit on the clock.
    #[error("webhook backoff max {0:?} is too long")]
    Backoff(Duration),
}

/// What the notifier has done, for a status page or a "notifier failing" alert.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Status {
    /// Events waiting for delivery.
    pub pending: usize,
    /// Events delivered since start.
    pub delivered: u64,
    /// Failed attempts since the last delivery.
    pub failures_in_a_row: u32,
    /// The last failed attempt or outbox write, if any since start.
    pub last_error: Option<String>,
}

/// A running webhook notifier.
///
/// [`Webhook::notify`] hands an event to the notifier's thread and returns at once; it
/// never waits on the network or the disk. The thread appends the event to the
/// outbox, writes the outbox file, then POSTs the oldest pending event until the
/// receiver answers 2xx, waiting [`Backoff`] between failures. A redirect is not
/// followed: a 3xx answer is a failed attempt. Delivery is in order and at least once:
/// the body carries the outbox id, which a receiver can use to drop a repeat.
///
/// An event the receiver refuses for good (a 4xx on every attempt) stays the head and
/// every later event waits behind it; [`Webhook::status`] is the only sign of that.
///
/// An event handed over but not yet written when the process dies is lost; one that is
/// written stays in the file until it is delivered, across any number of restarts.
#[derive(Debug)]
pub struct Webhook {
    events: Sender<Event>,
    status: Arc<Mutex<Status>>,
    worker: JoinHandle<()>,
}

/// The JSON body: the outbox id, then the event's fields.
#[derive(Serialize)]
struct Body<'a> {
    id: u64,
    #[serde(flatten)]
    event: &'a Event,
}

impl Webhook {
    /// Loads the outbox from `state_dir` (none is an empty outbox), checks that the
    /// directory takes the file, and starts the thread, which first delivers whatever
    /// the outbox already holds.
    ///
    /// # Errors
    /// The URL is not plain `http`, the backoff's `max` is too long to add to the
    /// clock, or the outbox cannot be read, parsed or written. A file that does not
    /// parse or cannot be read is an error, never an empty outbox.
    pub fn start(state_dir: &Path, config: WebhookConfig) -> Result<Self, WebhookError> {
        let url = parse_url(&config.url)?;
        if Instant::now().checked_add(config.backoff.max).is_none() {
            return Err(WebhookError::Backoff(config.backoff.max));
        }
        let file = OutboxFile(state_dir.join(OUTBOX_FILE));
        let outbox = file.load()?;
        file.save(&outbox).map_err(|source| WebhookError::Io {
            path: file.0.clone(),
            source,
        })?;
        let status = Arc::new(Mutex::new(Status {
            pending: outbox.len(),
            ..Status::default()
        }));
        let (events, rx) = mpsc::channel();
        let worker = Worker {
            rx,
            outbox,
            file,
            url,
            config,
            status: Arc::clone(&status),
        };
        let worker = std::thread::Builder::new()
            .name("kbf-alert-webhook".into())
            .spawn(move || worker.run())
            .expect("spawn the webhook thread");
        Ok(Self {
            events,
            status,
            worker,
        })
    }

    /// Queues `event` for delivery and returns without waiting.
    pub fn notify(&self, event: Event) {
        // The thread returns only once this sender is gone, so the send fails only
        // after the thread panicked; the event is then dropped.
        let _ = self.events.send(event);
    }

    /// What the notifier has done so far.
    #[must_use]
    pub fn status(&self) -> Status {
        lock(&self.status).clone()
    }

    /// Stops the thread and waits for it: an attempt in flight finishes or times
    /// out, events queued by [`Webhook::notify`] are written to the outbox, and
    /// whatever is not delivered stays in the file for the next start.
    pub fn stop(self) {
        drop(self.events);
        let _ = self.worker.join();
    }
}

fn lock(status: &Mutex<Status>) -> MutexGuard<'_, Status> {
    status.lock().unwrap_or_else(PoisonError::into_inner)
}

fn parse_url(text: &str) -> Result<Url, WebhookError> {
    let refuse = |reason: String| WebhookError::Url {
        url: text.to_owned(),
        reason,
    };
    let url = Url::parse(text).map_err(|e| refuse(e.to_string()))?;
    if url.scheme() != "http" {
        return Err(refuse(format!(
            "scheme {} is not supported; only http is",
            url.scheme()
        )));
    }
    Ok(url)
}

/// The outbox file, replaced whole on every write.
struct OutboxFile(PathBuf);

impl OutboxFile {
    fn load(&self) -> Result<Outbox, WebhookError> {
        match fs::read(&self.0) {
            Ok(bytes) => Outbox::decode(&bytes).map_err(|source| WebhookError::Outbox {
                path: self.0.clone(),
                source,
            }),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Outbox::new()),
            Err(source) => Err(WebhookError::Io {
                path: self.0.clone(),
                source,
            }),
        }
    }

    /// A temporary file beside it, synced, renamed over it, and the directory synced
    /// so the rename itself survives a crash. A crash at any point leaves the old file
    /// or the new one, never a mix.
    fn save(&self, outbox: &Outbox) -> io::Result<()> {
        let tmp = self.0.with_extension("json.tmp");
        let mut file = fs::File::create(&tmp)?;
        file.write_all(&outbox.encode())?;
        file.sync_all()?;
        fs::rename(&tmp, &self.0)?;
        let dir = self
            .0
            .parent()
            .expect("the outbox file is inside a directory");
        fs::File::open(dir)?.sync_all()
    }
}

struct Worker {
    rx: Receiver<Event>,
    outbox: Outbox,
    file: OutboxFile,
    url: Url,
    config: WebhookConfig,
    status: Arc<Mutex<Status>>,
}

impl Worker {
    fn run(mut self) {
        // A runtime of this thread's own, so the caller's (if any) never runs an
        // attempt. The client ignores proxy variables from the environment and follows
        // no redirect, so where it connects and POSTs is only what the URL says.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a current-thread runtime builds");
        let client = reqwest::Client::builder()
            .timeout(self.config.timeout)
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("a plain-HTTP client builds");
        let mut retry_at: Option<Instant> = None;
        let mut unsaved = false;
        loop {
            let wait = if self.outbox.is_empty() {
                Duration::MAX
            } else {
                retry_at.map_or(Duration::ZERO, |t| {
                    t.saturating_duration_since(Instant::now())
                })
            };
            match self.rx.recv_timeout(wait) {
                Ok(event) => {
                    self.outbox.push(event);
                    while let Ok(event) = self.rx.try_recv() {
                        self.outbox.push(event);
                    }
                    unsaved = true;
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            if unsaved {
                unsaved = !self.save();
            }
            if retry_at.is_some_and(|t| t > Instant::now()) {
                continue;
            }
            // The wait above ends only with an event (now pending) or, while something
            // is pending, at the retry time; an empty outbox waits for an event.
            let id = self.outbox.head().expect("an event is pending").id;
            let outcome = runtime.block_on(self.post(&client, id));
            let mut status = lock(&self.status);
            match outcome {
                Ok(()) => {
                    self.outbox.delivered(id);
                    retry_at = None;
                    status.delivered += 1;
                    status.failures_in_a_row = 0;
                }
                Err(error) => {
                    let failures = self.outbox.failed(id).unwrap_or(1);
                    retry_at = Some(Instant::now() + self.config.backoff.delay(failures));
                    status.failures_in_a_row = status.failures_in_a_row.saturating_add(1);
                    status.last_error = Some(error);
                }
            }
            status.pending = self.outbox.len();
            drop(status);
            unsaved = !self.save();
        }
    }

    /// Writes the outbox file; a failure is reported in the status and retried on the
    /// next change. Returns whether the file now matches memory.
    fn save(&self) -> bool {
        let result = self.file.save(&self.outbox);
        let mut status = lock(&self.status);
        status.pending = self.outbox.len();
        match result {
            Ok(()) => true,
            Err(e) => {
                status.last_error = Some(format!("{}: {e}", self.file.0.display()));
                false
            }
        }
    }

    async fn post(&self, client: &reqwest::Client, id: u64) -> Result<(), String> {
        let event = &self.outbox.head().expect("posting the head").event;
        let body = serde_json::to_vec(&Body { id, event }).expect("an event serializes");
        let mut request = client
            .post(self.url.clone())
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body);
        if let Some(token) = &self.config.bearer_token {
            request = request.bearer_auth(token);
        }
        let response = request.send().await.map_err(|e| e.to_string())?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(format!("webhook answered {}", response.status()))
        }
    }
}
