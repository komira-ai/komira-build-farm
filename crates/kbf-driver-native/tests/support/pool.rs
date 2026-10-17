//! Throwaway pool users for the root tests of the lease-user pool (`tests/slots.rs`,
//! `tests/slot_clean.rs`): a scratch directory every user may search, users made with
//! `dscl` at free uids from [`FIRST_UID`] with their homes in [`HOMES`], and the checks
//! on their processes.

use std::collections::BTreeSet;
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use kbf_daemon::uid;
use kbf_driver_native::NativeConfig;
use kbf_driver_native::slots::{Pool, SlotUser};

use super::PROMPT;

/// Where the users' uids start; kbf-mac-session's root tests
/// (crates/kbf-mac-session/tests/as_root.rs, `FIRST_UID` to `LAST_UID`) use 650 to 679.
pub const FIRST_UID: u32 = 690;
/// The next uid a test of this process may take.
static NEXT_UID: AtomicU32 = AtomicU32::new(FIRST_UID);
/// The pool users' primary group: staff.
pub const GID: u32 = 20;
/// Where the users' homes are: a root-owned folder that none of the places the clean of
/// a slot sweeps holds (`/private/tmp`, `/private/var/tmp`, the `Shared` folder under
/// `/Users`), and that Spotlight does not index (a `.noindex` name), as the pool's
/// volume is not: Spotlight starts `mdworker` as the user whose files changed, which
/// the clean's check would find after the kill. Made if missing, root's, mode 0755.
pub const HOMES: &str = "/private/var/kbf-ci-homes.noindex";

/// Stops a test run as the runner's user, where every check would fail for the wrong
/// reason. Asks the kernel, not the code under test.
pub fn assert_root(test: &str) {
    // SAFETY: geteuid takes no arguments and cannot fail.
    let euid = unsafe { libc::geteuid() };
    assert_eq!(
        euid, 0,
        "not root: run `cargo xtask sudo-test -p kbf-driver-native --test {test}`"
    );
}

/// A scratch directory under `/private/tmp`, root's, mode 0755 (every pool user may
/// search it: the runner's home is not searchable by them), holding a copy of this test
/// binary as `kbf-act` (so a pool user can run the act steps), removed when the test
/// ends.
pub struct Scratch(pub PathBuf);

impl Scratch {
    pub fn new(name: &str) -> Self {
        let dir = PathBuf::from(format!(
            "/private/tmp/kbf-ci-slots-{name}-{}",
            std::process::id()
        ));
        let _ = kbf_outputs::remove_tree(&dir);
        std::fs::create_dir_all(&dir).expect("scratch");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        let act = dir.join(kbf_testkit::act::NAME);
        std::fs::copy(std::env::current_exe().expect("this binary"), &act).expect("copy");
        std::fs::set_permissions(&act, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        Self(dir)
    }

    pub fn act(&self) -> String {
        self.0
            .join(kbf_testkit::act::NAME)
            .to_string_lossy()
            .into_owned()
    }

    /// A copy of this test binary named `kbf-as-user` (the name `kbf-act` selects the act
    /// steps instead): run with `__as-user <uid> <gid> -- <program>`, it drops to the uid
    /// and executes the program, as `kbf-daemon` does for an action.
    pub fn as_user_binary(&self) -> String {
        let path = self.0.join("kbf-as-user");
        if !path.exists() {
            std::fs::copy(std::env::current_exe().expect("this binary"), &path).expect("copy");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        }
        path.to_string_lossy().into_owned()
    }

    pub fn leases(&self) -> PathBuf {
        self.0.join("leases")
    }

    /// The root directory of lease `1.seq`'s directory.
    pub fn lease_root(&self, seq: u64) -> PathBuf {
        self.leases().join(format!("lease-1-{seq}")).join("root")
    }

    /// A copy of `/bin/sleep`, root's and setuid: started by a slot user, it runs with
    /// the effective uid 0 and the user's real uid.
    pub fn setuid_sleep(&self) -> PathBuf {
        let path = self.0.join("setuid-sleep");
        std::fs::copy("/bin/sleep", &path).expect("copy sleep");
        std::os::unix::fs::chown(&path, Some(0), Some(0)).expect("chown");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o4755)).expect("chmod");
        path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = kbf_outputs::remove_tree(&self.0);
    }
}

