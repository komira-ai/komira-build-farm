//! The macOS host: user records through OpenDirectory's local node, launchd through
//! `launchctl`, processes through `libproc` and `kill`.

use std::collections::BTreeSet;
use std::io;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, SystemTime};

use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::AnyObject;
use objc2_foundation::{NSError, NSString};
use objc2_open_directory::{ODNode, ODRecord, ODSession, kODNodeTypeLocalNodes};
use security_framework::random::SecRandom;

use crate::helper::{self, Host, NewUser};

const USERS: &str = "dsRecTypeStandard:Users";
const GROUPS: &str = "dsRecTypeStandard:Groups";
/// The group whose members are administrators.
const ADMIN_GROUP: &str = "admin";
/// A new lease user's login shell.
const SHELL: &str = "/bin/zsh";

/// `libproc`'s filters (`sys/proc_info.h`): processes by effective and by real uid.
const PROC_UID_ONLY: u32 = 4;
const PROC_RUID_ONLY: u32 = 5;
/// `pbi_status` of a process that has exited and waits to be reaped.
const SZOMB: u32 = 5;

/// The macOS host.
#[derive(Clone, Copy, Debug, Default)]
pub struct MacHost;

impl Host for MacHost {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }

    fn uid_taken(&self, uid: u32) -> io::Result<bool> {
        // SAFETY: a zeroed passwd is a valid value for getpwuid_r to overwrite.
        let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
        let mut buffer = vec![0 as libc::c_char; 1 << 16];
        let mut found: *mut libc::passwd = std::ptr::null_mut();
        // SAFETY: every pointer is to a live, writable value of the right type, and
        // `buffer.len()` is the buffer's size.
        let code = unsafe {
            libc::getpwuid_r(
                uid,
                &raw mut entry,
                buffer.as_mut_ptr(),
                buffer.len(),
                &raw mut found,
            )
        };
        if code != 0 {
            return Err(io::Error::from_raw_os_error(code));
        }
        Ok(!found.is_null())
    }

    fn make_home(&self, homes: &Path, name: &str, uid: u32, gid: u32) -> io::Result<()> {
        helper::make_home(homes, name, uid, gid)
    }

    fn create_user(&self, user: &NewUser) -> io::Result<()> {
        autoreleasepool(|_| {
            let node = local_node()?;
            let mut error: Option<Retained<NSError>> = None;
            // SAFETY: the record type and name are NSStrings; no attributes yet.
            let record = unsafe {
                node.createRecordWithRecordType_name_attributes_error(
                    Some(&NSString::from_str(USERS)),
                    Some(&NSString::from_str(&user.name)),
                    None,
                    Some(&mut error),
                )
            }
            .ok_or_else(|| od_error("creating the record", error))?;
            let home = user.home.to_string_lossy();
            for (attribute, value) in [
                ("dsAttrTypeStandard:UniqueID", user.uid.to_string()),
                ("dsAttrTypeStandard:PrimaryGroupID", user.gid.to_string()),
                ("dsAttrTypeStandard:NFSHomeDirectory", home.into_owned()),
                ("dsAttrTypeStandard:UserShell", SHELL.to_owned()),
                ("dsAttrTypeStandard:RealName", user.name.clone()),
                // Kept off the login window's list of users.
                ("dsAttrTypeNative:IsHidden", "1".to_owned()),
            ] {
                set(&record, attribute, &value)?;
            }
            // A random password nobody is told: the account cannot be logged into with
            // a password, and macOS gives it no secure token.
            let mut bytes = [0u8; 32];
            SecRandom::default().copy_bytes(&mut bytes)?;
            let password = NSString::from_str(&hex::encode(bytes));
            let mut error = None;
            // SAFETY: as above; no old password, as root on the local node.
            if !unsafe {
                record.changePassword_toPassword_error(None, Some(&password), Some(&mut error))
            } {
                return Err(od_error("setting the password", error));
            }
            if user.admin {
                let admin = group(&node, ADMIN_GROUP)?;
                let mut error = None;
                // SAFETY: both are records of the local node.
                if !unsafe { admin.addMemberRecord_error(Some(&record), Some(&mut error)) } {
                    return Err(od_error("adding to the admin group", error));
                }
            }
            Ok(())
        })
    }

    fn delete_user(&self, name: &str) -> io::Result<bool> {
        autoreleasepool(|_| {
            let node = local_node()?;
            let mut error = None;
            // SAFETY: the record type and name are NSStrings; no attributes needed.
            let record = unsafe {
                node.recordWithRecordType_name_attributes_error(
                    Some(&NSString::from_str(USERS)),
                    Some(&NSString::from_str(name)),
                    None,
                    Some(&mut error),
                )
            };
            let Some(record) = record else {
                return match error {
                    None => Ok(false),
                    error => Err(od_error("looking the user up", error)),
                };
            };
            // A stale membership would make the next holder of the name an
            // administrator; names are never reused, but the group is cleaned anyway.
            let admin = group(&node, ADMIN_GROUP)?;
            let mut ignored = None;
            // SAFETY: both are records of the local node. Not a member is an error
            // that changes nothing.
            unsafe { admin.removeMemberRecord_error(Some(&record), Some(&mut ignored)) };
            let mut error = None;
            // SAFETY: a record of the local node.
            if !unsafe { record.deleteRecordAndReturnError(Some(&mut error)) } {
                return Err(od_error("deleting the record", error));
            }
            Ok(true)
        })
    }

    fn bootout(&self, domain: &str) -> io::Result<()> {
        let output = Command::new("/bin/launchctl")
            .args(["bootout", domain])
            .env_clear()
            .output()?;
        if output.status.success() {
            return Ok(());
        }
        Err(io::Error::other(format!(
            "{}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )))
    }

    fn kill_all(&self, uid: u32) -> io::Result<()> {
        // SAFETY: the child calls only async-signal-safe functions (setuid, kill,
        // _exit) before it exits, as a fork of a threaded process must.
        let pid = unsafe { libc::fork() };
        if pid < 0 {
            return Err(io::Error::last_os_error());
        }
        if pid == 0 {
            // As the uid, kill(-1) signals every process the uid may signal (those
            // whose real or saved uid it is), in one call the uid's processes cannot
            // outrun by forking. On macOS the sender is one of them, so this child
            // usually ends here by SIGKILL; elsewhere the loop ends with ESRCH.
            // SAFETY: async-signal-safe calls only; `_exit` never returns.
            unsafe {
                if libc::setuid(uid) != 0 {
                    libc::_exit(1);
                }
                while libc::kill(-1, libc::SIGKILL) == 0 {}
                libc::_exit(0);
            }
        }
        let mut status = 0;
        // SAFETY: `pid` is this process's child; `status` is writable.
        if unsafe { libc::waitpid(pid, &raw mut status, 0) } < 0 {
            return Err(io::Error::last_os_error());
        }
        // macOS's kill(-1) signals the sender too (measured on the CI runner), so the
        // killer ending by SIGKILL is success: the signals to every other process of
        // the uid were posted in the same call. An exit of 1 is a failed setuid.
        let killed_itself = libc::WIFSIGNALED(status) && libc::WTERMSIG(status) == libc::SIGKILL;
        let exited_clean = libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0;
        if !killed_itself && !exited_clean {
            return Err(io::Error::other(format!(
                "the killer for uid {uid} failed to drop to it ({status:#x})"
            )));
        }
        Ok(())
    }

    fn live_processes(&self, uid: u32) -> io::Result<usize> {
        let mut pids = BTreeSet::new();
        for filter in [PROC_UID_ONLY, PROC_RUID_ONLY] {
            pids.extend(list_pids(filter, uid)?);
        }
        Ok(pids.into_iter().filter(|&pid| !zombie(pid)).count())
    }

    fn pause(&self) {
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// The local directory node (`/Local/Default`).
fn local_node() -> io::Result<Retained<ODNode>> {
    // SAFETY: plain constructors; the session may be the default (nil).
    let session = unsafe { ODSession::defaultSession() };
    let mut error = None;
    // SAFETY: as above.
    unsafe {
        ODNode::nodeWithSession_type_error(
            session.as_deref(),
            kODNodeTypeLocalNodes,
            Some(&mut error),
        )
    }
    .ok_or_else(|| od_error("opening the local node", error))
}

/// The group record `name` of `node`.
fn group(node: &ODNode, name: &str) -> io::Result<Retained<ODRecord>> {
    let mut error = None;
    // SAFETY: the record type and name are NSStrings; no attributes needed.
    unsafe {
        node.recordWithRecordType_name_attributes_error(
            Some(&NSString::from_str(GROUPS)),
            Some(&NSString::from_str(name)),
            None,
            Some(&mut error),
        )
    }
    .ok_or_else(|| od_error(&format!("looking up group {name}"), error))
}

/// Sets the single value of `attribute` on `record`.
fn set(record: &ODRecord, attribute: &str, value: &str) -> io::Result<()> {
    let value = NSString::from_str(value);
    let value: &AnyObject = &value;
    let mut error = None;
    // SAFETY: the value is an NSString and the attribute name an NSString.
    if unsafe {
        record.setValue_forAttribute_error(
            Some(value),
            Some(&NSString::from_str(attribute)),
            Some(&mut error),
        )
    } {
        return Ok(());
    }
    Err(od_error(&format!("setting {attribute}"), error))
}

fn od_error(what: &str, error: Option<Retained<NSError>>) -> io::Error {
    let why = error.map_or_else(
        || "no error given".to_owned(),
        |error| error.localizedDescription().to_string(),
    );
    io::Error::other(format!("OpenDirectory, {what}: {why}"))
}

/// The pids `libproc` lists for `filter` (by effective or real uid) and `uid`.
fn list_pids(filter: u32, uid: u32) -> io::Result<Vec<libc::pid_t>> {
    // SAFETY: a null buffer asks only for the size needed.
    let bytes = unsafe { libc::proc_listpids(filter, uid, std::ptr::null_mut(), 0) };
    if bytes < 0 {
        return Err(io::Error::last_os_error());
    }
    // Room for some more pids than the kernel reported, should some start meanwhile.
    let size = std::mem::size_of::<libc::pid_t>();
    let mut pids = vec![0 as libc::pid_t; bytes.unsigned_abs() as usize / size + 64];
    let room = i32::try_from(pids.len() * size).map_err(io::Error::other)?;
    // SAFETY: the buffer holds `room` bytes of pids and outlives the call.
    let wrote = unsafe { libc::proc_listpids(filter, uid, pids.as_mut_ptr().cast(), room) };
    if wrote < 0 {
        return Err(io::Error::last_os_error());
    }
    pids.truncate(wrote.unsigned_abs() as usize / size);
    pids.retain(|&pid| pid > 0);
    Ok(pids)
}

/// Whether `pid` has exited and waits to be reaped (or is gone already).
fn zombie(pid: libc::pid_t) -> bool {
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
    let Ok(size) = i32::try_from(std::mem::size_of::<libc::proc_bsdinfo>()) else {
        return false;
    };
    // SAFETY: `info` is a writable proc_bsdinfo of `size` bytes.
    let wrote = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    if wrote != size {
        // Gone since it was listed: not a live process.
        return true;
    }
    // SAFETY: the call filled the whole struct (checked above).
    unsafe { info.assume_init() }.pbi_status == SZOMB
}
