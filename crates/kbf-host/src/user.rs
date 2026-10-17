//! Running a tool as another user: who the user is, read from the host while planning,
//! and the switch the tool's process makes before it starts.
//!
//! A root plan that must act as a service user (a tool that reads or fills the kbf user's own files)
//! gives its [`Cmd`] a [`RunUser`]. Between fork and exec the child sets the user's
//! supplementary groups, then its group, then its uid, as `sudo -u` does. The groups
//! come first because each call needs the rights the next one gives up, and they are
//! set explicitly: `std::process::Command::uid` alone clears them (`setgroups(0)`), so
//! the tool would run without the groups a login gives the user.

use std::io;

use crate::exec::{Exec, succeeded};
use crate::path::HostPath;
use crate::step::Cmd;

/// `getent`, which reads the host's user database as a login does (files, LDAP, ...).
pub const GETENT: &str = "/usr/bin/getent";
/// `id`, for a user's groups (`id -G`).
pub const ID: &str = "/usr/bin/id";

/// A user a command runs as: the uid, the primary gid, every group a login gives the
/// user (the primary one included) and the home directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunUser {
    pub name: String,
    pub uid: u32,
    pub gid: u32,
    pub groups: Vec<u32>,
    pub home: String,
}

/// Why a user could not be looked up.
#[derive(Debug, thiserror::Error)]
pub enum UserError {
    #[error("{cmd}: {source}")]
    Query { cmd: String, source: io::Error },
    #[error("{cmd} printed {out:?}, which is not what this kit reads")]
    Output { cmd: String, out: String },
}

/// `name` as the host's user database has it: `getent passwd` for the uid, gid and
/// home, `id -G` for the groups. `None` when there is no such user (`getent` exits 2).
pub fn lookup<E: Exec>(exec: &E, name: &str) -> Result<Option<RunUser>, UserError> {
    let getent = Cmd::new(tool(GETENT)).arg("passwd").arg("--").arg(name);
    let out = exec.query(&getent).map_err(|source| UserError::Query {
        cmd: getent.to_string(),
        source,
    })?;
    if out.status.code() == Some(2) {
        return Ok(None);
    }
    let out = succeeded(&getent, out).map_err(|source| UserError::Query {
        cmd: getent.to_string(),
        source,
    })?;
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let bad = || UserError::Output {
        cmd: getent.to_string(),
        out: text.clone(),
    };
    // name:password:uid:gid:gecos:home:shell
    let fields: Vec<&str> = text.trim_end_matches('\n').split(':').collect();
    let [n, _, uid, gid, _, home, _] = fields[..] else {
        return Err(bad());
    };
    let (Ok(uid), Ok(gid)) = (uid.parse(), gid.parse()) else {
        return Err(bad());
    };
    if n != name {
        return Err(bad());
    }
    let home = home.to_owned();

    let id = Cmd::new(tool(ID)).arg("-G").arg("--").arg(name);
    let query = |cmd: &Cmd| {
        succeeded(cmd, exec.query(cmd)?)
            .map(|out| String::from_utf8_lossy(&out.stdout).into_owned())
    };
    let text = query(&id).map_err(|source| UserError::Query {
        cmd: id.to_string(),
        source,
    })?;
    let groups: Result<Vec<u32>, _> = text.split_whitespace().map(str::parse).collect();
    let groups = groups
        .ok()
        .filter(|g| g.contains(&gid))
        .ok_or(UserError::Output {
            cmd: id.to_string(),
            out: text.clone(),
        })?;
    Ok(Some(RunUser {
        name: name.to_owned(),
        uid,
        gid,
        groups,
        home,
    }))
}

fn tool(path: &str) -> HostPath {
    HostPath::new(path).expect("a tool's path is absolute")
}

/// What a child runs between fork and exec to become `user`: [`become_user`] on a copy of
/// its ids, made here in the parent so the child allocates nothing.
pub(crate) fn switch_to(user: &RunUser) -> impl FnMut() -> io::Result<()> + Send + Sync + 'static {
    let (uid, gid, groups) = (user.uid, user.gid, user.groups.clone());
    move || become_user(uid, gid, &groups)
}

/// Switch the calling process to `uid`, `gid` and `groups`: the supplementary groups,
/// then the group, then the user. Called in a child between fork and exec, so it makes
/// three system calls and allocates nothing.
pub(crate) fn become_user(uid: u32, gid: u32, groups: &[u32]) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    let n: libc::size_t = groups.len();
    #[cfg(not(target_os = "linux"))]
    let n = libc::c_int::try_from(groups.len())
        .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    // SAFETY: `groups` holds `n` gids for the call to read; it does not keep the pointer.
    if unsafe { libc::setgroups(n, groups.as_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: setgid and setuid take any number; one the caller may not take fails.
    if unsafe { libc::setgid(gid) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: as above.
    if unsafe { libc::setuid(uid) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
#[path = "user_tests.rs"]
mod tests;
