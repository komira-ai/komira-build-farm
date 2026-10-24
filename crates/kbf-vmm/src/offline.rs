//! Probe P0: a root offline write into the Data volume of a guest's disk image, and
//! `image harvest`, the read-only copy of a fixed allowlist out of it.
//!
//! The raw image is attached with `hdiutil` (ownership honoured), the Data volume is
//! found among the mounts by what each volume is (`volume`), files are written and read
//! back (or the allowlist is copied out), and the image is detached again. macOS only;
//! the guest must be off.

use std::io::Cursor;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::{Value, json};

use crate::system::OfflineFile;
use crate::volume;

/// What `hdiutil attach -plist` reported.
#[derive(Debug, PartialEq, Eq)]
pub struct Attach {
    /// The whole-disk device node (`/dev/disk4`).
    pub device: String,
    /// Every `dev-entry`, in order.
    pub entries: Vec<String>,
    pub mounts: Vec<PathBuf>,
}

/// Reads the plist `hdiutil attach -plist` prints.
///
/// # Errors
/// The text is not that plist, or it names no device.
pub fn parse_attach(plist_bytes: &[u8]) -> Result<Attach, String> {
    let value = plist::Value::from_reader(Cursor::new(plist_bytes))
        .map_err(|e| format!("not a plist: {e}"))?;
    let entities = value
        .as_dictionary()
        .and_then(|d| d.get("system-entities"))
        .and_then(plist::Value::as_array)
        .ok_or("no system-entities in the attach output")?;
    let mut entries = Vec::new();
    let mut mounts = Vec::new();
    for e in entities {
        let Some(d) = e.as_dictionary() else { continue };
        if let Some(dev) = d.get("dev-entry").and_then(plist::Value::as_string) {
            entries.push(dev.to_owned());
        }
        if let Some(m) = d.get("mount-point").and_then(plist::Value::as_string) {
            mounts.push(PathBuf::from(m));
        }
    }
    let device = entries
        .first()
        .cloned()
        .ok_or("the attach output names no device")?;
    Ok(Attach {
        device,
        entries,
        mounts,
    })
}

