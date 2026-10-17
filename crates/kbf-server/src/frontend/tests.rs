//! What a frontend hands `/readyz` from the state store `bind_frontend` opened.

use std::sync::Arc;

use kbf_state::fake::{Fake, Flaw};
use kbf_state::{ManualClock, Store};

use super::store_of;
use crate::StateStore;
use crate::farm::EpochPolicy;

/// Catches a frontend that probes a different store than the one it opened (or none when
/// one is open): `/readyz` must ask the very store the scheduler writes to, and ask
/// nothing when there is no store.
#[test]
fn readyz_probes_the_store_that_was_opened() {
    let store: Arc<dyn Store> = Arc::new(Fake::with_flaw(
        Arc::new(ManualClock::default()),
        Flaw::None,
    ));
    let state = StateStore {
        store: Arc::clone(&store),
        epoch: EpochPolicy::Keep,
        members: crate::members::MembershipConfig::default(),
        history: None,
    };
    let probed = store_of(Some(&state)).expect("an open store is probed");
    assert!(Arc::ptr_eq(&probed, &store));
    assert!(store_of(None).is_none());
}

fn flags_listening_on(
    reapi: std::net::SocketAddr,
    worker: std::net::SocketAddr,
    api: std::net::SocketAddr,
) -> super::FrontendFlags {
    let argv = [
        "frontend".to_owned(),
        "--storage=http://127.0.0.1:1".to_owned(),
        format!("--listen={reapi}"),
        format!("--worker-listen={worker}"),
        format!("--api-listen={api}"),
    ];
    let matches = super::command().try_get_matches_from(argv).expect("flags");
    super::parse(&matches).expect("a frontend's flags")
}

/// Catches: sockets adopted from a frontend that hands over without being checked
/// against the flags of the one that adopts them, for each of the three: the REAPI,
/// worker and operator API listeners must listen where `--listen`, `--worker-listen` and
/// `--api-listen` say, or the start fails naming the address of the flag (the TLS and
/// plain-text checks on the flags would describe an address that is not served).
#[tokio::test]
async fn sockets_are_adopted_only_where_the_flags_say() {
    use crate::handover::Sockets;
    let bind = || std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let (reapi, worker, api, other) = (bind(), bind(), bind(), bind());
    let at = |l: &std::net::TcpListener| l.local_addr().expect("address");
    let flags = flags_listening_on(at(&reapi), at(&worker), at(&api));
    let own = |l: &std::net::TcpListener| std::os::fd::OwnedFd::from(l.try_clone().expect("copy"));
    let sockets = |reapi: &std::net::TcpListener,
                   worker: &std::net::TcpListener,
                   api: &std::net::TcpListener| Sockets {
        reapi: own(reapi),
        worker: own(worker),
        api: Some(own(api)),
    };

    super::Prepared::new(&flags, Some(sockets(&reapi, &worker, &api)))
        .expect("every socket is where it should be");
    for (what, wrong) in [
        ("listen", sockets(&other, &worker, &api)),
        ("worker", sockets(&reapi, &other, &api)),
        ("api", sockets(&reapi, &worker, &other)),
    ] {
        let error = super::Prepared::new(&flags, Some(wrong)).err().expect(what);
        let wanted = match what {
            "listen" => at(&reapi),
            "worker" => at(&worker),
            _ => at(&api),
        };
        let said = error.to_string();
        assert!(
            said.contains(&format!("bind {wanted}")) && said.contains(&at(&other).to_string()),
            "{what}: {said}"
        );
    }
}
