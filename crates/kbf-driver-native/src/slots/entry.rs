//! A pool user's `/var/folders` entry, cleaned **as the pool user**.
//!
//! The daemon is root, but a launch daemon has no Full Disk Access, and without it root
//! cannot clear the flags or the ACLs of the entry's `0`, `C`, `T` and `X`, nor list
//! them: the system protects a user's temporary and cache folders from a process that
//! is not the user's own. The user itself can do all of it to the files it owns. So the
//! entry is cleaned by a child that drops to the slot's uid and gid (`setgroups`,
//! `setgid`, `setuid`, in that order) and then runs one of the modes below, or `find`
//! for the ACLs. The modes drop **in the process that is already running**, so the
//! pool user never has to be able to execute the daemon's binary; `find` goes through
//! `__as-user` ([`crate::slots::AS_USER_ARG`]) and a system tool. Nothing here runs a
//! shell, and root applies no flag or ACL change to the entry.
//!
//! - [`clear_flags`] (C6): the ACLs of the uid's entries below the four folders (`find
//!   -x ... -user <uid> -exec chmod -N`), the user flags and the owner's mode of everything
//!   the uid owns inside them ([`CLEAR`]), then the ACLs again. The entry and its four
//!   folders keep the flags and ACLs the system set (`sunlnk` is root's to clear, and
//!   root without Full Disk Access cannot); the check accepts them.
//! - [`restore`] (C10): the four folders narrowed to the system's modes, as the uid ([`RESTORE`]); their flags and
//!   ACLs stay as the system has them.
//! - [`empty`] (C7): the entry's `0`, `C`, `T` and `X` emptied and kept, anything else in
//!   the entry removed ([`EMPTY`]).
//! - [`left`] (the check): what an uid-side look finds in the entry afterwards
//!   ([`LOOK`]); root cannot look inside the folders, so it asks the uid.
//!
//! The decisions are plain functions over a [`Run`], so the order of the calls and who
//! each runs as are tested on every platform with a fake; the real [`Run`] is the
//! Mac host's.

use std::ffi::OsString;
use std::io::Write;
use std::path::Path;

use kbf_mac_session::sweep;

use super::keychain;
use super::probe::{self, SKELETON};

/// The argument after argv0 that selects this binary's entry mode:
/// `__entry <mode> <uid> <gid> <dir>`.
pub const ARG: &str = "__entry";
/// The mode that clears the flags and modes below the entry: `__entry clear <uid> <gid> <dir>`.
pub const CLEAR: &str = "clear";
/// The mode that empties the entry: `__entry empty <uid> <gid> <dir>`.
pub const EMPTY: &str = "empty";
/// The mode that gives the four folders their mode back: `__entry restore <uid> <gid> <dir>`.
pub const RESTORE: &str = "restore";
/// The mode that removes the ACLs of the uid's entries below the four folders:
/// `__entry acl <uid> <gid> <dir>`; the system's refusals of the entries the pool user
/// cannot open are not failures.
pub const ACL: &str = "acl";
/// The mode that empties the home: `__entry empty-home <uid> <gid> <dir>`.
pub const EMPTY_HOME: &str = "empty-home";
/// The mode that deletes the uid's keychain items: `__entry keychain <uid> <gid> <dir>`
/// (`dir` is the home; it is not read).
pub const KEYCHAIN: &str = "keychain";
/// The mode that prints what is left in the home: `__entry look-home <uid> <gid> <dir>`.
pub const LOOK_HOME: &str = "look-home";
/// The mode that prints what is left in the entry: `__entry look <uid> <gid> <dir>`.
pub const LOOK: &str = "look";

const USAGE: &str =
    "usage: __entry clear|empty|look|restore|acl|empty-home|look-home|keychain <uid> <gid> <dir>";

const FIND: &str = "/usr/bin/find";
const CHMOD: &str = "/bin/chmod";

/// The slot user a tool runs as, and the daemon binary that drops to it.
pub(crate) struct AsUid<'a> {
    /// The daemon binary: it answers [`crate::slots::AS_USER_ARG`] and [`ARG`].
    pub program: &'a Path,
    pub uid: u32,
    pub gid: u32,
}

