//! macOS: the host the verbs run on, the caller check by code signature, and `main`.
//!
//! Nothing here is reached by the Linux test jobs; the `native-macos` CI job runs the
//! helper for real as root (`tools/ci/mac-session-tests.sh`).
//!
//! # Signing
//!
//! The helper, like `kbf-daemon`, must be signed with the hardened runtime
//! (`codesign --options runtime`) and without the `get-task-allow` or
//! `disable-library-validation` entitlements: the hardened runtime turns on library
//! validation, so `DYLD_INSERT_LIBRARIES` and a debugger cannot reach a process that
//! runs as root (S4.3). An ad-hoc signature (`codesign -s -`) carries no team, so
//! library validation then admits only the system's own libraries, which is all the
//! helper links. The caller check relies on the same for `kbf-daemon`: a requirement
//! pinned by cdhash names exactly the installed daemon binary.

mod caller;
mod host;

use std::io;
use std::os::fd::{AsRawFd as _, OwnedFd};
use std::os::unix::ffi::OsStrExt as _;
use std::process::ExitCode;
use std::sync::Arc;

pub use caller::CodeSignatureCheck;
pub use host::MacHost;

use crate::server::{self, CallerCheck};
use crate::start::{self, Args};
use crate::sweep::SweepPlan;

/// Runs the helper: root only, then the caller check, then serving.
#[must_use]
pub fn main(args: &Args) -> ExitCode {
    tracing_subscriber::fmt().with_writer(io::stderr).init();
    if !rustix::process::geteuid().is_root() {
        tracing::error!("kbf-mac-session must run as root");
        return ExitCode::from(2);
    }
    let started = read_serial().and_then(|serial| {
        let check: Arc<dyn CallerCheck> =
            Arc::new(CodeSignatureCheck::new(&args.daemon_requirement)?);
        let ready = start::prepare(
            args,
            Box::new(MacHost),
            serial,
            SweepPlan::macos_schedules(),
            SweepPlan::macos(),
        )?;
        Ok((check, ready))
    });
    match started {
        Ok((check, ready)) => {
            let why = server::serve(&ready.listener, &ready.helper, &check);
            tracing::error!("accepting connections failed: {why}");
            ExitCode::FAILURE
        }
        Err(why) => {
            tracing::error!("kbf-mac-session did not start: {why}");
            ExitCode::from(2)
        }
    }
}

/// This Mac's serial number, from the I/O Registry.
fn read_serial() -> Result<String, String> {
    let output = std::process::Command::new("/usr/sbin/ioreg")
        .args(["-rd1", "-c", "IOPlatformExpertDevice"])
        .env_clear()
        .output()
        .map_err(|why| format!("ioreg: {why}"))?;
    start::parse_serial(&String::from_utf8_lossy(&output.stdout))
        .ok_or_else(|| "ioreg named no IOPlatformSerialNumber".to_owned())
}

/// The user flags that block removing an entry or a directory's entries.
const LOCKING_FLAGS: u32 = libc::UF_IMMUTABLE | libc::UF_APPEND;

/// Clears [`LOCKING_FLAGS`] of `name` in `dir`, whose flags are `flags`, without
/// following a link (`setattrlistat` with `FSOPT_NOFOLLOW`; macOS has no
/// `chflagsat`). The same call as `kbf-outputs`' tree removal makes.
pub(crate) fn clear_user_flags(
    dir: &OwnedFd,
    name: &std::ffi::OsStr,
    flags: u32,
) -> io::Result<()> {
    if flags & LOCKING_FLAGS == 0 {
        return Ok(());
    }
    let name = std::ffi::CString::new(name.as_bytes())?;
    let mut list = libc::attrlist {
        bitmapcount: libc::ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: libc::ATTR_CMN_FLAGS,
        volattr: 0,
        dirattr: 0,
        fileattr: 0,
        forkattr: 0,
    };
    let mut value: u32 = flags & !LOCKING_FLAGS;
    // SAFETY: `name` is NUL-terminated, `list` an attrlist that asks for the common
    // flags only, and `value` the u32 that attribute is; all three outlive the call.
    let set = unsafe {
        libc::setattrlistat(
            dir.as_raw_fd(),
            name.as_ptr(),
            (&raw mut list).cast(),
            (&raw mut value).cast(),
            std::mem::size_of::<u32>(),
            libc::FSOPT_NOFOLLOW,
        )
    };
    if set != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
