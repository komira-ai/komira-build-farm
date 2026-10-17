//! What a frontend start logs when it cannot route by the storage map. Alone in its test
//! binary: a log event is seen by a subscriber set for one thread only if no other thread
//! has already made the callsite uninteresting, which other frontend starts in the same
//! process would do.

use std::future::pending;
use std::sync::{Arc, Mutex, PoisonError};

use kbf_server::frontend::{self, start_frontend};

/// A `MakeWriter` into a shared buffer, standing for the log.
#[derive(Clone, Default)]
struct Log(Arc<Mutex<Vec<u8>>>);

struct LogWriter(Log);

impl std::io::Write for LogWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        (self.0)
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Log {
    type Writer = LogWriter;

    fn make_writer(&'a self) -> LogWriter {
        LogWriter(self.clone())
    }
}

impl Log {
    fn text(&self) -> String {
        String::from_utf8(
            self.0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone(),
        )
        .expect("UTF-8")
    }
}

/// The flags of `kbf frontend` for a start on loopback ports, and these more.
fn flags(more: &[&str]) -> frontend::FrontendFlags {
    let mut argv = vec![
        "frontend",
        "--listen=127.0.0.1:0",
        "--worker-listen=127.0.0.1:0",
        "--api-listen=127.0.0.1:0",
        "--storage=http://127.0.0.1:1",
    ];
    argv.extend_from_slice(more);
    frontend::parse(
        &frontend::command()
            .try_get_matches_from(argv)
            .expect("flags"),
    )
    .expect("parses")
}

/// Catches a frontend with a state store and no TLS flags that goes on storing to its one
/// `--storage` node in silence, when the members its daemons place blobs on are others
/// (the failure is an OK result whose outputs are not stored): the start logs a warning
/// saying so; without a state store there are no members and it does not.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_state_store_without_tls_flags_warns_that_members_are_not_routed_to() {
    let log = Log::default();
    let dispatch = tracing::Dispatch::new(
        tracing_subscriber::fmt()
            .with_writer(log.clone())
            .with_ansi(false)
            .finish(),
    );
    let start = |f: &frontend::FrontendFlags| {
        tracing::dispatcher::with_default(&dispatch, || {
            start_frontend(f, || Ok(pending::<()>())).expect("starts")
        })
    };

    let _plain = start(&flags(&[]));
    assert!(
        !log.text().contains("members cannot be dialled"),
        "{}",
        log.text()
    );
    let state = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("frontend-warns")
        .join(format!("state-{}", std::process::id()));
    std::fs::create_dir_all(&state).expect("directory");
    let store = format!(
        "--state-store=sqlite://{}",
        state.join("state.db").display()
    );
    let _stored = start(&flags(&[&store]));
    assert!(
        log.text().contains("members cannot be dialled"),
        "{}",
        log.text()
    );
}