/// Runs `program` with `args` and says what it printed, or why it failed. The real one
/// is the Mac host's (a plain child, a plain environment); a test's records the calls.
pub(crate) type Run<'a> = dyn FnMut(&Path, &[OsString]) -> Result<String, String> + 'a;

impl AsUid<'_> {
    /// This binary's entry `mode` on `dir`, which drops to the uid itself.
    fn mode(&self, mode: &str, dir: &Path, run: &mut Run<'_>) -> Result<String, String> {
        let args = [
            ARG.into(),
            mode.into(),
            self.uid.to_string().into(),
            self.gid.to_string().into(),
            dir.as_os_str().to_owned(),
        ];
        run(self.program, &args)
    }
}

/// C6: the home's flags, ACLs and modes by `as_root` (it gets the home only, and says the
/// errors it had), the entry's as the uid. The entry is never given to `as_root`: root
/// cannot do it there, and the uid can.
///
/// # Errors
/// A part failed; every part is tried.
pub(crate) fn flags_step(
    me: &AsUid<'_>,
    (home, folders): (&Path, &Path),
    as_root: &mut dyn FnMut(&Path) -> Vec<String>,
    run: &mut Run<'_>,
) -> Result<(), String> {
    let mut failed = as_root(home);
    failed.extend(clear_flags(me, folders, run).err());
    join(failed)
}

/// C7: the home emptied as the uid (root without Full Disk Access cannot empty its
/// `Library`), then the entry emptied as the uid.
///
/// # Errors
/// A part failed; both are tried.
pub(crate) fn contents_step(
    me: &AsUid<'_>,
    dirs: (&Path, &Path),
    run: &mut Run<'_>,
) -> Result<(), String> {
    contents_step_waiting(me, dirs, run, &mut std::thread::sleep)
}

/// How often the home's empty is tried again while only the keychain's files are left,
/// and for how long in all: `secd` removes the per-user keychain it made for the uid a
/// while after the lease (about a minute was seen), so the clean waits for it.
pub(crate) const KEYCHAIN_POLL: std::time::Duration = std::time::Duration::from_secs(2);
pub(crate) const KEYCHAIN_TOTAL: std::time::Duration = std::time::Duration::from_secs(60);

/// Whether `lines`, what the uid finds left in `home`, are all in its `Library/Keychains`
/// and there is at least one: the system holds them for a while, and the empty is tried
/// again every [`KEYCHAIN_POLL`] for [`KEYCHAIN_TOTAL`]. Never skipped: a keychain is
/// readable by the next lease that runs as the same user.
pub(crate) fn keychain_only(lines: &[String], home: &Path) -> bool {
    let keychains = home.join("Library/Keychains");
    !lines.is_empty() & lines.iter().all(|l| Path::new(l).starts_with(&keychains))
}

/// [`contents_step`], waiting with `sleep`: the uid's keychain items deleted, the home
/// emptied (again, polling, while only keychain files are left), the entry emptied.
///
/// # Errors
/// A part failed; all are tried.
pub(crate) fn contents_step_waiting(
    me: &AsUid<'_>,
    (home, folders): (&Path, &Path),
    run: &mut Run<'_>,
    sleep: &mut dyn FnMut(std::time::Duration),
) -> Result<(), String> {
    let mut failed = Vec::new();
    failed.extend(wipe_keychains(me, home, run).err());
    failed.extend(empty_home_waiting(me, home, run, sleep).err());
    failed.extend(empty(me, folders, run).err());
    join(failed)
}

/// The home emptied; while only files of `Library/Keychains` are left, again every
/// [`KEYCHAIN_POLL`] up to [`KEYCHAIN_TOTAL`], then the failure with what is left.
fn empty_home_waiting(
    me: &AsUid<'_>,
    home: &Path,
    run: &mut Run<'_>,
    sleep: &mut dyn FnMut(std::time::Duration),
) -> Result<(), String> {
    let mut waited = std::time::Duration::ZERO;
    loop {
        let Err(why) = empty_home(me, home, run) else {
            return Ok(());
        };
        let left = left_home(me, home, run).ok();
        if !left.as_deref().is_some_and(|l| keychain_only(l, home)) {
            return Err(why);
        }
        if waited >= KEYCHAIN_TOTAL {
            return Err(format!(
                "{why}; left after waiting {KEYCHAIN_TOTAL:?}: {:?}",
                left.unwrap_or_default()
            ));
        }
        sleep(KEYCHAIN_POLL);
        waited += KEYCHAIN_POLL;
    }
}