/// Throwaway users, deleted with every process of their uids and their homes when
/// dropped.
pub struct Users(pub Vec<SlotUser>);

impl Users {
    /// `count` users `kbf-ci-slot-<name>-<i>`, at free uids from [`FIRST_UID`] that no
    /// earlier test of this process used, their homes `<HOMES>/<user>`, mode 0700,
    /// their own. Never a uid again: this process's user database cache keeps the name
    /// a deleted user had (`getpwuid` named the first test's user for the next test's
    /// uid on the runner).
    pub fn new(name: &str, count: usize) -> Self {
        let taken = taken_uids();
        let from = NEXT_UID.load(Ordering::SeqCst);
        let uids: Vec<u32> = (from..FIRST_UID + 100)
            .filter(|u| !taken.contains(u))
            .take(count)
            .collect();
        NEXT_UID.store(uids.last().map_or(from, |u| u + 1), Ordering::SeqCst);
        std::fs::create_dir_all(HOMES).expect("homes");
        std::fs::set_permissions(HOMES, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        let mut users = Self(Vec::new());
        for (i, uid) in uids.into_iter().enumerate() {
            let user = SlotUser {
                name: format!("kbf-ci-slot-{name}-{i}"),
                uid,
                gid: GID,
            };
            let home = home_of(&user);
            let _ = kbf_outputs::remove_tree(&home);
            std::fs::create_dir(&home).expect("home");
            std::os::unix::fs::chown(&home, Some(uid), Some(GID)).expect("chown");
            std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700)).expect("chmod");
            let record = format!("/Users/{}", user.name);
            let home = home.to_string_lossy().into_owned();
            let (uid_text, gid_text) = (uid.to_string(), GID.to_string());
            let attrs: [&[&str]; 6] = [
                &[],
                &["UniqueID", uid_text.as_str()],
                &["PrimaryGroupID", gid_text.as_str()],
                &["UserShell", "/usr/bin/false"],
                &["NFSHomeDirectory", home.as_str()],
                &["IsHidden", "1"],
            ];
            for attr in attrs {
                let create: [&str; 3] = [".", "-create", record.as_str()];
                run("/usr/bin/dscl", &[&create[..], attr].concat());
            }
            users.0.push(user);
        }
        assert_eq!(
            users.0.len(),
            count,
            "no {count} free uids from {FIRST_UID}"
        );
        run("/usr/bin/dscacheutil", &["-flushcache"]);
        for user in &users.0 {
            wait_until(&format!("uid {} is {}", user.uid, user.name), || {
                name_of(user.uid).as_deref() == Some(user.name.as_str())
            });
        }
        users
    }

    pub fn pool(&self) -> Pool {
        Pool::new(self.0.clone()).expect("pool")
    }
}

impl Drop for Users {
    fn drop(&mut self) {
        for user in &self.0 {
            if let Err(why) = uid::end(user.uid, Duration::from_secs(10)) {
                eprintln!("cleanup: {why}");
            }
            let _ = Command::new("/usr/bin/dscl")
                .args([".", "-delete", &format!("/Users/{}", user.name)])
                .status();
            let _ = Command::new("/usr/bin/chflags")
                .args(["-R", "nouchg"])
                .arg(home_of(user))
                .status();
            let _ = kbf_outputs::remove_tree(&home_of(user));
        }
        let _ = Command::new("/usr/bin/dscacheutil")
            .arg("-flushcache")
            .status();
    }
}

/// The home `user` gets: `<HOMES>/<name>`.
pub fn home_of(user: &SlotUser) -> PathBuf {
    Path::new(HOMES).join(&user.name)
}

/// Every uid the local directory holds.
fn taken_uids() -> BTreeSet<u32> {
    let out = Command::new("/usr/bin/dscl")
        .args([".", "-list", "/Users", "UniqueID"])
        .output()
        .expect("dscl");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.split_whitespace().last()?.parse().ok())
        .collect()
}

