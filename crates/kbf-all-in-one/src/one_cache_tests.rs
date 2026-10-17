//! When the two halves keep one cache.

use super::*;
use crate::cli::{Flags, command, parse};

fn flags(extra: &[String]) -> Flags {
    let base = [
        "kbf-all-in-one",
        "--server=https://127.0.0.1:8981",
        "--ca-cert=ca.pem",
        "--cert=node.pem",
        "--key=node.key",
        "--node-id=node-1",
        "--driver=container",
    ];
    let argv = base
        .iter()
        .map(|s| (*s).to_owned())
        .chain(extra.iter().cloned());
    parse(&command().get_matches_from(argv)).expect("flags")
}

/// Whether the daemon half was given the one cache (its flags print it).
fn daemon_shares(flags: &Flags) -> bool {
    format!("{:?}", flags.daemon).contains("local_cache: Some(LocalCache")
}

/// Catches the halves given two caches, or only one of them the one cache, when the hot
/// store shares the data directory's disk (neither directory made yet): the server half
/// and the daemon half hold the same one.
#[test]
fn a_hot_store_on_the_data_directorys_disk_is_one_cache() {
    let dir = under_target("same");
    let mut flags = flags(&[
        format!("--hot-store=file://{}", dir.join("hot").display()),
        format!("--data-dir={}", dir.join("data").display()),
    ]);
    let local = share(&mut flags.server, &mut flags.daemon).expect("one cache");
    let server = flags
        .server
        .local_cache
        .as_ref()
        .expect("the server half's");
    assert!(Arc::ptr_eq(&local, server));
    assert!(daemon_holds(&local, &flags.daemon));
}

/// Catches one cache made where the halves do not share a disk (the budget would count
/// bytes on another disk, and the hot store would drop its flushed copies for a cache
/// elsewhere), or where there is no hot store directory: neither half is given one.
#[test]
fn no_hot_store_or_one_on_another_disk_keeps_two() {
    let dir = under_target("apart");
    for hot in [vec!["--hot-store=file:///proc/kbf-hot".to_owned()], vec![]] {
        let mut extra = hot.clone();
        extra.push(format!("--data-dir={}", dir.display()));
        let mut flags = flags(&extra);
        assert!(
            share(&mut flags.server, &mut flags.daemon).is_none(),
            "{hot:?}"
        );
        assert!(flags.server.local_cache.is_none());
        assert!(!daemon_shares(&flags));
    }
    assert!(!same_disk(&dir, std::path::Path::new("relative")));
}
