//! Rule `traverse` of [`super::modes`]: the directory a unit's `--hot-store=file://` names
//! can be reached by the unit's `User=`. Every directory above it that exists must give
//! that user the execute bit, as its owner, through one of its groups, or as other (the
//! kernel's order: the first class that matches decides). One that does not is how a
//! `storage-and-daemon` unit, running as `kbf`, failed to start with
//! `--hot-store /var/lib/kbf-server/hot: Permission denied`: the parent is the server
//! account's, mode 0700.
//!
//! Read on the node, from `lstat` (a directory that is a symbolic link is not followed,
//! so it is not judged; nor are access control lists, capabilities or a mount's flags).
//! The directory itself is not judged: `install-units --mode` makes it the user's, and
//! a missing directory and the ones below it are made 0755 beside it.

use std::io;

use kbf_host::exec::{Kind, Meta};
use kbf_host::path::HostPath;
use kbf_host::user::RunUser;

/// The first existing directory above `dir` that `user` cannot enter, with its mode and
/// owner; none when `user` can reach `dir`. `stat` reads one path (none: it is missing,
/// and so are the ones below it).
pub(super) fn blocked_by(
    user: &RunUser,
    dir: &HostPath,
    mut stat: impl FnMut(&HostPath) -> io::Result<Option<Meta>>,
) -> io::Result<Option<(HostPath, Meta)>> {
    let s = dir.as_str();
    // `/` itself is not a `HostPath`, and is not judged.
    for (i, _) in s.match_indices('/').skip(1) {
        let ancestor = &s[..i];
        let path = HostPath::new(ancestor).expect("a prefix of an absolute path is one");
        let Some(meta) = stat(&path)? else {
            return Ok(None);
        };
        if meta.kind == Kind::Dir && !can_enter(user, &meta) {
            return Ok(Some((path, meta)));
        }
    }
    Ok(None)
}

/// Whether `user` has the execute bit on a directory with `meta`: the owner's bit for
/// the owner, else the group's for a member of its group, else other's.
fn can_enter(user: &RunUser, meta: &Meta) -> bool {
    let bit = if meta.owner.uid == user.uid {
        0o100
    } else if user.groups.contains(&meta.owner.gid) || user.gid == meta.owner.gid {
        0o010
    } else {
        0o001
    };
    meta.perms.bits() & bit != 0
}

/// The refusal for [`blocked_by`]'s answer.
pub(super) fn message(user: &RunUser, hot: &HostPath, dir: &HostPath, meta: &Meta) -> String {
    format!(
        "--hot-store=file://{hot}: {dir} (mode {}, uid {} gid {}) cannot be entered by {}, \
         the unit's User=: chmod o+x {dir}, or set KBF_HOT_DIR to a directory {} can reach \
         (the default is under KBF_DATA_DIR)",
        meta.perms, meta.owner.uid, meta.owner.gid, user.name, user.name
    )
}

#[cfg(test)]
#[path = "traverse_tests.rs"]
mod tests;