/// The uid's keychain items deleted, as the uid.
///
/// # Errors
/// The call failed (an item could not be deleted, or one is left).
pub(crate) fn wipe_keychains(me: &AsUid<'_>, home: &Path, run: &mut Run<'_>) -> Result<(), String> {
    me.mode(KEYCHAIN, home, run)
        .map(drop)
        .map_err(|why| format!("keychain items of {}: {why}", home.display()))
}

/// [`KEYCHAIN`]: `wipe` deletes the items and says what failed or is left; exit 1 then.
fn keychain_mode(wipe: &mut dyn FnMut() -> Vec<String>, err: &mut dyn Write) -> i32 {
    let failed = wipe();
    for line in &failed {
        let _ = writeln!(err, "{line}");
    }
    i32::from(!failed.is_empty())
}

fn join(failed: Vec<String>) -> Result<(), String> {
    if failed.is_empty() {
        Ok(())
    } else {
        Err(failed.join("; "))
    }
}

/// `find`'s arguments that remove the ACL of every entry of the uid below
/// `entry`, not the entry itself or its four folders (see below) (never through a link, on one filesystem). A directory is done where `find`
/// meets it, before `find` reads it, so an ACL that denies the listing does not stop the
/// walk; the other entries are done in batches.
pub(crate) fn acl_args(entry: &Path, uid: u32) -> Vec<OsString> {
    let words = |list: &[&str]| list.iter().map(OsString::from).collect::<Vec<_>>();
    let mut args = words(&["-x"]);
    args.push(entry.as_os_str().to_owned());
    // The entry and its four folders keep the ACLs the system gave them; what is in
    // the folders is cleared.
    args.extend(words(&["!", "(", "-path"]));
    args.push(entry.as_os_str().to_owned());
    for folder in SKELETON {
        args.extend(words(&["-o", "-path"]));
        args.push(entry.join(folder).into_os_string());
    }
    args.extend(words(&[")"]));
    // One group holds both branches, so that `-user` applies to each.
    args.extend(words(&["-user", &uid.to_string(), "(", "("]));
    args.extend(words(&["-type", "d", "-exec", CHMOD, "-N", "{}", ";", ")"]));
    args.extend(words(&["-o", "(", "!", "-type", "l", "-exec", CHMOD, "-N"]));
    args.extend(words(&["{}", "+", ")", ")"]));
    args
}

/// C6 for the entry `folders`, all as the uid: the ACLs of the uid's entries removed, the
/// flags and modes below it cleared, and the ACLs removed again. Neither of the two can
/// do what the other lets through: the uid obeys an ACL that denies the listing of a
/// directory (root did not), so the flags below it are out of reach of the sweep until
/// the first removal, and a locked file cannot have its ACL changed until the sweep
/// unlocks it. The first removal is therefore allowed to fail; the sweep and the last
/// removal are not, and every failure of those is named with the entry.
///
/// # Errors
/// The sweep or the last removal failed.
pub(crate) fn clear_flags(me: &AsUid<'_>, folders: &Path, run: &mut Run<'_>) -> Result<(), String> {
    // Whatever this cannot reach is reached after the sweep, and judged then.
    let _ = me.mode(ACL, folders, run);
    let mut failed = Vec::new();
    if let Err(why) = me.mode(CLEAR, folders, run) {
        failed.push(why);
    }
    if let Err(why) = me.mode(ACL, folders, run) {
        failed.push(why);
    }
    named(folders, failed)
}

/// C10 for the entry `folders`: the four folders given back their mode as the uid, which
/// owns them. Nothing else is touched: not the entry, not the flags or ACLs the system
/// set.
///
/// # Errors
/// The call failed (a folder is not the uid's, or its mode could not be set).
pub(crate) fn restore(me: &AsUid<'_>, folders: &Path, run: &mut Run<'_>) -> Result<(), String> {
    named(
        folders,
        me.mode(RESTORE, folders, run).err().into_iter().collect(),
    )
}

