//! The lease's own home, temporary and cache directories, and the variables that name
//! them.
//!
//! Each lease directory holds, beside the input root:
//!
//! | Directory | Variables |
//! |---|---|
//! | `home` | `HOME` |
//! | `tmp` | `TMPDIR` |
//! | `cache` | `XDG_CACHE_HOME`; `CLANG_MODULE_CACHE_PATH` is `cache/clang/ModuleCache` |
//!
//! So a tool that writes under `~`, the temporary directory or a per-user cache
//! (`clang` and `swiftc` keep their module caches there) writes inside the lease, and
//! the lease's removal takes it away: nothing one action leaves there is seen by the
//! next. A variable the Command sets itself is the Command's: these are set first and
//! the Command's environment after them.

use std::ffi::OsString;
use std::path::Path;

/// The lease directory's subdirectories and the variable naming each, in the order
/// they are made. A path is relative to the lease directory.
const DIRS: [(&str, &str); 4] = [
    ("home", "HOME"),
    ("tmp", "TMPDIR"),
    ("cache", "XDG_CACHE_HOME"),
    ("cache/clang/ModuleCache", "CLANG_MODULE_CACHE_PATH"),
];

/// Makes the lease's home, temporary and cache directories under `dir` (the lease
/// directory) and returns the variables that name them.
///
/// # Errors
/// A directory cannot be made.
pub(crate) async fn make(dir: &Path) -> std::io::Result<Vec<(&'static str, OsString)>> {
    let mut vars = Vec::new();
    for (sub, name) in DIRS {
        let path = dir.join(sub);
        tokio::fs::create_dir_all(&path).await?;
        vars.push((name, path.into_os_string()));
    }
    Ok(vars)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::current_exe()
            .expect("test binary")
            .parent()
            .expect("deps")
            .join("kbf-driver-native-unit")
            .join(format!("home-{name}-{}", std::process::id()));
        // Absent unless a run with this pid left it.
        let _ = kbf_outputs::remove_tree(&dir);
        std::fs::create_dir_all(&dir).expect("scratch");
        dir
    }

    /// Catches: a variable that names a directory that is not made (a tool that writes
    /// its cache there fails), or one outside the lease directory.
    #[tokio::test]
    async fn every_variable_names_a_directory_inside_the_lease() {
        let dir = scratch("made");
        let vars = make(&dir).await.expect("made");
        let names: Vec<&str> = vars.iter().map(|(name, _)| *name).collect();
        assert_eq!(
            names,
            [
                "HOME",
                "TMPDIR",
                "XDG_CACHE_HOME",
                "CLANG_MODULE_CACHE_PATH"
            ]
        );
        for (name, path) in &vars {
            let path = Path::new(path);
            assert!(path.starts_with(&dir), "{name}={}", path.display());
            assert!(path.is_dir(), "{name}={}", path.display());
        }
        kbf_outputs::remove_tree(&dir).expect("clean");
    }

    /// Catches: a lease directory that cannot hold the directories (here, a file in
    /// its place) passed over silently.
    #[tokio::test]
    async fn a_directory_that_cannot_be_made_is_an_error() {
        let dir = scratch("file");
        let file = dir.join("lease");
        std::fs::write(&file, b"not a directory").expect("file");
        assert!(make(&file).await.is_err());
        kbf_outputs::remove_tree(&dir).expect("clean");
    }
}
