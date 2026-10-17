//! An action cannot read the node's cold-store key, against real rootless Podman
//! (docs/design/architecture-v2.md, sections 9 and 15: storage and actions are separate
//! uids on a node that holds the key and runs actions). Neither from a file nor from its
//! environment, even on a node whose containers.conf hands both over: it mounts the
//! directory of the key file into every container and sets `env_host = true`.
//!
//! The key file is the daemon user's, mode `0600`, as the unit's user would hold it
//! (the deploy kit's `s3.env` is root's, mode `0600`, which no action's id reaches
//! either). Two files beside it are the controls, read by the same action through the
//! same mount: one of mode `0644`, and one of mode `0600` given to the container's root
//! (`podman unshare chown 1:1`, the daemon user's first subordinate id). Both are
//! read, so the mount reaches the action and the key file's mode and owner are what
//! refuse it: a key file readable by others, or the actions' id's, would be read.
//!
//! Run by `cargo xtask podman-tests`, like `podman.rs` (the test is `#[ignore]` with
//! that reason). One test in this binary: it sets variables of its own process (the
//! key, and `CONTAINERS_CONF_OVERRIDE` naming the node configuration it writes), which
//! is sound only while no other thread reads the environment.

mod support;

use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;

use support::real::{Cell, podman, sh};

/// The key's two variables, as the unit's `EnvironmentFile=` sets them.
const KEY: [(&str, &str); 2] = [
    ("AWS_ACCESS_KEY_ID", "kbf-test-cold-store-key-id"),
    ("AWS_SECRET_ACCESS_KEY", "kbf-test-cold-store-secret"),
];

/// What the action runs: each file, or `refused` with `cat`'s exit code, then the
/// key's variables as it sees them.
const SCRIPT: &str = "for f in control given s3.env; do \
        printf '%s: ' $f; cat /kbf-node/$f || echo \"refused $?\"; \
    done; \
    echo \"env: ${AWS_ACCESS_KEY_ID:-unset} ${AWS_SECRET_ACCESS_KEY:-unset}\"";

/// Writes `contents` to `path` with permission bits `mode`.
fn write(path: &Path, contents: &str, mode: u32) {
    std::fs::write(path, contents).expect("write");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("chmod");
}

/// Catches an action that reads the key file: the "drop `--userns=nomap`" mutant
/// (the container's root is then the daemon's uid, the file's owner), and a key file
/// any id but its owner's may read (the controls are those files, and are read). And
/// catches the key in the action's environment under a node configuration that
/// copies the host's environment in (the "start Podman with the key" mutant, if this
/// Podman honours `env_host` beside `--unsetenv-all`; `tests/credential_env.rs`
/// catches it whatever the runtime does).
#[test]
#[ignore = "needs rootless Podman and a delegated cgroup: run by cargo xtask podman-tests"]
fn an_action_cannot_read_the_cold_store_key() {
    let node = support::scratch("podman-credential-node");
    let etc = node.join("etc");
    std::fs::create_dir_all(&etc).expect("mkdir");
    std::fs::set_permissions(&etc, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    let key_file: String = KEY
        .map(|(name, value)| format!("{name}={value}\n"))
        .concat();
    write(&etc.join("s3.env"), &key_file, 0o600);
    write(&etc.join("control"), "readable by any id\n", 0o644);
    let given = etc.join("given");
    write(&given, "the container root's own\n", 0o600);
    podman(&["unshare", "chown", "1:1", given.to_str().expect("utf-8")]);

    let etc_text = etc.to_str().expect("utf-8");
    assert!(!etc_text.contains(['"', '\\', ':']), "{etc_text}");
    let conf = node.join("containers.conf");
    let volume = format!("{etc_text}:/kbf-node:ro");
    let text = format!("[containers]\nenv_host = true\nvolumes = [\"{volume}\"]\n");
    std::fs::write(&conf, text).expect("write containers.conf");
    // SAFETY: the only test of this binary, and nothing before this starts a thread,
    // so no other thread of this process reads the environment meanwhile.
    unsafe {
        std::env::set_var("CONTAINERS_CONF_OVERRIDE", &conf);
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
        let result = cell.run(1, &sh(SCRIPT)).await.expect("ran");
        let (stdout, stderr) = (cell.stdout(&result), cell.stderr(&result));
        assert!(
            !stdout.contains(KEY[1].1),
            "the action read the key: {stdout}"
        );
        assert_eq!(
            stdout,
            "control: readable by any id\n\
             given: the container root's own\n\
             s3.env: refused 1\n\
             env: unset unset\n",
            "stderr: {stderr}"
        );
        assert!(
            stderr.contains("/kbf-node/s3.env") && stderr.contains("Permission denied"),
            "{stderr}"
        );
        assert_eq!(result.exit_code, 0);
        cell.assert_clean(1);
    });
}