/// [`RESTORE`]: each of [`SKELETON`]'s folders in `dir` that exists has its extended
/// attributes taken off (the protected ones stay) and is narrowed to the system's layout
/// ([`probe::folder_ceiling`]) only when its mode is wider, and nothing else; one the uid
/// does not own is an error that names it. Run as the uid. Exit 1 when a folder could not be put right.
fn restore_folders(
    dir: &Path,
    uid: u32,
    (chmod, strip): (&mut ChmodCall<'_>, &mut PathCall<'_>),
) -> sweep::Swept {
    use std::os::unix::fs::MetadataExt as _;
    let mut swept = sweep::Swept::default();
    for name in SKELETON {
        let path = dir.join(name);
        let meta = match std::fs::symlink_metadata(&path) {
            Ok(meta) if meta.is_dir() => meta,
            Ok(_) => {
                swept
                    .errors
                    .push(format!("{}: not a directory", path.display()));
                continue;
            }
            // The system makes it again, as the user, when it is next asked for.
            Err(why) if why.kind() == std::io::ErrorKind::NotFound => continue,
            Err(why) => {
                swept.errors.push(format!("{}: {why}", path.display()));
                continue;
            }
        };
        // What the lease put on the folder; what the system protects stays (it is
        // recorded as the folder's baseline, and the check finds any other that stays).
        if let Err(why) = strip(&path) {
            swept.errors.push(format!("{}: {why}", path.display()));
        }
        let (wrong_owner, narrow_to) =
            probe::folder_fix((meta.uid(), meta.mode()), uid, probe::folder_ceiling(name));
        if wrong_owner {
            swept.errors.push(format!(
                "{}: owned by {}, not the pool user",
                path.display(),
                meta.uid()
            ));
        } else if let Some(mode) = narrow_to
            && let Err(why) = chmod(&path, mode)
        {
            swept.errors.push(format!("{}: {why}", path.display()));
        }
    }
    swept
}

/// `find`, run with these arguments: whether it succeeded, and what it said on its error
/// output.
type Find<'a> = dyn FnMut(&[OsString]) -> std::io::Result<(bool, String)> + 'a;

/// A change of a path's mode that can fail.
type ChmodCall<'a> = dyn FnMut(&Path, u32) -> std::io::Result<()> + 'a;

/// A call on a path that can fail.
type PathCall<'a> = dyn FnMut(&Path) -> std::io::Result<()> + 'a;

/// The listing of extended attributes: a size without a buffer, else the bytes put in it.
type List<'a> = dyn FnMut(Option<&mut [u8]>) -> Result<usize, i32> + 'a;

/// Whether the error `errno` of taking an extended attribute off a folder or listing them
/// is one to leave: the attribute went since the listing, or the system protects it from
/// its owner (the entry's folders carry some).
fn xattr_left(errno: i32) -> bool {
    errno == libc::EPERM || errno == NO_ATTR
}

#[cfg(target_os = "macos")]
const NO_ATTR: i32 = libc::ENOATTR;
#[cfg(not(target_os = "macos"))]
const NO_ATTR: i32 = libc::ENODATA;

/// The extended attributes of the directory `path` (not through a link) taken off, the
/// ones the system protects excepted ([`xattr_left`]).
fn strip_xattrs(path: &Path) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt as _;
    let c_path =
        std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(std::io::Error::other)?;
    let errno = || {
        std::io::Error::last_os_error()
            .raw_os_error()
            .unwrap_or(libc::EIO)
    };
    let mut list = |buf: Option<&mut [u8]>| {
        let (ptr, size) = buf.map_or((std::ptr::null_mut(), 0), |b| (b.as_mut_ptr(), b.len()));
        // SAFETY: `c_path` is NUL-terminated; `ptr` is null with size 0, or writable for
        // `size` bytes.
        #[cfg(target_os = "macos")]
        let got =
            unsafe { libc::listxattr(c_path.as_ptr(), ptr.cast(), size, libc::XATTR_NOFOLLOW) };
        // SAFETY: as above.
        #[cfg(not(target_os = "macos"))]
        let got = unsafe { libc::llistxattr(c_path.as_ptr(), ptr.cast(), size) };
        usize::try_from(got).map_err(|_| errno())
    };
    let mut remove = |name: &std::ffi::CStr| {
        // SAFETY: both strings are NUL-terminated.
        #[cfg(target_os = "macos")]
        let done =
            unsafe { libc::removexattr(c_path.as_ptr(), name.as_ptr(), libc::XATTR_NOFOLLOW) };
        // SAFETY: as above.
        #[cfg(not(target_os = "macos"))]
        let done = unsafe { libc::lremovexattr(c_path.as_ptr(), name.as_ptr()) };
        usize::try_from(done).map(drop).map_err(|_| errno())
    };
    strip_with(&mut list, &mut remove)
}

