//! Shared by the integration tests of this crate.

#![allow(dead_code)]

pub mod registry;

use std::path::{Path, PathBuf};

/// A fresh, empty directory for one test, under Cargo's temporary directory.
pub fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("kbf-image-{name}"));
    force_remove(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch");
    dir
}

/// Removes `dir`, first giving the owner every permission on every directory in it.
pub fn force_remove(dir: &Path) {
    use std::os::unix::fs::PermissionsExt as _;
    if std::fs::symlink_metadata(dir).is_err() {
        return;
    }
    let mut pending = vec![dir.to_owned()];
    while let Some(d) = pending.pop() {
        let _ = std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o755));
        if let Ok(entries) = std::fs::read_dir(&d) {
            for entry in entries.flatten() {
                if entry.file_type().is_ok_and(|t| t.is_dir()) {
                    pending.push(entry.path());
                }
            }
        }
    }
    let status = std::process::Command::new("rm")
        .arg("-rf")
        .arg(dir)
        .status()
        .expect("run rm");
    assert!(status.success(), "rm -rf {}", dir.display());
}
