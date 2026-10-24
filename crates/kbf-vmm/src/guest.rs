//! Running a guest that carries `kbf-guest`: the shared folder it finds, the files the
//! guest writes back, and the launch daemons that start the guest side offline.
//!
//! The base image has no user, no agent and no network. The only way into it without a
//! person at a console is a file written while the guest is off (probe P0), so the
//! daemons below are the offline payload; without them the guest runs nothing of ours and
//! every probe that needs a guest answer says `needs-manual`.

use std::collections::BTreeMap;
use std::io::Read;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::probe::{Ctx, Vm};
use crate::system::OfflineFile;

/// Where the system's own automount shows the share once a user session exists. The
/// probes read it to report whether it is usable without one; they do not depend on it.
pub const AUTOMOUNT_DIR: &str = "/Volumes/My Shared Files/kbf";
/// Where the root mount daemon mounts the automount tag itself, so the guest side needs
/// no user session.
pub const GUEST_MOUNT: &str = "/private/var/kbf-mnt";
/// The share as the guest daemons use it: the shared folder `kbf` under [`GUEST_MOUNT`].
pub const GUEST_SHARE: &str = "/private/var/kbf-mnt/kbf";
/// The virtiofs tag the system's automount uses.
pub const AUTOMOUNT_TAG: &str = "com.apple.virtio-fs.automount";
/// The most a result file may hold before the host refuses to read it.
pub const MAX_RESULT_BYTES: u64 = 1 << 20;
/// The custom virtiofs tag of the second share.
pub const TAG: &str = "kbfprobe";
/// The vsock port the host listens on and the guest tries.
pub const VSOCK_PORT: u32 = 5000;
/// The console port name the host offers and the guest tries.
pub const CONSOLE_NAME: &str = "kbf.probe";

pub const RESULTS: &str = "results";
pub const GUEST_BIN: &str = "kbf-guest";
/// Results written by the daemon that runs as root (the control arm).
pub const ROOT_RESULT: &str = "guest-probe-root.json";
/// Results written by the daemon that runs as the unprivileged user `nobody`.
pub const USER_RESULT: &str = "guest-probe-user.json";
/// Markers: a daemon ran (`system`), the login window exists (`login`), and, when the
/// image has an auto-login user with the agent installed, the Aqua session (`aqua`).
pub const READY_SYSTEM: &str = "ready-system";
pub const READY_LOGIN: &str = "ready-login";
pub const READY_AQUA: &str = "ready-aqua";
/// The label of the daemon that needs nothing else in the payload.
pub const LOCAL_LABEL: &str = "com.kbf.probe.local";
/// The file launchd writes the output of the `com.kbf.probe.local` daemon to, on the
/// guest's own disk. The daemon is one system tool and depends on nothing else in the
/// payload (no mount, no share, no kbf-guest, no shell), so the file's presence after a
/// boot proves launchd loaded our plists, its modification time says when, and `image
/// harvest` reads it.
pub const LOCAL_MARKER: &str = "/private/var/log/kbf-probe-local";

/// The host side of the share for one VM.
#[derive(Debug, Clone)]
pub struct Share {
    pub dir: PathBuf,
    pub tagged: PathBuf,
}

impl Share {
    #[must_use]
    pub fn results(&self) -> PathBuf {
        self.dir.join(RESULTS)
    }
}

/// Makes fresh share directories for the VM `name`, with the results directory and, when
/// given, the guest binary. A directory a previous run left is removed first (a symbolic
/// link is removed, not followed). The results and tagged directories are world-writable
/// because the guest's unprivileged daemons write into them.
///
/// # Errors
/// A directory or the copy could not be made.
pub fn prepare_share(ctx: &Ctx<'_>, name: &str) -> Result<Share, String> {
    let dir = ctx.work.join(format!("{name}-share"));
    let tagged = ctx.work.join(format!("{name}-tagged"));
    let results = dir.join(RESULTS);
    remove_leftover(&dir);
    remove_leftover(&tagged);
    make_dir(&dir, 0o755)?;
    make_dir(&tagged, 0o777)?;
    make_dir(&results, 0o777)?;
    if let Some(bin) = &ctx.guest_binary {
        copy_exclusive(bin, &dir.join(GUEST_BIN))
            .map_err(|e| format!("copy {} into the share: {e}", bin.display()))?;
    }
    Ok(Share { dir, tagged })
}

