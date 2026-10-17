//! An action cannot read the node's cold-store key, against real rootless `crun`
//! (docs/design/architecture-v2.md, sections 9 and 15: storage and actions are separate
//! uids on a node that holds the key and runs actions). Neither from the environment nor
//! from a file.
//!
//! The environment: the daemon's unit sets the key's two variables from its
//! `EnvironmentFile=`, and the driver starts `crun` and its helpers without them, so no
//! runtime feature that copies the host's environment into a container has the key to
//! copy (`tests/credential_env.rs` checks that on a stand-in `crun`; this checks the real
//! one's container). The files: a container holds nothing of the node's but its image,
//! its input root and the shared trees (no node configuration can mount a directory into
//! every container), and the daemon's own files are
//! unmapped ids to it: a file of the daemon's user, mode `0600`, is `nobody`'s to the
//! action. The key file here is on the input root's path, so a mount that did reach the
//! action would be read: the control is a file of mode `0644` beside it, which is.
//!
//! Run by `cargo xtask crun-tests`, like `crun.rs` (the test is `#[ignore]` with
//! that reason). One test in this binary: it sets variables of its own process (the
//! key), which is sound only while no other thread reads the environment.

mod support;

use support::real::{Cell, sh};

/// The key's two variables, as the unit's `EnvironmentFile=` sets them.
const KEY: [(&str, &str); 2] = [
    ("AWS_ACCESS_KEY_ID", "kbf-test-cold-store-key-id"),
    ("AWS_SECRET_ACCESS_KEY", "kbf-test-cold-store-secret"),
];

/// What the action runs: the key's variables as it sees them, the control file, and a
/// search of the directories it can see for the key's secret.
const SCRIPT: &str = "echo \"env: ${AWS_ACCESS_KEY_ID:-unset} ${AWS_SECRET_ACCESS_KEY:-unset}\"; \
    printf 'control: '; cat /kbf/root/control; \
    printf 'found: '; grep -rl kbf-test-cold-store-secret /bin /etc /home /kbf /root /tmp /usr /var \
        2>/dev/null | wc -l";

/// Catches the key in the action's environment (the "start `crun` with the key"
/// mutant: the key's variables are set in this process, which is the daemon's, and the
/// action must not see them), and the key file reachable by the action: the key's file
/// is mode `0600` and the daemon's, so it could not be read, and a search of the container's
/// directories for the secret finds nothing. The control, a
/// `0644` file given as an input, is read, so the search reads what it can.
#[test]
#[ignore = "needs rootless crun and a delegated cgroup: run by cargo xtask crun-tests"]
fn an_action_cannot_read_the_cold_store_key() {
    // SAFETY: the only test of this binary, and nothing before this starts a thread,
    // so no other thread of this process reads the environment meanwhile.
    unsafe {
        for (name, value) in KEY {
            std::env::set_var(name, value);
        }
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let cell = Cell::new("credential");
        let mut spec = sh(SCRIPT);
        spec.inputs = vec![("control", b"readable by any id\n", false)];
        let result = cell.run(1, &spec).await.expect("ran");
        let (stdout, stderr) = (cell.stdout(&result), cell.stderr(&result));
        assert!(
            !stdout.contains(KEY[1].1),
            "the action saw the key: {stdout}"
        );
        assert_eq!(
            stdout, "env: unset unset\ncontrol: readable by any id\nfound: 0\n",
            "stderr: {stderr}"
        );
        assert_eq!(result.exit_code, 0);
        cell.assert_clean(1);
    });
}