/// The name the user database gives `uid`.
pub fn name_of(uid: u32) -> Option<String> {
    let out = Command::new("/usr/bin/id")
        .args(["-un", &uid.to_string()])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// What a failed first lease needs to be classified later: for each of `users`, what
/// the user database says, the home with its owner, mode, flags and ACLs and what it
/// holds, the head of the user's launchd domain, and the uid's processes. Each command's
/// output is kept whether it succeeded or not. Meant for the message of an assertion
/// that fails, so it costs nothing when the lease passed.
pub fn slot_report(users: &[SlotUser]) -> String {
    let mut report = String::new();
    for user in users {
        let (uid, name) = (user.uid.to_string(), user.name.as_str());
        let record = format!("/Users/{name}");
        let home = home_of(user);
        let home = home.to_string_lossy().into_owned();
        let domain = format!("user/{uid}");
        let steps: [(&str, Vec<&str>, usize); 6] = [
            ("/usr/bin/id", vec![name], 5),
            (
                "/usr/bin/dscl",
                vec![
                    ".",
                    "-read",
                    record.as_str(),
                    "UniqueID",
                    "NFSHomeDirectory",
                ],
                5,
            ),
            ("/bin/ls", vec!["-leaOd@", home.as_str()], 5),
            ("/bin/ls", vec!["-leaOR@", home.as_str()], 40),
            ("/bin/launchctl", vec!["print", domain.as_str()], 30),
            (
                "/bin/ps",
                vec!["-U", uid.as_str(), "-o", "pid,ppid,stat,command"],
                40,
            ),
        ];
        report.push_str(&format!("--- slot user {name} (uid {uid}) ---\n"));
        for (program, args, lines) in steps.iter() {
            report.push_str(&format!("$ {program} {}\n", args.join(" ")));
            match Command::new(program).args(args).output() {
                Ok(out) => {
                    let text = format!(
                        "{}{}",
                        String::from_utf8_lossy(&out.stdout),
                        String::from_utf8_lossy(&out.stderr)
                    );
                    for line in text.lines().take(*lines) {
                        report.push_str(line);
                        report.push('\n');
                    }
                    report.push_str(&format!("[{}]\n", out.status));
                }
                Err(why) => report.push_str(&format!("[not run: {why}]\n")),
            }
        }
    }
    report
}

/// The effective and real uid of `pid`, as `ps` reports them.
pub fn ids_of(pid: i32) -> Option<(u32, u32)> {
    let out = Command::new("/bin/ps")
        .args(["-o", "uid=,ruid=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut ids = text.split_whitespace().map(str::parse::<u32>);
    match (ids.next(), ids.next()) {
        (Some(Ok(euid)), Some(Ok(ruid))) => Some((euid, ruid)),
        _ => None,
    }
}

/// Runs `program` with `args` and fails the test unless it exits 0.
pub fn run(program: &str, args: &[&str]) {
    let out = Command::new(program).args(args).output().expect(program);
    assert!(
        out.status.success(),
        "{program} {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

pub fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let give_up = Instant::now() + PROMPT;
    while !done() {
        assert!(Instant::now() < give_up, "{what}: not within {PROMPT:?}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// A configuration whose leases run as `pool`, its memory never measured (so the old
/// tracker never sees a process the action starts: a kill that relies on it misses
/// whatever left the action's group).
pub fn config(scratch: &Scratch, pool: Pool) -> NativeConfig {
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();
    let mut config = NativeConfig::new(scratch.leases());
    config.poll = Duration::from_secs(3600);
    config.pool = Some(pool);
    config
}

/// A `sleep 300` started by this test, as root or as `user`.
pub fn sleeper(user: Option<&SlotUser>) -> Child {
    let mut command = Command::new("/bin/sleep");
    command.arg("300").stdin(Stdio::null());
    if let Some(user) = user {
        command.uid(user.uid).gid(user.gid);
    }
    command.spawn().expect("sleep")
}
