//! A run whose driver panics (the all-in-one design, komira-build-farm#657, section 7.3).

use kbf_proto::reapi::Digest;

use std::any::Any;

use super::*;
use crate::lease_panic::panicked;
use crate::runtime::Runtime;

fn id() -> LeaseId {
    LeaseId::new(3, 4)
}

fn code(result: &worker::Result) -> i32 {
    result.status.as_ref().expect("status").code
}

/// A runtime whose first run panics (a driver bug); every later run succeeds. It
/// records each kill.
struct PanicsOnce(
    std::sync::atomic::AtomicBool,
    std::sync::Mutex<Vec<LeaseId>>,
);

impl Runtime for PanicsOnce {
    fn driver(&self) -> &'static str {
        "panics-once"
    }

    fn serves(&self, _kind: &str) -> bool {
        true
    }

    async fn run(&self, _work: Work) -> Result<ActionResult, RuntimeError> {
        if !self.0.swap(true, std::sync::atomic::Ordering::SeqCst) {
            panic!("a driver bug");
        }
        Ok(ActionResult::default())
    }

    async fn kill(&self, lease_id: LeaseId) {
        self.1.lock().expect("kills").push(lease_id);
    }
}

/// The next finished run, within ten seconds.
async fn reported(done: &mut mpsc::UnboundedReceiver<Done>) -> Done {
    tokio::time::timeout(std::time::Duration::from_secs(10), done.recv())
        .await
        .expect("the run never reported: its lease stays running for ever")
        .expect("open")
}

/// Catches a run whose driver panics leaving the daemon half-alive (the all-in-one
/// design, komira-build-farm#657, section 7.3): the panic is contained in the run's
/// task, but a run that never reports keeps its lease listed as running in every
/// heartbeat, so the server neither answers nor runs it again. The run must report
/// INTERNAL, naming the driver and the panic, after a kill of the lease (what the
/// run left behind); its lease must stop running, and the next lease on the same
/// driver must run.
#[tokio::test]
async fn a_run_whose_driver_panics_reports_internal_and_the_next_runs() {
    let runtime = Arc::new(PanicsOnce(
        std::sync::atomic::AtomicBool::new(false),
        std::sync::Mutex::default(),
    ));
    let (done, mut done_rx) = mpsc::unbounded_channel();
    let mut leases = Leases::new(Runtimes::new(Arc::clone(&runtime)), done);
    let start = |id| Start {
        lease_id: Some(proto_lease_id(id)),
        kind: "action".to_owned(),
        action_digest: Some(Digest::default()),
        ..Start::default()
    };
    assert_eq!(leases.start(start(id())), None);
    let (ended, outcome, usage, phases) = reported(&mut done_rx).await;
    assert_eq!(ended, id());
    let result = leases.finished(ended, outcome, usage, &phases);
    assert_eq!(code(&result), Code::Internal as i32, "{result:?}");
    let message = &result.status.as_ref().expect("status").message;
    assert_eq!(message, "the panics-once driver panicked: a driver bug");
    assert!(leases.running().is_empty());
    // What the run left was the runtime's to kill before the report.
    assert_eq!(*runtime.1.lock().expect("kills"), [id()]);
    let next = LeaseId::new(3, 5);
    assert_eq!(leases.start(start(next)), None);
    let (ended, outcome, _, _) = reported(&mut done_rx).await;
    assert_eq!(ended, next);
    assert!(outcome.is_ok(), "{outcome:?}");
}

/// Catches a panic's message lost from the report: a `panic!` with arguments
/// carries a `String`, without them a `&str`; any other payload says it had none.
#[test]
fn a_panic_names_its_message_whatever_its_payload() {
    let text = |e: RuntimeError| e.to_string();
    let formatted: Box<dyn Any + Send> = Box::new(format!("bad {}", 7));
    assert_eq!(
        text(panicked("container", formatted.as_ref())),
        "the container driver panicked: bad 7"
    );
    let other: Box<dyn Any + Send> = Box::new(7u8);
    assert_eq!(
        text(panicked("container", other.as_ref())),
        "the container driver panicked: no message"
    );
}