/// What [`strip_xattrs`] does over the two calls: `list` (a size when given no buffer,
/// else the bytes of the NUL-separated names put in it) and `remove` of one name; each
/// fails with an errno.
fn strip_with(
    list: &mut List<'_>,
    remove: &mut dyn FnMut(&std::ffi::CStr) -> Result<(), i32>,
) -> std::io::Result<()> {
    let leave = |errno: i32| {
        if xattr_left(errno) {
            Ok(())
        } else {
            Err(std::io::Error::from_raw_os_error(errno))
        }
    };
    let size = match list(None) {
        Ok(size) => size,
        Err(errno) => return leave(errno),
    };
    let mut names = vec![0_u8; size];
    match list(Some(&mut names)) {
        Ok(got) => names.truncate(got),
        Err(errno) => return leave(errno),
    }
    for name in names.split(|b| *b == 0).filter(|n| !n.is_empty()) {
        let name = std::ffi::CString::new(name).map_err(std::io::Error::other)?;
        if let Err(errno) = remove(&name) {
            leave(errno)?;
        }
    }
    Ok(())
}

/// C7 for the entry `folders`: emptied as the uid.
///
/// # Errors
/// The call failed.
pub(crate) fn empty(me: &AsUid<'_>, folders: &Path, run: &mut Run<'_>) -> Result<(), String> {
    named(
        folders,
        me.mode(EMPTY, folders, run).err().into_iter().collect(),
    )
}

/// C7 for the home `home`: emptied as the uid.
///
/// # Errors
/// The call failed.
pub(crate) fn empty_home(me: &AsUid<'_>, home: &Path, run: &mut Run<'_>) -> Result<(), String> {
    me.mode(EMPTY_HOME, home, run)
        .map(drop)
        .map_err(|why| format!("{}: {why}", home.display()))
}

/// What the uid finds left in the home `home`, one line each; what it cannot open is not
/// left.
///
/// # Errors
/// The uid could not look.
pub(crate) fn left_home(
    me: &AsUid<'_>,
    home: &Path,
    run: &mut Run<'_>,
) -> Result<Vec<String>, String> {
    let said = me
        .mode(LOOK_HOME, home, run)
        .map_err(|why| format!("{}: {why}", home.display()))?;
    Ok(said.lines().map(str::to_owned).collect())
}

/// What the uid finds left in the entry `folders`, one line each.
///
/// # Errors
/// The uid could not look.
pub(crate) fn left(
    me: &AsUid<'_>,
    folders: &Path,
    run: &mut Run<'_>,
) -> Result<Vec<String>, String> {
    let said = me
        .mode(LOOK, folders, run)
        .map_err(|why| format!("{}: {why}", folders.display()))?;
    Ok(said.lines().map(str::to_owned).collect())
}

fn named(folders: &Path, failed: Vec<String>) -> Result<(), String> {
    if failed.is_empty() {
        return Ok(());
    }
    Err(format!("{}: {}", folders.display(), failed.join("; ")))
}

