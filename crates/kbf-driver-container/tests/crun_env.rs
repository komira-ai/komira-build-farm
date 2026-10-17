//! What an action's container gets beyond its files, against real rootless `crun`: its
//! environment, and its pids, `/dev/shm` and ulimit limits. The bundle the driver writes
//! is all there is to it: no image `ENV`, no node configuration (no `containers.conf`) and
//! no runtime default is read, so what the action sees is what these tests name.
//!
//! Run by `cargo xtask crun-tests`, like `crun.rs` (each test is `#[ignore]` with
//! that reason).

mod support;

use kbf_driver_container::ContainerLimits;
use support::Spec;
use support::real::{Cell, sh};

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

/// Catches anything but the `Command`'s variables reaching the action (the "add the
/// image's `ENV`" and "pass the daemon's environment on" mutants): the image's `PATH` and
/// the daemon's own variables would each appear. A `Command` that sets `HOME` and
/// `HOSTNAME` (with a value holding a space and an `=` besides) gets exactly its
/// variables. One that sets neither gets the two the driver adds: `HOSTNAME=localhost`
/// and `HOME=/root`.
#[tokio::test]
#[ignore = "needs rootless crun and a delegated cgroup: run by cargo xtask crun-tests"]
async fn the_action_sees_only_the_commands_environment() {
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

/// Catches a limit left to the runtime's defaults (the "drop the pids limit" mutant:
/// `pids.max` then reads `max`, no limit at all), a limit that is not the configured one,
/// and the action not running as the container's root.
#[tokio::test]
#[ignore = "needs rootless crun and a delegated cgroup: run by cargo xtask crun-tests"]
async fn the_configured_limits_hold_inside_the_container() {
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

/// Catches defaults a stock host refuses (a ulimit above the daemon's own hard limit
/// fails the container's start), and defaults that do not hold.
#[tokio::test]
#[ignore = "needs rootless crun and a delegated cgroup: run by cargo xtask crun-tests"]
async fn the_default_limits_hold_inside_the_container() {
    let cell = Cell::new("default-limits");
    let result = cell.run(1, &sh(LIMITS)).await.expect("ran");
    assert_eq!(result.exit_code, 0, "{}", cell.stdout(&result));
    assert_eq!(cell.stdout(&result), expected(ContainerLimits::DEFAULT));
    cell.assert_clean(1);
}

/// Catches a program the container cannot start answered as the farm's fault: on
/// real `crun`, a bare `argv[0]` in a Command without `PATH` (the container
/// gets only the Command's environment), a bare name on a `PATH` that lacks it and a
/// path that does not exist are the action's result, exit 127; a file without the
/// executable bit and a directory are exit 126. Each with kbf's message naming the
/// program and where it was looked up. The red run before the fix showed each as
/// `RuntimeError::Failed` ("the container did not run to an exit").
#[tokio::test]
#[ignore = "needs rootless crun and a delegated cgroup: run by cargo xtask crun-tests"]
async fn a_program_the_container_cannot_start_is_the_actions_failure() {
    let cell = Cell::new("program");
    let action = |argv: &[&str], env: &[(&str, &str)]| {
        let mut spec = env_action(env);
        spec.argv = argv.iter().map(|a| (*a).to_owned()).collect();
        spec.inputs = vec![("data", b"not a program", false)];
        spec
    };
    let cases = [
        (action(&["env"], &[]), 127, "the Command sets no PATH"),
        (
            action(&["no-such-tool"], &[("PATH", "/usr/bin:/bin")]),
            127,
            "PATH \"/usr/bin:/bin\"",
        ),
        (action(&["/no/such/tool"], &[]), 127, "was not found"),
        (action(&["./data"], &[]), 126, "not an executable file"),
        (action(&["/bin"], &[]), 126, "not an executable file"),
    ];
    for (seq, (spec, code, says)) in (1..).zip(cases) {
        let result = cell.run(seq, &spec).await;
        let result = result.unwrap_or_else(|e| panic!("{:?}: {e:?}", spec.argv));
        let stderr = cell.stderr(&result);
        assert_eq!(result.exit_code, code, "{:?}: {stderr}", spec.argv);
        assert!(stderr.starts_with("kbf: "), "{stderr}");
        assert!(stderr.contains(&format!("`{}`", spec.argv[0])), "{stderr}");
        assert!(stderr.contains(says), "{stderr}");
        cell.assert_clean(seq);
    }
}
