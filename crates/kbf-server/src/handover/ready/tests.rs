//! The frontend telling the service manager it serves, against a manager the test plays.

use std::os::unix::net::UnixDatagram;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use futures::future::BoxFuture;

use super::*;
use crate::handover::tests::Scratch;
use crate::health::{Probe, Readiness};

/// A dependency that fails until the test lets it pass, and counts being asked.
struct Dependency {
    up: Arc<AtomicBool>,
    asked: Arc<AtomicUsize>,
}

impl Probe for Dependency {
    fn probe(&self) -> BoxFuture<'_, Result<(), String>> {
        self.asked.fetch_add(1, Ordering::SeqCst);
        let up = self.up.load(Ordering::SeqCst);
        Box::pin(async move { up.then_some(()).ok_or_else(|| "not up".to_owned()) })
    }
}

/// Catches: `READY=1` sent before the frontend's probes pass (the service manager would
/// start the next unit, or the updater would think the new process serves, while the
/// storage node or state store is not reachable), never sent once they do, and a manager
/// that is not there (no `$NOTIFY_SOCKET`, or a socket nobody listens on) stopping the
/// frontend from running.
#[tokio::test]
async fn ready_waits_for_the_probes_then_tells_the_manager() {
    let dir = Scratch::new();
    let path = dir.0.join("notify");
    let manager = UnixDatagram::bind(&path).expect("a datagram socket");
    manager.set_nonblocking(true).expect("nonblocking");
    let up = Arc::new(AtomicBool::new(false));
    let asked = Arc::new(AtomicUsize::new(0));
    let dependency = Dependency {
        up: Arc::clone(&up),
        asked: Arc::clone(&asked),
    };
    let checks = Arc::new(
        Checks::new(Arc::new(Readiness::default())).probing("dependency", Arc::new(dependency)),
    );
    let ready = Ready {
        checks: Arc::clone(&checks),
        notify: Some(path.clone().into_os_string()),
    };
    let task = tokio::spawn(ready.run());
    // Asked twice while the dependency is down: the first failure was followed by a wait.
    while asked.load(Ordering::SeqCst) < 2 {
        tokio::task::yield_now().await;
    }
    let mut buffer = [0_u8; 32];
    assert!(
        manager.recv(&mut buffer).is_err(),
        "ready sent while a probe fails"
    );
    up.store(true, Ordering::SeqCst);
    task.await.expect("ready ends once the probes pass");
    let got = manager.recv(&mut buffer).expect("READY=1");
    assert_eq!(&buffer[..got], b"READY=1");

    for notify in [None, Some(dir.0.join("nobody-listens").into_os_string())] {
        Ready {
            checks: Arc::clone(&checks),
            notify,
        }
        .run()
        .await;
    }
}