/// Makes the new directory `path` with exactly `mode` (not reduced by the umask).
fn make_dir(path: &Path, mode: u32) -> Result<(), String> {
    std::fs::create_dir(path)
        .and_then(|()| std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)))
        .map_err(|e| format!("create {}: {e}", path.display()))
}

/// Removes whatever is at `path` without following a symbolic link. A removal that fails
/// is not reported here: the `create_dir` that follows fails on what is left.
fn remove_leftover(path: &Path) {
    let _ = match std::fs::symlink_metadata(path) {
        Err(_) => Ok(()),
        Ok(m) if m.is_dir() => std::fs::remove_dir_all(path),
        Ok(_) => std::fs::remove_file(path),
    };
}

/// Copies `src` to a new file `dst` that must not exist (`O_EXCL`), mode 0755.
fn copy_exclusive(src: &Path, dst: &Path) -> std::io::Result<()> {
    let mut from = std::fs::File::open(src)?;
    let mut to = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o755)
        .open(dst)?;
    std::io::copy(&mut from, &mut to)?;
    to.set_permissions(std::fs::Permissions::from_mode(0o755))
}

/// Reads a result file the guest wrote. The share is guest-writable and the host reads
/// it, so a symbolic link, a pipe, a device or a huge file is refused rather than read.
///
/// # Errors
/// The path is not a regular file, is too large, or cannot be read.
pub fn read_result(path: &Path) -> Result<String, String> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|e| e.to_string())?;
    let meta = file.metadata().map_err(|e| e.to_string())?;
    if !meta.is_file() {
        return Err("not a regular file".to_owned());
    }
    let mut text = String::new();
    file.take(MAX_RESULT_BYTES + 1)
        .read_to_string(&mut text)
        .map_err(|e| e.to_string())?;
    if text.len() as u64 > MAX_RESULT_BYTES {
        return Err(format!("larger than {MAX_RESULT_BYTES} bytes"));
    }
    Ok(text)
}

/// What a guest run found.
#[derive(Debug)]
pub struct GuestRun {
    /// Result files that came back, by file name.
    pub results: BTreeMap<String, Value>,
    /// Result files that did not come back within the time.
    pub missing: Vec<String>,
    /// Vsock connections and console writes the host saw.
    pub traffic: Value,
}

/// Waits for each of `wanted` (file names under the results directory) and reads them.
pub fn collect(ctx: &Ctx<'_>, vm: &mut Vm, share: &Share, wanted: &[&str]) -> GuestRun {
    let mut results = BTreeMap::new();
    let mut missing = Vec::new();
    for name in wanted {
        let path = share.results().join(name);
        if vm
            .wait_file(&path, ctx.timing.guest_timeout, ctx.timing.tick)
            .is_none()
        {
            missing.push((*name).to_owned());
            continue;
        }
        let value = read_result(&path)
            .and_then(|t| {
                serde_json::from_str(&t).map_err(|e| {
                    let head: String = t.chars().take(200).collect();
                    format!("not JSON ({e}): {head}")
                })
            })
            .unwrap_or_else(|e| serde_json::json!({"unreadable": e}));
        results.insert((*name).to_owned(), value);
    }
    // Let a late connection or console write arrive before the traffic is read.
    vm.watch(ctx.timing.tick, ctx.timing.tick, false);
    GuestRun {
        results,
        missing,
        traffic: vm.guest_traffic(),
    }
}

