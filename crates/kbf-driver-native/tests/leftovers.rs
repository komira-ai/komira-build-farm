//! What actions leave in the user folders (`kbf_driver_native::user_folders`) is swept
//! at start and after every lease, once it is old enough to be no lease's save in
//! progress. Here the folders are stand-ins in the test's scratch directory, so the
//! test runs on every platform; `tests/user_folders.rs` uses the real ones on macOS.

mod support;

use std::path::Path;
use std::sync::Arc;

use kbf_driver_native::user_folders::{LEFTOVER_AGE, UserFolders};
use support::{MemoryCas, Spec, config, run, runtime, scratch, stderr};

/// Makes `path` a directory holding a file, last changed in 2000 (old) or now.
fn leftover(path: &Path, old: bool) {
    std::fs::create_dir_all(path).expect("dir");
    std::fs::write(path.join("f"), b"x").expect("file");
    if old {
        let status = std::process::Command::new("touch")
            .args(["-t", "200001010000"])
            .arg(path)
            .status()
            .expect("touch");
        assert!(status.success());
    }
}

/// Catches a leftover of a previous daemon kept past start, one an action leaves kept
/// past its lease (the next lease sees it), and a young one (another lease's save in
/// progress) removed by either sweep.
#[tokio::test]
async fn old_leftovers_are_swept_at_start_and_after_each_lease() {
    let dir = scratch("leftovers");
    let (temp, cache) = (dir.join("T"), dir.join("C"));
    let items = temp.join("TemporaryItems");
    std::fs::create_dir_all(&cache).expect("C");
    leftover(&items.join("from-before"), true);
    leftover(&items.join("in-progress"), false);
    let mut config = config(&dir);
    config.user_folders = UserFolders::new(temp, cache);
    assert!(config.user_folders.is_some());
    assert_eq!(config.leftover_age, LEFTOVER_AGE);
    let cas = Arc::new(MemoryCas::default());
    let rt = runtime(config, &cas);
    assert!(!items.join("from-before").exists(), "kept past start");
    assert!(items.join("in-progress").exists(), "removed at start");

    let left = items.join("by-a-lease");
    let spec = Spec::sh(&format!(
        "mkdir -p '{0}' && echo x > '{0}/f' && touch -t 200001010000 '{0}'",
        left.display()
    ))
    .env("PATH", "/usr/bin:/bin");
    let result = run(&rt, &cas, 1, &spec).await.expect("ran");
    assert_eq!(result.exit_code, 0, "{}", stderr(&cas, &result));
    assert!(!left.exists(), "kept past the lease");
    assert!(
        items.join("in-progress").exists(),
        "removed after the lease"
    );
}