/// The entry mode for `args` (the program's own, argv0 first): its exit code when they
/// select it. It drops to the uid and gid it is given with `drop_to` (which checks the
/// ids it ends with), then runs the mode. Errors go to `err`, what [`LOOK`] finds to
/// `out`. Refuses root as the target: the mode exists to do what the pool user can.
/// Exits 2 for arguments it cannot read, 126 when the drop fails.
pub(crate) fn dispatch(
    args: &[OsString],
    drop_to: &mut dyn FnMut(u32, u32) -> Result<(), String>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Option<i32> {
    if args.get(1).is_none_or(|a| a != ARG) {
        return None;
    }
    let [_, _, mode, uid, gid, dir] = args else {
        let _ = writeln!(err, "{USAGE}");
        return Some(2);
    };
    let id = |text: &OsString| text.to_str().and_then(|t| t.parse::<u32>().ok());
    let (Some(uid), Some(gid)) = (id(uid).filter(|n| *n != 0), id(gid).filter(|n| *n != 0)) else {
        let _ = writeln!(err, "{USAGE}: the ids are not non-root ids");
        return Some(2);
    };
    let mode = match mode.to_str() {
        Some(mode @ (CLEAR | EMPTY | LOOK | RESTORE | ACL | EMPTY_HOME | LOOK_HOME | KEYCHAIN)) => {
            mode
        }
        _ => {
            let _ = writeln!(err, "{ARG}: no mode {mode:?}");
            return Some(2);
        }
    };
    if let Err(why) = drop_to(uid, gid) {
        let _ = writeln!(err, "{ARG}: {why}");
        return Some(126);
    }
    let dir = Path::new(dir);
    let swept = match mode {
        CLEAR => sweep::clear_flags_within(dir, uid, &SKELETON),
        RESTORE => restore_folders(
            dir,
            uid,
            (
                &mut |path, mode| {
                    use std::os::unix::fs::PermissionsExt as _;
                    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
                },
                &mut strip_xattrs,
            ),
        ),
        EMPTY => sweep::empty_keeping(dir, uid, &SKELETON),
        ACL => return Some(acl_mode(dir, uid, &mut run_find, err)),
        EMPTY_HOME => sweep::empty(dir, uid),
        KEYCHAIN => return Some(keychain_mode(&mut keychain::wipe_user_keychains, err)),
        LOOK_HOME => return Some(look_home(dir, out, err)),
        _ => return Some(look(dir, out, err)),
    };
    for error in &swept.errors {
        let _ = writeln!(err, "{error}");
    }
    Some(i32::from(!swept.errors.is_empty()))
}

/// [`LOOK`]: a line for each name in `dir` that is not one of [`SKELETON`], and for
/// each of those that is not empty, leaving out what the pool user cannot open (see
/// [`sweep::path_is_protected`]). Exit 1 when something could not be listed.
fn look(dir: &Path, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let names = |path: &Path| -> std::io::Result<Vec<String>> {
        // A link in place of a folder is not followed: it is reported, not listed.
        if !std::fs::symlink_metadata(path)?.is_dir() {
            return Err(std::io::Error::other("not a directory"));
        }
        let mut names = std::fs::read_dir(path)?
            .map(|e| e.map(|e| e.file_name().to_string_lossy().into_owned()))
            .collect::<Result<Vec<_>, _>>()?;
        names.sort();
        // What the pool user cannot open is a protected store of the system, not left.
        names.retain(|n| !sweep::path_is_protected(&path.join(n)));
        Ok(names)
    };
    let mut code = 0;
    let top = match names(dir) {
        Ok(top) => top,
        Err(why) => {
            let _ = writeln!(err, "{}: {why}", dir.display());
            return 1;
        }
    };
    for name in top {
        if !SKELETON.contains(&name.as_str()) {
            let _ = writeln!(out, "{}: {name}", dir.display());
            continue;
        }
        let folder = dir.join(&name);
        match names(&folder).map(|mut inside| {
            // A directory the system placed is kept; only what is in it is left.
            inside.retain(|n| {
                let path = folder.join(n);
                let placed = sweep::path_is_system_placed(&path);
                if placed {
                    left_below(&path, out, err, &mut code);
                }
                !placed
            });
            inside
        }) {
            Ok(inside) if inside.is_empty() => {}
            Ok(inside) => {
                let _ = writeln!(out, "{}: {inside:?}", folder.display());
            }
            Err(why) => {
                let _ = writeln!(err, "{}: {why}", folder.display());
                code = 1;
            }
        }
    }
    code
}

/// [`LOOK_HOME`]: a line for each file, link or empty directory left below `dir`, leaving
/// out what the pool user cannot open and a directory that holds only such things (it
/// stays because they do). Exit 1 when something could not be listed.
fn look_home(dir: &Path, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let mut code = 0;
    left_below(dir, out, err, &mut code);
    code
}

/// What [`left_below`] found in a directory: whether it printed a line, and whether it
/// skipped something the pool user cannot open.
#[derive(Default)]
struct Found {
    printed: bool,
    protected: bool,
    failed: bool,
}

impl Found {
    /// Whether nothing was printed, skipped or failed: the directory is empty.
    fn nothing(&self) -> bool {
        !(self.printed | self.protected | self.failed)
    }
}

fn left_below(dir: &Path, out: &mut dyn Write, err: &mut dyn Write, code: &mut i32) -> Found {
    let mut found = Found::default();
    let names = match std::fs::read_dir(dir).and_then(|names| {
        names
            .map(|e| e.map(|e| e.file_name()))
            .collect::<Result<Vec<_>, _>>()
    }) {
        Ok(mut names) => {
            names.sort();
            names
        }
        Err(why) => {
            let _ = writeln!(err, "{}: {why}", dir.display());
            *code = 1;
            found.failed = true;
            return found;
        }
    };
    for name in names {
        let path = dir.join(&name);
        if sweep::path_is_protected(&path) {
            found.protected = true;
            continue;
        }
        let is_dir = std::fs::symlink_metadata(&path).is_ok_and(|m| m.is_dir());
        if !is_dir {
            let _ = writeln!(out, "{}", path.display());
            found.printed = true;
            continue;
        }
        let placed = sweep::path_is_system_placed(&path);
        let inside = left_below(&path, out, err, code);
        found.printed |= inside.printed;
        found.protected |= inside.protected;
        found.failed |= inside.failed;
        if inside.nothing() & !placed {
            let _ = writeln!(out, "{}", path.display());
            found.printed = true;
        }
        // An empty directory the system placed stays, and so does the one that holds it.
        found.protected |= inside.nothing() & placed;
    }
    found
}

/// The error lines of `find` and `chmod` that name a path they were refused:
/// `find: <path>: Operation not permitted`, `chmod: Failed to clear ACL on file <path>:
/// Operation not permitted`; the path.
fn refused_path(line: &str) -> Option<&Path> {
    let line = [": Operation not permitted", ": Permission denied"]
        .iter()
        .find_map(|tail| line.strip_suffix(tail))?;
    let path = ["find: ", "chmod: Failed to clear ACL on file "]
        .iter()
        .find_map(|head| line.strip_prefix(head))?;
    Some(Path::new(path))
}

/// Runs `find` with `args`: whether it succeeded, and what it said on its error output.
fn run_find(args: &[OsString]) -> std::io::Result<(bool, String)> {
    let out = std::process::Command::new(FIND)
        .args(args)
        .env_clear()
        .stdin(std::process::Stdio::null())
        .output()?;
    Ok((
        out.status.success(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    ))
}

/// [`ACL`]: the ACLs removed below `dir` by `find`; its refusals of entries the pool user
/// cannot open do not fail it, every other error line does. Exit 1 then.
fn acl_mode(dir: &Path, uid: u32, find: &mut Find<'_>, err: &mut dyn Write) -> i32 {
    let (ok, said) = match find(&acl_args(dir, uid)) {
        Ok(done) => done,
        Err(why) => {
            let _ = writeln!(err, "{FIND}: {why}");
            return 1;
        }
    };
    if ok {
        return 0;
    }
    let others: Vec<&str> = said
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter(|l| {
            !refused_path(l)
                .is_some_and(|p| sweep::path_is_protected(p) || sweep::path_is_system_placed(p))
        })
        .collect();
    if said.trim().is_empty() {
        let _ = writeln!(err, "{FIND} failed");
        return 1;
    }
    for line in &others {
        let _ = writeln!(err, "{line}");
    }
    i32::from(!others.is_empty())
}

#[cfg(test)]
mod tests;