/// The reason text for a probe whose guest wrote nothing.
#[must_use]
pub fn no_agent_reason(have_binary: bool, missing: &[String]) -> String {
    let binary = if have_binary {
        ""
    } else {
        "no --guest-binary was given, so the share holds no kbf-guest; "
    };
    format!(
        "{binary}the guest wrote no {}. Possible causes: the base image has no launch daemons (install them \
         as root with `kbf-vmm image setup --base <dir>`, or build the image with `--offline-setup`); the share \
         did not mount in the guest (see /var/log/com.kbf.probe.mount.log in the guest); the guest could not \
         write into the results directory (see /var/log/com.kbf.probe.*.log); or the guest is slower than the \
         wait. To run by hand, log in once and run \
         `{GUEST_SHARE}/{GUEST_BIN} probe-guest --out {GUEST_SHARE}/{RESULTS}/<file>`",
        missing.join(", ")
    )
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// A launch daemon plist that runs `args` at load, as `user` when given, restarting
/// itself every few seconds until the share is mounted and the command succeeds.
#[must_use]
pub fn daemon_plist(
    label: &str,
    user: Option<&str>,
    args: &[String],
    stdout: Option<&str>,
) -> String {
    let args: String = args
        .iter()
        .map(|a| format!("    <string>{}</string>\n", escape(a)))
        .collect();
    let user = user
        .map(|u| format!("  <key>UserName</key>\n  <string>{}</string>\n", escape(u)))
        .unwrap_or_default();
    let stdout = stdout
        .map(|p| {
            format!(
                "  <key>StandardOutPath</key>\n  <string>{}</string>\n",
                escape(p)
            )
        })
        .unwrap_or_default();
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
<plist version=\"1.0\">\n<dict>\n  <key>Label</key>\n  <string>{label}</string>\n  <key>ProgramArguments</key>\n  <array>\n{args}  </array>\n{user}  <key>RunAtLoad</key>\n  <true/>\n  <key>KeepAlive</key>\n  <dict>\n    <key>SuccessfulExit</key>\n    <false/>\n  </dict>\n  <key>ThrottleInterval</key>\n  <integer>3</integer>\n  <key>StandardErrorPath</key>\n  <string>/var/log/{label}.log</string>\n{stdout}</dict>\n</plist>\n"
    )
}

fn guest_args(sub: &[&str]) -> Vec<String> {
    let mut v = vec![format!("{GUEST_SHARE}/{GUEST_BIN}")];
    v.extend(sub.iter().map(|s| (*s).to_owned()));
    v
}

/// The files the offline write puts into the guest: the Setup Assistant marker, the mount
/// point directory, and six launch daemons (the share mounter, a probe as root, a probe
/// as `nobody`, the system marker, the login marker, and the local marker that needs
/// none of the others).
#[must_use]
pub fn offline_payload() -> Vec<OfflineFile> {
    let out = |name: &str| format!("{GUEST_SHARE}/{RESULTS}/{name}");
    let wait = GUEST_SHARE;
    let mut files = vec![
        OfflineFile {
            path: "private/var/db/.AppleSetupDone".to_owned(),
            contents: Vec::new(),
            mode: 0o644,
        },
        // Creates the mount point; the mount hides the file.
        OfflineFile {
            path: "private/var/kbf-mnt/.keep".to_owned(),
            contents: Vec::new(),
            mode: 0o644,
        },
    ];
    let daemons: [(&str, Option<&str>, Vec<String>); 6] = [
        (
            "com.kbf.probe.mount",
            None,
            vec![
                "/sbin/mount_virtiofs".to_owned(),
                AUTOMOUNT_TAG.to_owned(),
                GUEST_MOUNT.to_owned(),
            ],
        ),
        (
            "com.kbf.probe.root",
            None,
            guest_args(&[
                "probe-guest",
                "--wait-path",
                wait,
                "--out",
                &out(ROOT_RESULT),
                "--vsock-port",
                &VSOCK_PORT.to_string(),
                "--console-name",
                CONSOLE_NAME,
                "--tag",
                TAG,
                "--automount-dir",
                AUTOMOUNT_DIR,
                "--mountpoint",
                "/private/tmp/kbfmnt-root",
            ]),
        ),
        (
            "com.kbf.probe.user",
            Some("nobody"),
            guest_args(&[
                "probe-guest",
                "--wait-path",
                wait,
                "--out",
                &out(USER_RESULT),
                "--vsock-port",
                &VSOCK_PORT.to_string(),
                "--console-name",
                CONSOLE_NAME,
                "--tag",
                TAG,
                "--automount-dir",
                AUTOMOUNT_DIR,
                "--mountpoint",
                "/private/tmp/kbfmnt-user",
            ]),
        ),
        (
            "com.kbf.probe.system",
            Some("nobody"),
            guest_args(&[
                "probe-ready",
                "--wait-path",
                wait,
                "--out",
                &out(READY_SYSTEM),
            ]),
        ),
        (
            "com.kbf.probe.login",
            Some("nobody"),
            guest_args(&[
                "probe-ready",
                "--wait-path",
                wait,
                "--wait-process",
                "loginwindow",
                "--out",
                &out(READY_LOGIN),
            ]),
        ),
        (
            LOCAL_LABEL,
            None,
            [
                "/usr/sbin/sysctl",
                "kern.boottime",
                "kern.osversion",
                "kern.osproductversion",
            ]
            .map(str::to_owned)
            .to_vec(),
        ),
    ];
    for (label, user, args) in daemons {
        files.push(OfflineFile {
            path: format!("Library/LaunchDaemons/{label}.plist"),
            contents: daemon_plist(
                label,
                user,
                &args,
                (label == LOCAL_LABEL).then_some(LOCAL_MARKER),
            )
            .into_bytes(),
            mode: 0o644,
        });
    }
    files
}

