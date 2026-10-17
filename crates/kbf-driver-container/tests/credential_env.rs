//! The node's cold-store key does not reach an action through the runtime's
//! environment, against the stand-in `podman` and `crun` (`support/fake.rs`,
//! `support/fake_oci.rs`). Each stand-in runs the action with its own environment, as a
//! runtime that copies the host's into a container would, so what the action sees is
//! what the driver started the runtime with. The stand-in `podman` hands on what it
//! was run with at `create` too (its `env-host` knob), as Podman's `env_host` copies
//! the host's environment when the container is created. Real `crun` hands on only the
//! config's `process.env`, which the stand-in records and this test reads.
//!
//! One test in this binary: it sets variables of its own process, which is sound only
//! while no other thread reads the environment. `tests/podman_credential.rs` checks
//! the same on real rootless Podman, with the key file besides.

mod support;

use support::Spec;
use support::fake::{Fake, image};
use support::fake_oci::FakeOci;

/// The key's two variables, as the unit's `EnvironmentFile=` sets them.
const KEY: [(&str, &str); 2] = [
    ("AWS_ACCESS_KEY_ID", "kbf-test-cold-store-key-id"),
    ("AWS_SECRET_ACCESS_KEY", "kbf-test-cold-store-secret"),
];

/// A variable of the node that is not a credential: the stand-ins hand it on.
const NODE_VARIABLE: (&str, &str) = ("KBF_TEST_NODE_VARIABLE", "handed-on");

/// Act steps printing each of the key's variables, then [`NODE_VARIABLE`], each
/// `unset` when the action does not have it.
fn print_env() -> Vec<String> {
    let mut steps = Vec::new();
    for (name, _) in KEY.into_iter().chain([NODE_VARIABLE]) {
        if !steps.is_empty() {
            steps.push(";".to_owned());
        }
        steps.extend(["env", name, "unset"].map(str::to_owned));
    }
    steps
}

/// The variables of `record` (the stand-in `podman`'s `create-env`: `NAME=value`, each
/// followed by a NUL).
fn recorded(record: &std::path::Path) -> Vec<String> {
    let bytes = std::fs::read(record).expect("create-env");
    String::from_utf8_lossy(&bytes)
        .split('\0')
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Catches Podman or `crun` started with the key in its environment (the "drop
/// `without_credentials`" mutant at each start site the lease goes through): the
/// stand-in hands it to the action, which prints it. That includes `podman create`
/// (the "scrub only in `start`" mutant), whose environment the stand-in hands on
/// under `env-host` as real Podman does; the OCI config's `process.env` (the "add this
/// process's `AWS_*` to it" mutant in `oci::spec::action_config`), what real `crun`
/// gives the action; and the `podman create` that unpacks an image for the OCI
/// backend. [`NODE_VARIABLE`] is the control: it reaches the action and each record,
/// so an `unset` for the key is the driver's doing, not a stand-in that hands nothing
/// on, and `HOME=/root` in `process.env` shows the config's variables were read.
#[test]
fn the_runtime_is_started_without_the_cold_store_key() {
    // SAFETY: the only test of this binary, and the first thing it does: no other
    // thread of this process runs yet, so none reads the environment meanwhile.
    unsafe {
        for (name, value) in KEY.into_iter().chain([NODE_VARIABLE]) {
            std::env::set_var(name, value);
        }
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let want = "unset\nunset\nhanded-on\n";
    runtime.block_on(async {
        let spec = Spec::new(&image(), "unused");

        let node = format!("{}={}", NODE_VARIABLE.0, NODE_VARIABLE.1);
        let no_key = |env: &[String], what: &str| {
            assert!(env.contains(&node), "{what}: no {node} in {env:?}");
            let keys: Vec<_> = env.iter().filter(|v| v.starts_with("AWS_")).collect();
            assert!(keys.is_empty(), "{what}: {keys:?}");
        };

        let fake = Fake::new("credential-env");
        fake.knob("env-host", "");
        let result = fake.run(1, &spec, &print_env()).await.expect("ran");
        let stdout = support::blob(&fake.cas, result.stdout_digest.as_ref());
        assert_eq!(String::from_utf8_lossy(&stdout), want, "podman");
        assert_eq!(result.exit_code, 0);
        no_key(&recorded(&fake.state.join("create-env")), "podman create");
        fake.assert_clean(1);

        let oci = FakeOci::new("credential-env");
        oci.pulled_not_unpacked();
        std::fs::write(oci.podman_state.join("env-host"), "").expect("knob");
        let result = oci.run(1, &spec, &print_env()).await.expect("ran");
        let stdout = support::blob(&oci.cas, result.stdout_digest.as_ref());
        assert_eq!(String::from_utf8_lossy(&stdout), want, "crun");
        assert_eq!(result.exit_code, 0);
        let process_env: Vec<String> = oci.config_json()["process"]["env"]
            .as_array()
            .expect("process.env")
            .iter()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect();
        assert!(
            process_env.contains(&"HOME=/root".to_owned()),
            "{process_env:?}"
        );
        let keys: Vec<_> = process_env
            .iter()
            .filter(|v| v.starts_with("AWS_"))
            .collect();
        assert!(keys.is_empty(), "process.env: {keys:?}");
        no_key(
            &recorded(&oci.podman_state.join("create-env")),
            "podman create (unpack)",
        );
        oci.assert_clean(1);
    });
}