/// The first whole-disk device node (`/dev/diskN`) named anywhere in `output`.
#[must_use]
pub fn whole_disk(output: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(output);
    let at = text.find("/dev/disk")?;
    let digits: String = text[at + "/dev/disk".len()..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    (!digits.is_empty()).then(|| format!("/dev/disk{digits}"))
}

fn run(program: &str, args: &[&str]) -> Result<Vec<u8>, String> {
    let out = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("run {program}: {e}"))?;
    if out.status.success() {
        Ok(out.stdout)
    } else {
        Err(format!(
            "{program} {} failed ({}): {}",
            args.join(" "),
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

fn put(root: &Path, f: &OfflineFile) -> Result<Value, String> {
    let path = root.join(&f.path);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    std::fs::write(&path, &f.contents).map_err(|e| format!("write {}: {e}", path.display()))?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(f.mode))
        .map_err(|e| format!("chmod {}: {e}", path.display()))?;
    let meta = std::fs::metadata(&path).map_err(|e| format!("stat {}: {e}", path.display()))?;
    Ok(json!({
        "path": f.path,
        "uid": meta.uid(),
        "gid": meta.gid(),
        "mode": format!("{:o}", meta.mode() & 0o7777),
        "bytes": meta.len(),
    }))
}

/// Attaches `disk` (read-only when `readonly`), runs `f` on the Data volume, and detaches.
/// Returns what `f` returned with the attach facts added.
fn with_data_volume(
    disk: &Path,
    readonly: bool,
    f: impl FnOnce(&Path) -> Result<Value, String>,
) -> Result<Value, String> {
    let disk_s = disk.to_string_lossy();
    let mut args = vec![
        "attach",
        "-imagekey",
        "diskimage-class=CRawDiskImage",
        "-owners",
        "on",
        "-nobrowse",
        "-plist",
    ];
    if readonly {
        args.push("-readonly");
    }
    args.push(&disk_s);
    let out = run("/usr/bin/hdiutil", &args)?;
    let attach = match parse_attach(&out) {
        Ok(a) => a,
        Err(e) => {
            // The image is attached whether or not the output could be read: find the
            // whole-disk device in the raw text and detach it, or later starts fail on a
            // locked disk.
            let detached =
                whole_disk(&out).map(|dev| run("/usr/bin/hdiutil", &["detach", "-force", &dev]));
            return Err(match detached {
                Some(Ok(_)) => format!("{e}; the image was detached again"),
                Some(Err(d)) => format!("{e}; and detaching it failed: {d}"),
                None => format!(
                    "{e}; no device in the output, so the image may still be attached (see `hdiutil info`)"
                ),
            });
        }
    };
    let result = volume::data_volume(&attach.mounts).and_then(|root| {
        let mut observed = f(&root)?;
        observed["data_volume"] = json!(root);
        Ok::<_, String>(observed)
    });
    let detached = run("/usr/bin/hdiutil", &["detach", "-force", &attach.device]);
    let mut observed = result?;
    observed["attached"] = json!(attach.entries);
    observed["mounts"] = json!(attach.mounts);
    observed["detached"] = json!(detached.is_ok());
    if let Err(e) = detached {
        observed["detach_error"] = json!(e);
    }
    Ok(observed)
}

/// Attaches `disk`, writes `files` into its Data volume, and detaches.
///
/// # Errors
/// A step failed; the image is detached whenever the attach worked.
pub fn write(disk: &Path, files: &[OfflineFile]) -> Result<Value, String> {
    with_data_volume(disk, false, |root| {
        let written = files
            .iter()
            .map(|f| put(root, f))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(json!({"written": written}))
    })
}

/// Attaches `disk` read-only, copies the allowlist from its Data volume into the new
/// directory `out`, and detaches.
///
/// # Errors
/// A step failed; the image is detached whenever the attach worked.
pub fn harvest(disk: &Path, out: &Path) -> Result<Value, String> {
    with_data_volume(disk, true, |root| {
        Ok(
            json!({"harvest": crate::harvest::copy_allowlist(root, out, &crate::harvest::Limits::DEFAULT)?}),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict><key>system-entities</key><array>
<dict><key>content-hint</key><string>GUID_partition_scheme</string><key>dev-entry</key><string>/dev/disk4</string></dict>
<dict><key>dev-entry</key><string>/dev/disk4s1</string></dict>
<dict><key>dev-entry</key><string>/dev/disk5s1</string><key>mount-point</key><string>/Volumes/Data</string></dict>
</array></dict></plist>"#;

    /// Catches a parser that takes the first mount-less entry as a mount, or loses the whole-disk device.
    #[test]
    fn attach_output_gives_the_device_entries_and_mounts() {
        let a = parse_attach(SAMPLE.as_bytes()).expect("parse");
        assert_eq!(a.device, "/dev/disk4");
        assert_eq!(a.entries.len(), 3);
        assert_eq!(a.mounts, vec![PathBuf::from("/Volumes/Data")]);
    }

    /// Catches a device lookup that returns a slice (`/dev/disk4s1`) instead of the whole
    /// disk, finds nothing in unreadable output, or takes `/dev/disk` with no number.
    #[test]
    fn the_whole_disk_is_found_in_text_that_is_not_a_plist() {
        assert_eq!(
            whole_disk(b"garbage /dev/disk4s1 more /dev/disk5").as_deref(),
            Some("/dev/disk4")
        );
        assert_eq!(whole_disk(b"/dev/diskX"), None);
        assert_eq!(whole_disk(b"nothing"), None);
    }

    /// Catches a parser that accepts a plist with no entities as an attach.
    #[test]
    fn attach_output_without_devices_is_refused() {
        let bare = b"<?xml version=\"1.0\"?><plist version=\"1.0\"><dict><key>system-entities</key><array/></dict></plist>";
        assert!(parse_attach(bare).is_err());
        assert!(parse_attach(b"not a plist").is_err());
    }
}