/// True when `dir` holds the guest binary.
#[must_use]
pub fn has_guest_binary(share: &Share) -> bool {
    share.dir.join(GUEST_BIN).is_file()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::{FakeSystem, ctx, holds, tmp, work_with_base};

    /// Catches plist text that is not escaped (a path with `&` would corrupt the file
    /// launchd reads), a missing user key for the unprivileged daemons, and one present
    /// for the root control.
    #[test]
    fn a_daemon_plist_escapes_and_names_its_user() {
        let user = daemon_plist(
            "com.kbf.x",
            Some("nobody"),
            &["/a&b".into(), "<c>".into()],
            None,
        );
        assert!(
            holds(&[
                user.contains("<string>/a&amp;b</string>"),
                user.contains("&lt;c&gt;")
            ]),
            "{user}"
        );
        assert!(
            user.contains("<key>UserName</key>\n  <string>nobody</string>"),
            "{user}"
        );
        assert!(user.contains("<string>/var/log/com.kbf.x.log</string>"));
        let root = daemon_plist("com.kbf.y", None, &["/bin/true".into()], Some("/o&p"));
        assert!(!root.contains("UserName"), "{root}");
        assert!(
            root.contains("<key>StandardOutPath</key>\n  <string>/o&amp;p</string>"),
            "{root}"
        );
        assert!(!user.contains("StandardOutPath"), "{user}");
    }

    /// Catches a payload that lacks the marker that skips Setup Assistant, the mount point
    /// directory, the root daemon that mounts the share without a user session, a daemon
    /// the guest would never start, a daemon that waits for a path the mounter does not
    /// make, or a root control that runs as nobody (the G3 verdict compares the two users).
    #[test]
    fn the_offline_payload_holds_the_marker_the_mount_point_and_six_daemons() {
        let files = offline_payload();
        let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths[0], "private/var/db/.AppleSetupDone");
        assert_eq!(paths[1], "private/var/kbf-mnt/.keep");
        assert_eq!(paths.len(), 8);
        assert!(paths[2..].iter().all(|p| holds(&[
            p.starts_with("Library/LaunchDaemons/com.kbf.probe."),
            p.ends_with(".plist")
        ])));
        let text = |i: usize| String::from_utf8(files[i].contents.clone()).expect("utf8");
        assert!(holds(&[
            !text(2).contains("UserName"),
            text(2).contains("/sbin/mount_virtiofs"),
            text(2).contains(AUTOMOUNT_TAG),
            text(2).contains(&format!("<string>{GUEST_MOUNT}</string>")),
        ]));
        assert!(holds(&[
            !text(3).contains("UserName"),
            text(3).contains(ROOT_RESULT),
            text(3).contains(AUTOMOUNT_DIR),
        ]));
        assert!(holds(&[
            text(4).contains("<string>nobody</string>"),
            text(4).contains(USER_RESULT)
        ]));
        assert!(text(5).contains(READY_SYSTEM));
        assert!(holds(&[
            text(6).contains("loginwindow"),
            text(6).contains(READY_LOGIN)
        ]));
        assert_eq!(paths[7], "Library/LaunchDaemons/com.kbf.probe.local.plist");
        assert!(files.iter().all(|f| f.mode == 0o644));
        // The daemons run the binary from, and wait for, the path the mounter provides.
        assert!(GUEST_SHARE.starts_with(GUEST_MOUNT));
        for i in 3..7 {
            assert!(text(i).contains(&format!("<string>{GUEST_SHARE}/{GUEST_BIN}</string>")));
            assert!(text(i).contains(&format!("<string>{GUEST_SHARE}</string>")));
        }
    }

    /// Catches a local-marker daemon that depends on the share or a shell (it must run
    /// when the mount never happens), runs as a user that cannot write the log directory,
    /// writes somewhere the harvest allowlist does not read, or would be restarted forever
    /// (the plist restarts a daemon that exits non-zero), and a log name the harvest's
    /// pattern would miss.
    #[test]
    fn the_local_daemon_needs_nothing_and_writes_where_harvest_reads() {
        let files = offline_payload();
        let plist = String::from_utf8(files[7].contents.clone()).expect("utf8");
        assert!(
            holds(&[
                plist.contains("<string>com.kbf.probe.local</string>"),
                plist.contains("<string>/usr/sbin/sysctl</string>"),
                plist.contains("<string>kern.boottime</string>"),
                // The label, the tool and its three keys, and the two log paths: no shell.
                plist.matches("<string>").count() == 7,
                !plist.contains("UserName"),
                !plist.contains(GUEST_SHARE),
                !plist.contains(GUEST_BIN),
                plist.contains(&format!(
                    "<key>StandardOutPath</key>\n  <string>{LOCAL_MARKER}</string>"
                )),
                plist.contains("<key>RunAtLoad</key>"),
            ]),
            "{plist}"
        );
        // Only the local daemon has its output redirected.
        for f in &files[2..7] {
            let text = String::from_utf8(f.contents.clone()).expect("utf8");
            assert!(!text.contains("StandardOutPath"), "{}", f.path);
        }
        assert_eq!(
            crate::harvest::ALLOWLIST[0],
            crate::harvest::Item::File(LOCAL_MARKER.trim_start_matches('/'))
        );
        // Every daemon log the others write is read by the harvest's name pattern.
        for f in &files[2..8] {
            let name = f.path.trim_start_matches("Library/LaunchDaemons/");
            let label = name.trim_end_matches(".plist");
            assert!(label.starts_with("com.kbf.probe."), "{label}");
        }
    }

    /// Catches a share that is not made, a guest binary that is not copied into it, a
    /// results directory the guest's unprivileged daemons cannot write, and a leftover
    /// directory (or a symbolic link a guest left) that is reused instead of replaced.
    #[test]
    fn the_share_gets_a_results_directory_and_the_guest_binary() {
        let work = work_with_base("guest-share");
        let sys = FakeSystem::new();
        let mut c = ctx(&sys, &work);
        let share = prepare_share(&c, "vm").expect("share");
        assert!(holds(&[share.results().is_dir(), share.tagged.is_dir()]));
        assert!(!has_guest_binary(&share));
        let mode = |p: &Path| std::fs::metadata(p).expect("meta").permissions().mode() & 0o777;
        assert_eq!(mode(&share.results()), 0o777);
        assert_eq!(mode(&share.tagged), 0o777);
        let bin = work.join("kbf-guest-src");
        std::fs::write(&bin, "bin").expect("bin");
        c.guest_binary = Some(bin);
        let share = prepare_share(&c, "vm2").expect("share");
        assert!(has_guest_binary(&share));
        assert_eq!(mode(&share.dir.join(GUEST_BIN)), 0o755);
        c.guest_binary = Some(work.join("missing"));
        assert!(prepare_share(&c, "vm3").unwrap_err().contains("copy"));
        // A work directory that is a file cannot hold a share.
        c.work = work.join("base").join("disk.img");
        assert!(prepare_share(&c, "vm4").unwrap_err().contains("create"));
    }

    /// Catches a share directory reused with what the last guest left in it, and one
    /// that is a symbolic link being followed (the host would then write through it).
    #[test]
    fn a_leftover_share_is_replaced_and_a_symbolic_link_is_not_followed() {
        let work = work_with_base("guest-share-leftover");
        let sys = FakeSystem::new();
        let c = ctx(&sys, &work);
        let share = prepare_share(&c, "vm").expect("share");
        std::fs::write(share.results().join("old.json"), "x").expect("old");
        let again = prepare_share(&c, "vm").expect("again");
        assert!(!again.results().join("old.json").exists());
        // A guest swapped the share directory for a link to a directory outside.
        let outside = work.join("outside");
        std::fs::create_dir(&outside).expect("outside");
        std::fs::write(outside.join("keep"), "k").expect("keep");
        std::fs::remove_dir_all(&again.dir).expect("rm");
        std::os::unix::fs::symlink(&outside, &again.dir).expect("link");
        prepare_share(&c, "vm").expect("replaced");
        assert!(outside.join("keep").exists(), "the target was left alone");
        // A file where the directory goes is removed too.
        std::fs::remove_dir_all(&again.tagged).expect("rm tagged");
        std::fs::write(&again.tagged, "f").expect("file");
        assert!(prepare_share(&c, "vm").expect("share").tagged.is_dir());
    }

    /// Catches a result reader that follows a symbolic link, opens a pipe, reads a
    /// directory, or reads without a limit.
    #[test]
    fn a_result_is_read_only_from_a_small_regular_file() {
        let dir = tmp("guest-read-result");
        let f = dir.join("r");
        std::fs::write(&f, "ok").expect("f");
        assert_eq!(read_result(&f).expect("read"), "ok");
        let link = dir.join("link");
        std::os::unix::fs::symlink(&f, &link).expect("link");
        assert!(read_result(&link).is_err(), "a link is not followed");
        assert!(read_result(&dir.join("missing")).is_err());
        assert!(read_result(&dir).unwrap_err().contains("regular"));
        let big = dir.join("big");
        std::fs::write(
            &big,
            vec![b'a'; usize::try_from(MAX_RESULT_BYTES).expect("n") + 1],
        )
        .expect("big");
        assert!(read_result(&big).unwrap_err().contains("larger"));
        let limit = dir.join("limit");
        std::fs::write(
            &limit,
            vec![b'a'; usize::try_from(MAX_RESULT_BYTES).expect("n")],
        )
        .expect("limit");
        assert!(read_result(&limit).is_ok());
        std::fs::write(dir.join("bin"), [0xff, 0xfe]).expect("bin");
        assert!(read_result(&dir.join("bin")).is_err());
    }

    /// Catches a collector that reports a file it never saw, parses nothing, or lets a
    /// file that is not JSON (or not a file) stop the run.
    #[test]
    fn collect_reads_the_files_that_came_back() {
        let work = work_with_base("guest-collect");
        let sys = FakeSystem::new();
        let c = ctx(&sys, &work);
        let share = prepare_share(&c, "vm").expect("share");
        std::fs::write(share.results().join("a.json"), r#"{"k":1}"#).expect("a");
        std::fs::write(share.results().join("b.json"), "not json").expect("b");
        std::fs::create_dir(share.results().join("c.json")).expect("c");
        let mut vm = crate::probe::Vm::spawn(
            &sys,
            "vm",
            &crate::spec::VmSpec::minimal(work.join("base")),
            None,
        )
        .expect("vm");
        let run = collect(
            &c,
            &mut vm,
            &share,
            &["a.json", "b.json", "c.json", "d.json"],
        );
        assert_eq!(run.results["a.json"]["k"], 1);
        assert!(
            run.results["b.json"]["unreadable"]
                .as_str()
                .expect("text")
                .contains("not JSON")
        );
        assert!(run.results["c.json"]["unreadable"].is_string());
        assert_eq!(run.missing, vec!["d.json".to_owned()]);
        assert_eq!(run.traffic["vsock_connections"], serde_json::json!([]));
    }

    /// Catches a reason that does not tell the operator what to do, or that blames the
    /// image when the binary was simply not given.
    #[test]
    fn the_no_agent_reason_names_the_fix() {
        let with = no_agent_reason(true, &["x.json".into()]);
        assert!(
            holds(&[
                with.contains("x.json"),
                with.contains("image setup --base"),
                with.contains("mount"),
                !with.contains("--guest-binary")
            ]),
            "{with}"
        );
        let without = no_agent_reason(false, &["x.json".into(), "y.json".into()]);
        assert!(
            holds(&[
                without.contains("--guest-binary"),
                without.contains("x.json, y.json")
            ]),
            "{without}"
        );
    }
}
