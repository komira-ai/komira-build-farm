//! What an action's container gets beyond its files, against real rootless Podman: its
//! environment, and its pids, `/dev/shm` and ulimit limits.
//!
//! Run by `tools/ci/podman-tests.sh`, like `podman.rs` (each test is `#[ignore]` with
//! that reason), with `CONTAINERS_CONF_OVERRIDE` naming
//! `fixtures/containers-override.conf`: a node configuration that sets its own
//! environment variable, pids limit, `/dev/shm` size and ulimits. A test fails if the
//! variable is not set, so the override cannot be dropped silently.

mod support;

use kbf_driver_container::ContainerLimits;
use support::Spec;
use support::real::{Cell, sh, var};

/// Fails unless the run has the node configuration the tests are meant to resist.
fn assert_override() {
    let file = var("CONTAINERS_CONF_OVERRIDE");
    let text = std::fs::read_to_string(&file).expect("read the containers.conf override");
    assert!(text.contains("pids_limit = 999"), "{file}: {text}");
}

/// An action that prints its environment with nothing between it and the container's
/// start: `/bin/env` run directly, no shell (a shell would add `PWD` and the like).
fn env_action(env: &[(&str, &str)]) -> Spec {
    let mut spec = sh("unused");
    spec.argv = vec!["/bin/env".to_owned()];
    spec.env = env
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect();
    spec
}

/// Catches anything but the `Command`'s variables reaching the action (the "drop
/// `--unsetenv-all`" mutant): the image's `PATH`, Podman's own `TERM` and `container`,
/// and the node's `containers.conf` `env` would each appear. A `Command` that sets
/// `HOME` and `HOSTNAME` (with a value holding a space and an `=` besides) gets
/// exactly its variables. One that sets neither gets the two Podman adds whatever
/// `--unsetenv-all` says: `HOSTNAME=localhost` and the busybox image's root home.
#[tokio::test]
#[ignore = "needs rootless Podman and a delegated cgroup: run by tools/ci/podman-tests.sh"]
async fn the_action_sees_only_the_commands_environment() {
    assert_override();
    let cell = Cell::new("env");
    let sorted = |stdout: &str| -> Vec<String> {
        let mut lines: Vec<String> = stdout.lines().map(str::to_owned).collect();
        lines.sort_unstable();
        lines
    };
    let spec = env_action(&[
        ("A_B", "x y=z"),
        ("HOME", "/kbf/home"),
        ("HOSTNAME", "h"),
        ("LANG", "C"),
    ]);
    let result = cell.run(1, &spec).await.expect("ran");
    assert_eq!(result.exit_code, 0);
    assert_eq!(
        sorted(&cell.stdout(&result)),
        ["A_B=x y=z", "HOME=/kbf/home", "HOSTNAME=h", "LANG=C"]
    );
    let result = cell.run(2, &env_action(&[])).await.expect("ran");
    assert_eq!(result.exit_code, 0);
    assert_eq!(
        sorted(&cell.stdout(&result)),
        ["HOME=/root", "HOSTNAME=localhost"],
        "an empty Command environment"
    );
    cell.assert_clean(1);
    cell.assert_clean(2);
}

/// What the action sees of its limits, one per line: `pids.max`, the size option of
/// the `/dev/shm` mount, the soft and hard `nofile` and `nproc` ulimits, and its uid
/// and gid.
const LIMITS: &str = "cat /sys/fs/cgroup/pids.max; \
    sed -n 's|^[^ ]* /dev/shm tmpfs .*size=\\([0-9]*k\\).*|\\1|p' /proc/mounts; \
    ulimit -Sn; ulimit -Hn; ulimit -Su; ulimit -Hu; id -u; id -g";

/// The lines [`LIMITS`] prints under `limits`.
fn expected(limits: ContainerLimits) -> String {
    let ContainerLimits {
        pids,
        shm_mib,
        nofile,
        nproc,
    } = limits;
    let shm_kib = shm_mib * 1024;
    format!("{pids}\n{shm_kib}k\n{nofile}\n{nofile}\n{nproc}\n{nproc}\n0\n0\n")
}

/// Catches a limit left to Podman or the node's `containers.conf` (the "drop
/// `--pids-limit`" mutant: on the hosted runners' Podman 4.9 `pids.max` then reads
/// `max`, no limit at all, override or not), a limit that is not the configured one,
/// and the action not running as the container's root.
#[tokio::test]
#[ignore = "needs rootless Podman and a delegated cgroup: run by tools/ci/podman-tests.sh"]
async fn the_configured_limits_hold_inside_the_container() {
    assert_override();
    let limits = ContainerLimits {
        pids: 321,
        shm_mib: 7,
        nofile: 1111,
        nproc: 2222,
    };
    let cell = Cell::with("limits", |config| config.limits = limits);
    let result = cell.run(1, &sh(LIMITS)).await.expect("ran");
    assert_eq!(result.exit_code, 0, "{}", cell.stdout(&result));
    assert_eq!(cell.stdout(&result), expected(limits));
    cell.assert_clean(1);
}

/// Catches defaults rootless Podman refuses on a stock host (a ulimit above the
/// daemon's own hard limit fails `podman start`), and defaults that do not hold.
#[tokio::test]
#[ignore = "needs rootless Podman and a delegated cgroup: run by tools/ci/podman-tests.sh"]
async fn the_default_limits_hold_inside_the_container() {
    assert_override();
    let cell = Cell::new("default-limits");
    let result = cell.run(1, &sh(LIMITS)).await.expect("ran");
    assert_eq!(result.exit_code, 0, "{}", cell.stdout(&result));
    assert_eq!(cell.stdout(&result), expected(ContainerLimits::DEFAULT));
    cell.assert_clean(1);
}
