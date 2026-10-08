//! Files the gate trusts because of who owns them: the allowed signers, the inventory,
//! the profile allowlist and profiles, the root key. They are root-owned and writable
//! by nobody else (M4.3: adding a key is an edit that needs root on the gate's host).
//! The check is made on the opened file, so a swap between check and read is caught.

use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

/// Why a file was not read.
#[derive(Debug, thiserror::Error)]
enum Why {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("not a regular file")]
    NotFile,
    #[error("owned by uid {0}, not {1}")]
    Owner(u32, u32),
    #[error("mode {0:o} lets others write it")]
    Mode(u32),
}

fn read_checked(path: &Path, owner: u32) -> Result<Vec<u8>, Why> {
    let mut file = std::fs::File::open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(Why::NotFile);
    }
    if meta.uid() != owner {
        return Err(Why::Owner(meta.uid(), owner));
    }
    if meta.mode() & 0o022 != 0 {
        return Err(Why::Mode(meta.mode() & 0o777));
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// Reads `path` if it is a regular file owned by `owner` and not writable by its group
/// or others. The binary passes uid 0 unless configured otherwise for tests.
///
/// # Errors
/// The file cannot be read, or fails the ownership check; the message names the path
/// and says which.
pub fn read(path: &Path, owner: u32) -> Result<Vec<u8>, String> {
    read_checked(path, owner).map_err(|why| format!("{}: {why}", path.display()))
}

/// [`read`], as UTF-8 text.
///
/// # Errors
/// As [`read`], or the file is not UTF-8.
pub fn read_text(path: &Path, owner: u32) -> Result<String, String> {
    String::from_utf8(read(path, owner)?).map_err(|_| format!("{}: not UTF-8", path.display()))
}

/// The uid that owns `path` (tests use it as "this user").
///
/// # Errors
/// The path cannot be read.
pub fn owner_of(path: &Path) -> std::io::Result<u32> {
    Ok(std::fs::metadata(path)?.uid())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[test]
    fn only_files_owned_by_the_owner_and_closed_to_others_are_read() {
        // Catches: reading an allowed-signers file another user can write, which would
        // let that user add a key and sign erases.
        let dir = crate::testkit::scratch("trusted");
        let me = owner_of(&dir).unwrap();
        let path = dir.join("signers");
        std::fs::write(&path, b"text").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(read_text(&path, me).unwrap(), "text");
        let e = read(&path, me + 1).unwrap_err();
        assert!(
            e.ends_with(&format!("owned by uid {me}, not {}", me + 1)),
            "{e}"
        );
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o664)).unwrap();
        assert!(
            read(&path, me)
                .unwrap_err()
                .ends_with("mode 664 lets others write it")
        );
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o646)).unwrap();
        assert!(read(&path, me).is_err());
        assert!(read(&dir, me).unwrap_err().ends_with("not a regular file"));
        assert!(read(&dir.join("missing"), me).is_err());
        let binary = dir.join("binary");
        std::fs::write(&binary, [0xff, 0xfe]).unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(read_text(&binary, me).unwrap_err().ends_with("not UTF-8"));
    }
}
