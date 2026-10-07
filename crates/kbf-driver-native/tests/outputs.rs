//! Outputs through the native driver follow the Linux rules: no symlink followed at
//! any level, and the limits fail the action.

mod support;

use std::sync::Arc;

use kbf_daemon::{Runtime, RuntimeError};
use support::{MemoryCas, Spec, config, no_leases, run, runtime, scratch};

/// Catches: an output read through a symlink the action planted (the working
/// directory replaced by a link to a host directory, a link on an output's path, a
/// link inside an output directory), which would upload host files the action could
/// not otherwise reach.
#[tokio::test]
async fn no_symlink_the_action_plants_is_followed() {
    let dir = scratch("symlinks");
    let host = dir.join("host");
    std::fs::create_dir_all(host.join("sub")).expect("mkdir");
    std::fs::write(host.join("secret"), b"host secret").expect("write");
    std::fs::write(host.join("sub/secret"), b"host secret").expect("write");
    let config = config(&dir);
    let cas = Arc::new(MemoryCas::default());
    let rt = runtime(config.clone(), &cas);
    let host_text = host.to_str().expect("utf8");

    let script = "mkdir -p out && ln -s \"$HOST\" out/link && ln -s \"$HOST\" via && ln -s \"$HOST/secret\" file";
    let spec = Spec::sh(script)
        .env("HOST", host_text)
        .outputs(&["out", "via/secret", "file"]);
    let result = run(&rt, &cas, 1, &spec).await.expect("ran");
    assert!(result.output_files.is_empty(), "{result:?}");
    assert_eq!(result.output_symlinks.len(), 1);
    assert_eq!(
        result.output_symlinks[0].target,
        format!("{host_text}/secret")
    );
    let digest = result.output_directories[0]
        .tree_digest
        .as_ref()
        .expect("tree");
    let root = support::tree(&cas, digest).root.expect("root");
    assert!(
        root.files.is_empty() && root.directories.is_empty(),
        "{root:?}"
    );
    assert_eq!(root.symlinks[0].name, "link");

    // The working directory itself replaced by a link to the host directory.
    let mut spec = Spec::sh("cd .. && rm -rf w && ln -s \"$HOST\" w")
        .env("HOST", host_text)
        .outputs(&["secret", "sub"]);
    spec.working_directory = "w".to_owned();
    let result = run(&rt, &cas, 2, &spec).await.expect("ran");
    assert!(result.output_files.is_empty(), "{result:?}");
    assert!(result.output_directories.is_empty(), "{result:?}");
    assert!(no_leases(&config));
}

/// Catches: the output limits not applied by the native driver, or an output past
/// them reported as the client's error rather than a failed lease.
#[tokio::test]
async fn outputs_past_a_limit_fail_the_lease() {
    let dir = scratch("limits");
    let mut config = config(&dir);
    config.outputs.max_bytes = 10;
    let cas = Arc::new(MemoryCas::default());
    let rt = runtime(config.clone(), &cas);
    let spec = Spec::sh("printf 'eleven byte' > f").outputs(&["f"]);
    let outcome = run(&rt, &cas, 1, &spec).await;
    assert!(
        matches!(&outcome, Err(RuntimeError::Failed(why)) if why.contains("--output-max-bytes")),
        "{outcome:?}"
    );
    assert!(no_leases(&config));
}

/// Catches: stdout past its limit stored anyway (a runaway log would fill the CAS),
/// or its limit not named; and an upload failure of the captured output swallowed.
#[tokio::test]
async fn stdout_past_its_limit_and_a_failed_upload_fail_the_lease() {
    let dir = scratch("stdio");
    let mut config = config(&dir);
    config.outputs.max_stdio_bytes = 4;
    let cas = Arc::new(MemoryCas::default());
    let rt = runtime(config.clone(), &cas);
    let outcome = run(&rt, &cas, 1, &Spec::sh("echo too long")).await;
    assert!(
        matches!(&outcome, Err(RuntimeError::Failed(why)) if why.contains("--output-max-stdio-bytes")),
        "{outcome:?}"
    );
    let refusing = Arc::new(MemoryCas {
        refuse_puts: true,
        ..MemoryCas::default()
    });
    let action = Spec::sh("echo hi").store(&refusing);
    let rt = runtime(support::config(&dir), &refusing);
    let outcome = rt.run(support::work(2, action, 0)).await;
    assert!(
        matches!(&outcome, Err(RuntimeError::Failed(why)) if why.contains("the CAS is down")),
        "{outcome:?}"
    );
    assert!(no_leases(&config));
}
