//! The node's update client, set up from `--update-dir`: the daemon's end of a pull-based
//! update (`kbf_update`). The client learns the release the frontend wants over the
//! session, fetches it over the daemon's mutual-TLS channel to `--cas` (the worker
//! listener, which also serves releases) and leaves it in the update directory's inbox
//! for the root helper. Without `--update-dir` the node updates by hand: it says so and
//! touches no directory.

use std::sync::Arc;

use kbf_daemon::{BUILD_COMMIT, NodeReport, UpdateLink};
use kbf_types::ReleasePlatform;
use kbf_update::{Config, ReleaseClient};

use super::{Cli, Error, transfer};

/// The update link of the node `cli` describes. With `--update-dir`, the client is
/// started and its worker spawned on the current tokio runtime (the caller has entered
/// it); the channel to `--cas` connects when the first release is fetched.
///
/// # Errors
/// `--update-dir` without `--cas`, a node no release is built for, an update directory
/// that cannot be opened, or TLS files that cannot be read.
pub(crate) fn link(cli: &Cli) -> Result<UpdateLink, Error> {
    let Some(dir) = &cli.update_dir else {
        return Ok(UpdateLink::manual(BUILD_COMMIT));
    };
    if cli.cas.is_none() {
        return Err(
            "--update-dir needs --cas: releases are fetched from the server's worker \
                    listener it names"
                .into(),
        );
    }
    let platform = platform(&NodeReport::detect(&[])?)?;
    let source = ReleaseClient::new(transfer::endpoint(cli)?.connect_lazy());
    let config = Config::new(dir.clone(), platform.to_string(), BUILD_COMMIT.to_owned());
    let (handle, worker) = kbf_update::start(config, Arc::new(source))?;
    tokio::spawn(worker.run());
    Ok(UpdateLink::new(handle))
}

/// The release platform `report` (a Hello's `os` and `arch` entries) names.
fn platform(report: &NodeReport) -> Result<ReleasePlatform, Error> {
    let entry = |key: &str| {
        report
            .capabilities()
            .iter()
            .find(|c| c.key == key)
            .map_or("", |c| c.value.as_str())
    };
    let (os, arch) = (entry("os"), entry("arch"));
    ReleasePlatform::from_report(os, arch).ok_or_else(|| {
        format!("no release is built for os {os:?} on arch {arch:?}: --update-dir is refused")
            .into()
    })
}

#[cfg(test)]
mod tests {
    use clap::Parser as _;

    use super::*;
    use crate::tests::{scratch, tls_files};

    fn parse(dir: &std::path::Path, extra: &[String]) -> Cli {
        let base = [
            "kbf-daemon".to_owned(),
            "--server=https://front:7070".to_owned(),
            format!("--ca-cert={}", dir.join("ca.pem").display()),
            format!("--cert={}", dir.join("node.pem").display()),
            format!("--key={}", dir.join("node.key").display()),
            "--node-id=n1".to_owned(),
            "--driver=fake".to_owned(),
        ];
        Cli::try_parse_from(base.iter().chain(extra)).expect("flags")
    }

    /// Catches: a platform read from the wrong entry, a pair no release is built for
    /// taken as one (a Mac on x86_64), and a report with no `os`/`arch` guessed at.
    #[test]
    fn the_platform_is_the_reports_os_and_arch() {
        let table = [
            ("linux", "x86_64", Some("linux-x86_64")),
            ("linux", "arm64", Some("linux-arm64")),
            ("macos", "arm64", Some("darwin-arm64")),
            ("macos", "x86_64", None),
            ("freebsd", "x86_64", None),
        ];
        for (os, arch, want) in table {
            let report = NodeReport::new([("os", os), ("arch", arch), ("drivers", "native")]);
            let got = platform(&report).ok().map(|p| p.to_string());
            assert_eq!(got.as_deref(), want, "{os}/{arch}");
        }
        let why = platform(&NodeReport::new([("os", "linux")])).expect_err("no arch");
        assert!(why.to_string().contains("arch"), "{why}");
    }

    /// Catches: `--update-dir` absent starting a client or creating the directory
    /// (the node must be a node that updates by hand and touch nothing), and a manual
    /// link that reports a state other than MANUAL.
    #[tokio::test]
    async fn without_a_directory_the_node_updates_by_hand_and_touches_nothing() {
        let dir = scratch("update-absent");
        tls_files(&dir);
        let cli = parse(&dir, &[]);
        assert!(cli.update_dir.is_none());
        let before = std::fs::read_dir(&dir).expect("dir").count();
        let _link = link(&cli).expect("manual link");
        assert_eq!(std::fs::read_dir(&dir).expect("dir").count(), before);
    }

    /// Catches: `--update-dir` accepted without `--cas` (the client would have no
    /// release service to fetch from and would fail on the first offer, long after
    /// the start).
    #[tokio::test]
    async fn an_update_directory_needs_cas() {
        let dir = scratch("update-no-cas");
        tls_files(&dir);
        let cli = parse(&dir, &[format!("--update-dir={}", dir.join("u").display())]);
        let why = link(&cli).err().expect("refused").to_string();
        assert!(why.contains("--update-dir needs --cas"), "{why}");
        assert!(!dir.join("u").exists(), "nothing was created");
    }

    /// Catches: the client not started from the flags (no inbox under the directory),
    /// and a worker spawned outside the runtime.
    #[tokio::test]
    async fn an_update_directory_starts_the_client_with_its_inbox() {
        let dir = scratch("update-dir");
        tls_files(&dir);
        let updates = dir.join("u");
        let cli = parse(
            &dir,
            &[
                "--cas=https://127.0.0.1:1".to_owned(),
                format!("--update-dir={}", updates.display()),
            ],
        );
        let _link = link(&cli).expect("link");
        assert!(updates.join("inbox").is_dir());
    }
}
