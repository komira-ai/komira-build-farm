//! The group of accounts that may use the socket besides the frontend's own.

use std::fs::{self, Permissions};
use std::os::unix::fs::{PermissionsExt, chown};
use std::path::{Path, PathBuf};

use crate::error::SignerError;

/// Where group membership is read: a `passwd` file and a `group` file, in their usual
/// colon-separated formats.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Accounts {
    passwd: PathBuf,
    group: PathBuf,
}

impl Accounts {
    /// `/etc/passwd` and `/etc/group`.
    #[must_use]
    pub fn system() -> Self {
        Self::in_dir(Path::new("/etc"))
    }

    /// `passwd` and `group` in `dir`: stand-ins for the system files, for tests.
    #[must_use]
    pub fn in_dir(dir: &Path) -> Self {
        Self {
            passwd: dir.join("passwd"),
            group: dir.join("group"),
        }
    }
}

/// The fields of the lines of a colon-separated database; blank lines and comments are
/// skipped.
fn records(path: &Path) -> std::io::Result<Vec<Vec<String>>> {
    Ok(fs::read_to_string(path)?
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
        .map(|l| l.split(':').map(str::to_owned).collect())
        .collect())
}

/// A named group whose members may use the socket (`--client-group`).
///
/// Membership is read from the account files again at every connection, so an account
/// added to or removed from the group takes effect without a restart. A file that cannot
/// be read admits nobody.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientGroup {
    gid: u32,
    accounts: Accounts,
}

impl ClientGroup {
    /// The group called `name` in `accounts`.
    ///
    /// # Errors
    /// [`SignerError::NoSuchGroup`] when there is no such group (an empty name is none);
    /// [`SignerError::Io`] when the group file cannot be read.
    pub fn resolve(name: &str, accounts: Accounts) -> Result<Self, SignerError> {
        let gid = records(&accounts.group)?
            .iter()
            .find(|f| f[0] == name && !name.is_empty())
            .and_then(|f| number(f, 2))
            .ok_or_else(|| SignerError::NoSuchGroup(name.to_owned()))?;
        Ok(Self { gid, accounts })
    }

    /// The group's id.
    #[must_use]
    pub fn gid(&self) -> u32 {
        self.gid
    }

    /// Whether a peer with user id `uid` and effective group id `gid` belongs to the
    /// group: its effective group is this one, or an account with its user id has this
    /// group as its primary group or is listed among the group's members.
    #[must_use]
    pub fn admits(&self, uid: u32, gid: u32) -> bool {
        if gid == self.gid {
            return true;
        }
        let (Ok(passwd), Ok(group)) = (
            records(&self.accounts.passwd),
            records(&self.accounts.group),
        ) else {
            return false;
        };
        let members: Vec<&str> = group
            .iter()
            .filter(|f| number(f, 2) == Some(self.gid))
            .flat_map(|f| f.get(3).map_or("", String::as_str).split(','))
            .collect();
        passwd
            .iter()
            .filter(|f| number(f, 2) == Some(uid))
            .any(|f| number(f, 3) == Some(self.gid) || members.contains(&f[0].as_str()))
    }
}

/// Field `i` of a record as a number.
fn number(fields: &[String], i: usize) -> Option<u32> {
    fields.get(i)?.parse().ok()
}

/// Gives `path` to group `gid` (its owner stays) and sets its mode. The account must own
/// `path` and be a member of the group, or be root.
pub(crate) fn share(path: &Path, gid: u32, mode: u32) -> Result<(), SignerError> {
    chown(path, None, Some(gid))
        .and_then(|()| fs::set_permissions(path, Permissions::from_mode(mode)))
        .map_err(|source| SignerError::Share {
            path: path.to_path_buf(),
            source,
        })
}
