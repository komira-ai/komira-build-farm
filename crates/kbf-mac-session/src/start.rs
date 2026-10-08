//! Flags and start-up: everything between `main` and serving that does not need macOS.

use std::ffi::CString;
use std::io;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use rustix::fs::{CWD, Mode, OFlags};

use crate::grant::GrantKeys;
use crate::helper::{Helper, Host, Settings};
use crate::lease::UidRange;
use crate::ledger::Ledger;
use crate::server;
use crate::sweep::SweepPlan;

/// macOS's `staff` group, which every macOS user is in: never the socket's group.
pub const STAFF_GID: u32 = 20;

/// The helper's flags. Every value is a flag; nothing is read from the environment.
#[derive(Clone, Debug, clap::Parser)]
#[command(
    name = "kbf-mac-session",
    version,
    about = "The root helper that gives each Mac lease its own throwaway user"
)]
pub struct Args {
    /// The Unix socket the daemon connects to. Its directory is made mode 0750.
    #[arg(long, default_value = "/var/run/kbf-mac-session/socket")]
    pub socket: PathBuf,
    /// The group that may reach the socket: the daemon's dedicated group, never
    /// `staff` or the lease users' group.
    #[arg(long, default_value = "_kbf")]
    pub socket_group: String,
    /// Where the ledger of used lease ids is kept (made mode 0700).
    #[arg(long, default_value = "/var/db/kbf-mac-session")]
    pub state_dir: PathBuf,
    /// The uids lease users get, `<first>-<last>`.
    #[arg(long, default_value = "600-699")]
    pub uid_range: UidRange,
    /// Every lease user's primary group id (`staff` by default).
    #[arg(long, default_value_t = STAFF_GID)]
    pub lease_gid: u32,
    /// Where lease users' home folders are made.
    #[arg(long, default_value = "/Users")]
    pub homes: PathBuf,
    /// The code requirement a caller must satisfy: the installed `kbf-daemon`'s, as
    /// the installed software set pins it (for example `cdhash H"..."`).
    #[arg(long)]
    pub daemon_requirement: String,
    /// The MDM gate's public grant keys (one hex Ed25519 key per line). Without this
    /// file no lease user is ever an administrator.
    #[arg(long)]
    pub grant_keys: Option<PathBuf>,
}

/// A bound socket and the helper behind it, ready to serve.
pub struct Ready {
    pub listener: UnixListener,
    pub helper: Arc<Helper>,
}

/// Checks the flags, opens the ledger and binds the socket.
///
/// # Errors
/// What stops the helper from starting.
pub fn prepare(
    args: &Args,
    host: Box<dyn Host>,
    serial: String,
    sweep: SweepPlan,
) -> Result<Ready, String> {
    let gid = group_gid(&args.socket_group).map_err(|why| why.to_string())?;
    if gid == STAFF_GID || gid == args.lease_gid {
        return Err(format!(
            "the socket's group {:?} must be the daemon's own: not staff, not the lease users' group",
            args.socket_group
        ));
    }
    let grant_keys = match &args.grant_keys {
        None => None,
        Some(path) => {
            let text = std::fs::read_to_string(path)
                .map_err(|why| format!("{}: {why}", path.display()))?;
            Some(GrantKeys::parse(&text).map_err(|why| format!("{}: {why}", path.display()))?)
        }
    };
    private_dir(&args.state_dir).map_err(|why| format!("{}: {why}", args.state_dir.display()))?;
    let ledger_path = args.state_dir.join("ledger");
    let ledger = Ledger::open(&ledger_path).map_err(|why| why.to_string())?;
    let listener = server::bind(&args.socket, gid)
        .map_err(|why| format!("{}: {why}", args.socket.display()))?;
    let settings = Settings {
        range: args.uid_range,
        gid: args.lease_gid,
        homes: args.homes.clone(),
        sweep,
        grant_keys,
        serial,
    };
    tracing::info!(
        socket = %args.socket.display(),
        range = %args.uid_range,
        admin_grants = settings.grant_keys.is_some(),
        "kbf-mac-session ready"
    );
    Ok(Ready {
        listener,
        helper: Arc::new(Helper::new(host, settings, ledger)),
    })
}

/// Makes `dir` (mode 0700) unless it exists, and makes sure it is a directory, not a
/// link, and private.
fn private_dir(dir: &Path) -> io::Result<()> {
    match rustix::fs::mkdir(dir, Mode::from_raw_mode(0o700)) {
        Ok(()) | Err(rustix::io::Errno::EXIST) => {}
        Err(why) => return Err(why.into()),
    }
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let opened = rustix::fs::openat(CWD, dir, flags, Mode::empty())?;
    Ok(rustix::fs::fchmod(&opened, Mode::from_raw_mode(0o700))?)
}

/// The id of the group named `name`.
///
/// # Errors
/// There is no such group, or the name holds NUL.
pub fn group_gid(name: &str) -> io::Result<u32> {
    let c_name = CString::new(name)?;
    // SAFETY: a zeroed `group` is a valid value for getgrnam_r to overwrite.
    let mut group: libc::group = unsafe { std::mem::zeroed() };
    let mut buffer = vec![0 as libc::c_char; 1 << 16];
    let mut found: *mut libc::group = std::ptr::null_mut();
    // SAFETY: every pointer is to a live, writable value of the right type, and
    // `buffer.len()` is the buffer's size; the strings the call stores in `group`
    // point into `buffer`, which is not read.
    let code = unsafe {
        libc::getgrnam_r(
            c_name.as_ptr(),
            &raw mut group,
            buffer.as_mut_ptr(),
            buffer.len(),
            &raw mut found,
        )
    };
    // On an error the call leaves `found` null too, so `found` alone decides.
    let gid = (!found.is_null()).then_some(group.gr_gid);
    gid.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("no group named {name:?} (getgrnam_r: {code})"),
        )
    })
}

/// Reads this Mac's serial number from `ioreg -rd1 -c IOPlatformExpertDevice`'s
/// output: the `"IOPlatformSerialNumber" = "..."` line.
#[must_use]
pub fn parse_serial(ioreg: &str) -> Option<String> {
    ioreg.lines().find_map(|line| {
        let value = line
            .trim()
            .strip_prefix("\"IOPlatformSerialNumber\" = \"")?
            .strip_suffix('"')?;
        (!value.is_empty() && value.bytes().all(|b| b.is_ascii_alphanumeric()))
            .then(|| value.to_owned())
    })
}

/// Runs the helper with `args`. Only macOS serves; elsewhere it exits at once.
#[must_use]
pub fn main(args: &Args) -> ExitCode {
    #[cfg(target_os = "macos")]
    {
        crate::macos::main(args)
    }
    #[cfg(not(target_os = "macos"))]
    {
        eprintln!(
            "kbf-mac-session: serves only on macOS (socket {} not bound)",
            args.socket.display()
        );
        ExitCode::from(2)
    }
}

#[cfg(test)]
mod tests;
