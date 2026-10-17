//! `kbf-cell m2` against fakes: every check of the M2 test passes on a cell that does
//! what M2 requires, and each one fails the run when the cell breaks it.
//!
//! The fakes are this test binary, run through links named `kbf-server` and `buck2` in
//! a `--bin-dir` of the test's own, and `crun`, `podman` (which must never run), `sudo`
//! and `systemctl` found first on `PATH`: a start-up hook answers as the program its argv0
//! names before the test
//! harness starts. Each fake appends its call to `<dir>/calls` and plants the defect
//! `KBF_M2_FAKE_DEFECT` names, if it is one of its own. The fake `sudo` stands in for
//! systemd too: its `systemd-run` writes what the daemon would log (`welcomed`, or the
//! daemon's refusal of a unit without `Delegate=yes`). The daemon's cgroup is a tree
//! the test writes under `--cgroup-root`.
//!
//! The real cell is checked by the `ci / m2-container` job, which runs `kbf-cell m2`
//! with the real server and daemon, rootless `crun`, systemd, and the pinned buck2.

#![cfg(target_os = "linux")]

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use kbf_it::m2::{IMAGE, has_interpreter};

const BIN: &str = env!("CARGO_BIN_EXE_kbf-cell");

/// The daemon's unit cgroup, as the fake `systemctl` names it.
const CGROUP: &str = "/kbf.slice/kbf-daemon.slice/kbf-m2-daemon.service";

// The fakes' start-up hook: before the harness reads its arguments, this binary
// answers as the program its argv0 (or `KBF_M2_FAKE_ROLE`) names, if it names one.
const _: () = {
    extern "C" fn fake_hook() {
        fake::hook();
    }
    #[used]
    #[unsafe(link_section = ".init_array")]
    static FAKE_HOOK: extern "C" fn() = fake_hook;
};

/// The fake programs.
mod fake {
    use std::io::Write as _;
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::time::Duration;

    /// The directory the fakes write `calls` to.
    pub const DIR: &str = "KBF_M2_FAKE_DIR";
    /// The defect to plant.
    pub const DEFECT: &str = "KBF_M2_FAKE_DEFECT";
    /// The cgroup root the test wrote.
    pub const CGROUP_ROOT: &str = "KBF_M2_FAKE_CGROUP_ROOT";
    /// A role for a process the fakes start themselves.
    const ROLE: &str = "KBF_M2_FAKE_ROLE";
    /// The file the late writer appends to.
    const LATE_FILE: &str = "KBF_M2_FAKE_LATE_FILE";
    /// The main pid the fake `systemctl` reports.
    pub const PID: &str = "4242";
    /// The server's lines the harness counts.
    const HOG: &str = "\x1b[33m WARN\x1b[0m kbf_server: the action passed its memory limit \
                       with 1 GiB booked; it runs again with 2 GiB booked";
    const RESTARTED: &str = "\x1b[32m INFO\x1b[0m kbf_server: the daemon restarted, and its \
                             successor killed the run before registering";

    pub fn hook() {
        let mut args = std::env::args();
        let argv0 = args.next().unwrap_or_default();
        let role = std::env::var(ROLE).unwrap_or_else(|_| {
            let name = Path::new(&argv0).file_name().and_then(|n| n.to_str());
            name.unwrap_or_default().to_owned()
        });
        let roles = [
            "kbf-server",
            "buck2",
            "crun",
            "podman",
            "sudo",
            "systemctl",
            "late-writer",
        ];
        if !roles.contains(&role.as_str()) {
            return;
        }
        let args: Vec<String> = args.collect();
        record(&role, &args);
        let code = match role.as_str() {
            "kbf-server" => server(),
            "buck2" => buck2(&args),
            "crun" => crun(&args),
            "podman" => podman(),
            "sudo" => sudo(&args),
            "systemctl" => systemctl(&args),
            _ => late_writer(),
        };
        std::process::exit(code);
    }

    fn defect(name: &str) -> bool {
        std::env::var(DEFECT).is_ok_and(|d| d == name)
    }

    fn append(path: &Path, line: &str) {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .expect("open for append");
        writeln!(file, "{line}").expect("append");
    }

    fn dir() -> PathBuf {
        PathBuf::from(std::env::var(DIR).expect("the fakes' directory"))
    }

    /// Appends `<role>\t<working directory>\t<arguments>` to `calls`.
    fn record(role: &str, args: &[String]) {
        let cwd = std::env::current_dir().expect("working directory");
        let line = format!("{role}\t{}\t{}", cwd.display(), args.join(" "));
        append(&dir().join("calls"), &line);
    }

    /// How many calls of `role` so far, this one included, hold `word`.
    fn calls(role: &str, word: &str) -> usize {
        let calls = std::fs::read_to_string(dir().join("calls")).expect("calls");
        let prefix = format!("{role}\t");
        let mine = calls.lines().filter(|l| l.starts_with(&prefix));
        mine.filter(|l| l.split_whitespace().any(|w| w == word))
            .count()
    }

    fn server() -> i32 {
        if !defect("server-silent") {
            println!("kbf-server listening reapi=127.0.0.1:8980 worker=127.0.0.1:8981");
            std::io::stdout().flush().expect("flush");
        }
        let requeues = if defect("no-requeue") {
            0
        } else if defect("two-requeues") {
            2
        } else {
            1
        };
        for _ in 0..requeues {
            eprintln!("{HOG}");
        }
        // Until SIGINT (or a minute, so a lost one does not linger).
        std::thread::sleep(Duration::from_secs(60));
        0
    }

    /// Whether the project's `tools/kbf-m2-act` is there and executable, as an action
    /// that runs it needs.
    fn act_runs() -> bool {
        let mode = std::fs::metadata("tools/kbf-m2-act").map(|m| m.permissions().mode());
        mode.is_ok_and(|m| m & 0o111 != 0)
    }

    fn buck2(args: &[String]) -> i32 {
        let has = |word: &str| args.iter().any(|a| a == word);
        if has("--version") {
            println!("buck2 fake");
            return 0;
        }
        if has("clean") {
            return i32::from(defect("clean-fails"));
        }
        if has("kill") {
            return 0;
        }
        if !act_runs() {
            eprintln!("tools/kbf-m2-act: Permission denied");
            return 1;
        }
        if has("//:hog") {
            let out = std::env::current_dir().expect("cwd").join("buck-out-hog");
            let held = if defect("hog-short") {
                "1024\n"
            } else {
                "3072\n"
            };
            std::fs::write(&out, held).expect("hog output");
            println!("{}", out.display());
            return 0;
        }
        if has("//:pause") {
            return pause(args);
        }
        let events = args.iter().position(|a| a == "--event-log");
        let events = events.map(|i| args[i + 1].clone()).expect("an event log");
        std::fs::write(&events, b"\x1f\x8b events").expect("event log");
        if !events.contains("second") {
            if defect("build-fails") {
                eprintln!("Action failed: root//:shout");
                return 1;
            }
            if defect("build-hangs") {
                std::thread::sleep(Duration::from_secs(30));
            }
            println!("Commands: 5 (cached: 0, remote: 5, local: 0)");
            return 0;
        }
        let hits = if defect("second-runs") { 4 } else { 5 };
        println!(
            "Commands: 5 (cached: {hits}, remote: {}, local: 0)",
            5 - hits
        );
        0
    }

    /// `//:pause`: a lease in the daemon's log, its cgroup under `actions/`, the sleep.
    /// The second call is the one the daemon is killed under.
    fn pause(args: &[String]) -> i32 {
        let n = calls("buck2", "//:pause");
        let restart = n > 1;
        let work = std::env::current_dir().expect("cwd");
        let work = work.parent().expect("the work directory");
        if !defect("no-lease") {
            let lease = format!(
                "\x1b[2mT\x1b[0m \x1b[32m INFO\x1b[0m kbf_daemon::lease: lease started \
                 \x1b[3mlease\x1b[0m\x1b[2m=\x1b[0m3.{n} \
                 \x1b[3mdriver\x1b[0m\x1b[2m=\x1b[0m\"container\""
            );
            append(&work.join("logs/daemon.err"), &lease);
        }
        let name = format!("kbf-lease-3-{n}");
        if !defect("no-lease-cgroup") {
            let root = PathBuf::from(std::env::var(CGROUP_ROOT).expect("the cgroup root"));
            let actions = root
                .join(super::CGROUP.trim_start_matches('/'))
                .join("actions");
            std::fs::create_dir_all(actions.join(&name)).expect("lease cgroup");
        }
        // The lease's scratch directory, and `crun`'s state directory for its container
        // (`<lease>/crun/<id>`), which exists while the container does.
        let lease = work.join("data").join("leases").join(&name);
        let containers = if defect("no-container") {
            0
        } else if defect("two-containers") {
            2
        } else {
            1
        };
        for i in 0..containers {
            std::fs::create_dir_all(lease.join("crun").join(format!("{name}-{i}")))
                .expect("a container's state");
        }
        let secs = args
            .iter()
            .find_map(|a| a.strip_prefix("kbf_m2.sleep_secs="));
        let secs: u64 = secs.expect("a sleep").parse().expect("seconds");
        let sleep = Duration::from_secs(secs);
        // Quick, but not so quick the harness misses the container.
        let sleep = if defect("pause-quick") {
            sleep / 2
        } else {
            sleep
        };
        std::thread::sleep(sleep);
        // The container ends and the lease is cleaned, unless a defect leaves them.
        if !defect("container-left") {
            let _ = std::fs::remove_dir_all(lease.join("crun"));
        }
        if !defect("container-left") & !defect("lease-left") {
            let _ = std::fs::remove_dir_all(&lease);
        }
        let failed = if restart {
            "restart-fails"
        } else {
            "pause-fails"
        };
        if defect(failed) {
            eprintln!("Action failed: root//:pause");
            return 1;
        }
        if restart && defect("restart-local") {
            println!("Commands: 1 (cached: 0, remote: 0, local: 1)");
        } else {
            println!("Commands: 1 (cached: 0, remote: 1, local: 0)");
        }
        0
    }

    /// `crun --version`.
    fn crun(args: &[String]) -> i32 {
        if defect("crun-fails") {
            eprintln!("crun: not found");
            return 127;
        }
        if args.first().map(String::as_str) == Some("--version") {
            println!("crun version fake");
        }
        0
    }

    /// The `podman` that must never run: it fails, and its call is on record.
    fn podman() -> i32 {
        eprintln!("Error: podman was removed");
        125
    }

    fn sudo(args: &[String]) -> i32 {
        if args.first().map(String::as_str) != Some("-n") {
            eprintln!("sudo: a password is required");
            return 1;
        }
        let words: Vec<&str> = args[1..].iter().map(String::as_str).collect();
        match words.as_slice() {
            ["systemd-run", ..] => systemd_run(&args[1..]),
            ["systemctl", "kill", ..] => i32::from(defect("kill-fails")),
            ["systemctl", "stop", _] => 0,
            _ => 9,
        }
    }

    /// Starts the "daemon": its claim on the scratch directory, then `welcomed` in its
    /// log, or its refusal of a unit without `Delegate=yes`. The second unit's start
    /// also logs the server's rerun of the killed daemon's lease.
    #[expect(
        clippy::zombie_processes,
        reason = "the late writer outlives this fake on purpose, as a server's log line would"
    )]
    fn systemd_run(args: &[String]) -> i32 {
        let unit = value(args, "--unit=").expect("a unit");
        let log = PathBuf::from(value(args, "StandardError=append:").expect("a log"));
        let data = args.iter().position(|a| a == "--data-dir");
        let data = data
            .map(|i| PathBuf::from(&args[i + 1]))
            .expect("a data directory");
        let scratch = data.join("leases");
        std::fs::write(scratch.join("daemon-instance"), b"1\n").expect("claim");
        // Before `welcomed`, the daemon fetched and unpacked the image it was given
        // (`--image docker://<repo>@sha256:<hex>`): its directory in the image store.
        let image = args.iter().position(|a| a == "--image");
        let hex = image.and_then(|i| args[i + 1].rsplit_once("@sha256:").map(|(_, h)| h));
        if let Some(hex) = hex.filter(|_| !defect("image-not-unpacked")) {
            let rootfs = scratch.join("oci-images").join(hex).join("rootfs");
            std::fs::create_dir_all(rootfs.join("kbf/root")).expect("the unpacked image");
        }
        let delegated = args.windows(2).any(|w| w == ["-p", "Delegate=yes"]);
        if !delegated | defect("daemon-refuses") {
            let refusal = format!(
                "Error: cgroup {CGROUP} does not offer memory (it lists \"\"): the daemon's \
                 unit needs Delegate=yes (or Delegate=cpu memory pids)",
                CGROUP = super::CGROUP
            );
            append(&log, &refusal);
            return 0;
        }
        append(
            &log,
            "\x1b[2mT\x1b[0m \x1b[32m INFO\x1b[0m kbf_daemon: welcomed",
        );
        if unit.ends_with("-2") {
            let server = log.parent().expect("logs").join("server.err");
            if defect("two-restarts") {
                append(&server, RESTARTED);
                append(&server, RESTARTED);
            } else if defect("late-restart") {
                let me = std::env::current_exe().expect("this binary");
                let mut late = Command::new(me);
                late.env(ROLE, "late-writer").env(LATE_FILE, &server);
                late.stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null());
                late.spawn().expect("start the late writer");
            } else if !defect("no-restart") {
                append(&server, RESTARTED);
            }
        }
        0
    }

    /// The rest of the argument that starts with `prefix`.
    fn value<'a>(args: &'a [String], prefix: &str) -> Option<&'a str> {
        args.iter().find_map(|a| a.strip_prefix(prefix))
    }

    /// Logs the rerun 11 s late: past the 10 s the harness allows.
    fn late_writer() -> i32 {
        std::thread::sleep(Duration::from_secs(11));
        let file = std::env::var_os(LATE_FILE).expect("the late file");
        append(Path::new(&file), RESTARTED);
        0
    }

    fn systemctl(args: &[String]) -> i32 {
        let words: Vec<&str> = args.iter().map(String::as_str).collect();
        match words.as_slice() {
            ["show", "-P", "ControlGroup", unit] => {
                println!("/kbf.slice/kbf-daemon.slice/{unit}.service");
            }
            ["show", "-P", "MainPID", _] => println!("{PID}"),
            _ => return 9,
        }
        0
    }
}

/// A 64-bit little-endian ELF file whose program headers have these types.
fn elf(types: &[u32]) -> Vec<u8> {
    let mut bytes = vec![0u8; 64];
    bytes[..6].copy_from_slice(b"\x7fELF\x02\x01");
    bytes[0x20..0x28].copy_from_slice(&64u64.to_le_bytes());
    bytes[0x36..0x38].copy_from_slice(&56u16.to_le_bytes());
    let count = u16::try_from(types.len()).expect("a few headers");
    bytes[0x38..0x3a].copy_from_slice(&count.to_le_bytes());
    for t in types {
        let mut header = vec![0u8; 56];
        header[..4].copy_from_slice(&t.to_le_bytes());
        bytes.extend(header);
    }
    bytes
}

/// `PT_LOAD`, `PT_INTERP`, `PT_PHDR`.
const LOAD: u32 = 1;
const INTERP: u32 = 3;
const PHDR: u32 = 6;

/// A fresh directory for one run of one test.
fn scratch(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("kbf-m2-tests")
        .join(unique(name));
    std::fs::create_dir_all(&dir).expect("create a scratch directory");
    dir
}

/// A name for one run's directory that no other call returns.
fn unique(name: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after 1970")
        .as_nanos();
    unique_at(name, nanos)
}

/// [`unique`] at the clock reading `nanos`. The pid parts this run from other live
/// processes, the count from the other calls of this one (its tests are its threads,
/// which can read one clock tick), and the clock from earlier runs with this pid.
fn unique_at(name: &str, nanos: u128) -> String {
    static CALLS: AtomicU64 = AtomicU64::new(0);
    let call = CALLS.fetch_add(1, Ordering::Relaxed);
    format!("{name}-{}-{call}-{nanos}", std::process::id())
}

/// Catches: a name made of the pid and the clock alone. The tests are threads of one
/// process, so two of them that read the clock in one tick got the same scratch
/// directory.
#[test]
fn two_names_made_at_one_instant_differ() {
    assert_ne!(unique_at("m2", 7), unique_at("m2", 7));
}

/// One fake cell: the fakes in `<dir>/bin`, the daemon's cgroup under `<dir>/cgroup`, and
/// `kbf-cell m2`'s work directory `<dir>/work`.
struct Cell {
    dir: PathBuf,
    defect: String,
}

impl Cell {
    fn new(name: &str, defect: &str) -> Self {
        let dir = scratch(name);
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).expect("bin");
        let me = std::env::current_exe().expect("this test binary");
        for fake in ["kbf-server", "buck2", "crun", "podman", "sudo", "systemctl"] {
            std::os::unix::fs::symlink(&me, bin.join(fake)).expect("link a fake");
        }
        let act = match defect {
            "act-dynamic" => elf(&[PHDR, INTERP, LOAD]),
            "act-not-elf" => {
                b"a stand-in for kbf-m2-act, not an ELF file at all; long enough\n".to_vec()
            }
            _ => elf(&[PHDR, LOAD]),
        };
        // Not executable: `kbf-cell m2` must make the copy executable itself.
        std::fs::write(bin.join("kbf-m2-act"), act).expect("kbf-m2-act");
        let mode = std::fs::Permissions::from_mode(0o644);
        std::fs::set_permissions(bin.join("kbf-m2-act"), mode).expect("chmod");

        let unit = dir.join("cgroup").join(CGROUP.trim_start_matches('/'));
        std::fs::create_dir_all(unit.join("actions")).expect("cgroup");
        let root = if defect == "root-no-controllers" {
            "cpu pids\n"
        } else {
            "cpu memory pids\n"
        };
        std::fs::write(unit.join("cgroup.subtree_control"), root).expect("controllers");
        let actions = if defect == "actions-no-memory" {
            "cpu pids\n"
        } else {
            "cpu io memory pids\n"
        };
        std::fs::write(unit.join("actions/cgroup.subtree_control"), actions).expect("controllers");
        if defect != "no-supervisor" {
            std::fs::create_dir_all(unit.join("supervisor")).expect("supervisor");
            let procs = format!("{}\n4243\n", fake::PID);
            std::fs::write(unit.join("supervisor/cgroup.procs"), procs).expect("procs");
        }

        Self {
            dir,
            defect: defect.to_owned(),
        }
    }

    fn work(&self) -> PathBuf {
        self.dir.join("work")
    }

    /// `kbf-cell m2` with the fakes and 1 s pauses, and the daemon's data directory in the
    /// work directory.
    fn command(&self) -> Command {
        let mut command = self.command_without_data_dir();
        command.arg("--data-dir").arg(self.work().join("data"));
        command
    }

    /// [`Cell::command`], leaving the data directory to the cell's default.
    fn command_without_data_dir(&self) -> Command {
        let bin = self.dir.join("bin");
        let mut path = std::ffi::OsString::from(&bin);
        path.push(":");
        path.push(std::env::var_os("PATH").unwrap_or_default());
        let mut command = Command::new(BIN);
        command
            .arg("m2")
            .arg("--bin-dir")
            .arg(&bin)
            .arg("--project")
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("m2/buck2"))
            .arg("--work")
            .arg(self.work())
            .arg("--buck2")
            .arg(bin.join("buck2"))
            .args(["--pause-secs", "1", "--restart-pause-secs", "1"])
            .arg("--cgroup-root")
            .arg(self.dir.join("cgroup"))
            .env("PATH", path)
            .env(fake::DIR, &self.dir)
            .env(fake::DEFECT, &self.defect)
            .env(fake::CGROUP_ROOT, self.dir.join("cgroup"));
        command
    }

    fn run(&self) -> Output {
        self.command().output().expect("run kbf-cell m2")
    }

    /// The calls of the fake `role`: its working directory and its arguments.
    fn calls(&self, role: &str) -> Vec<(String, String)> {
        let calls = std::fs::read_to_string(self.dir.join("calls")).unwrap_or_default();
        calls
            .lines()
            .filter_map(|l| l.split_once('\t'))
            .filter(|(r, _)| *r == role)
            .filter_map(|(_, rest)| rest.split_once('\t'))
            .map(|(cwd, args)| (cwd.to_owned(), args.to_owned()))
            .collect()
    }

    fn args(&self, role: &str) -> Vec<String> {
        self.calls(role).into_iter().map(|(_, a)| a).collect()
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn both(out: &Output) -> String {
    format!(
        "status {}\n--- stdout\n{}--- stderr\n{}",
        out.status,
        text(&out.stdout),
        text(&out.stderr)
    )
}

/// Runs `kbf-cell m2` with the defect `defect`, and requires exit status `code` with
/// `why` on stderr and the logs' tails on stdout.
fn fails(defect: &str, code: i32, why: &str) -> (Cell, Output) {
    let cell = Cell::new(defect, defect);
    let out = cell.run();
    assert_eq!(out.status.code(), Some(code), "{}", both(&out));
    assert!(
        text(&out.stderr).contains(why),
        "wanted {why:?}\n{}",
        both(&out)
    );
    let stdout = text(&out.stdout);
    for tail in [
        "--- last lines of server.err",
        "--- last lines of daemon.err",
    ] {
        assert!(stdout.contains(tail), "{}", both(&out));
    }
    (cell, out)
}

/// Catches: a harness that starts the server or the daemon's unit with other flags than
/// the M2 cell needs (the unit without `Delegate=yes`, or not as this user), skips the
/// runner setup or a check, builds in another order or place, runs Podman (the fake one
/// fails and is on record), leaves the copy's `kbf-m2-act` unexecutable (the fake build
/// fails as the farm's action would), leaves a unit running, or passes without one of its
/// OK lines.
#[test]
fn a_cell_that_does_everything_passes() {
    let cell = Cell::new("passes", "none");
    let out = cell.run();
    assert_eq!(out.status.code(), Some(0), "{}", both(&out));
    let stdout = text(&out.stdout);
    let lines = [
        "crun version fake".to_owned(),
        "swap areas:".to_owned(),
        "kbf-server listening reapi=127.0.0.1:8980".to_owned(),
        "daemon joined (kbf-m2-daemon)".to_owned(),
        format!("OK: the daemon fetched {IMAGE} and unpacked it"),
        format!(
            "OK: the daemon's cgroup {CGROUP} holds supervisor/ (pid 4242), and it and \
             actions/ enable cpu memory pids"
        ),
        "OK Buck2: 5 of 5 action(s) ran on the farm, then all were remote cache hits".to_owned(),
        "OK: //:hog was killed at its own 2 GiB cap, run again with 2 GiB booked, and passed"
            .to_owned(),
        "OK: //:pause holds a lease in one running container, in the cgroup \
         actions/kbf-lease-3-1"
            .to_owned(),
        "OK: //:pause took ".to_owned(),
        "killed the daemon mid-lease".to_owned(),
        "daemon joined (kbf-m2-daemon-2)".to_owned(),
        "OK: the killed daemon's lease was requeued within ".to_owned(),
        "OK: //:pause succeeded on the farm after its daemon was killed".to_owned(),
    ];
    let mut at = 0;
    for line in &lines {
        let found = stdout[at..].find(line.as_str());
        at += found.unwrap_or_else(|| panic!("no {line:?} after byte {at}\n{}", both(&out)));
    }
    let clean = "OK: no container and no lease directory left";
    assert_eq!(stdout.matches(clean).count(), 2, "{}", both(&out));
    assert!(!stdout.contains("--- last lines"), "{}", both(&out));

    let work = cell.work();
    let w = work.display();
    let uid = rustix::process::getuid().as_raw();
    let gid = rustix::process::getgid().as_raw();
    let server = cell.args("kbf-server");
    let expected = format!(
        "--listen 127.0.0.1:8980 --worker-listen 127.0.0.1:8981 \
         --cold-store file://{w}/server-blobs --meta-dir {w}/server-meta \
         --worker-tls-cert {w}/pki/server.pem --worker-tls-key {w}/pki/server.key \
         --worker-client-ca {w}/pki/ca.pem --memory-ladder-gib 1,2,4"
    );
    assert_eq!(server, [expected]);

    let home = std::env::var("HOME").unwrap_or_default();
    let path = format!(
        "{}:{}",
        cell.dir.join("bin").display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let unit = |name: &str| {
        format!(
            "-n systemd-run --quiet --collect --unit={name} --slice=kbf-daemon.slice \
             -p Delegate=yes -p OOMScoreAdjust=-900 -p StandardError=append:{w}/logs/daemon.err \
             --uid={uid} --gid={gid} --working-directory={w} -E HOME={home} -E PATH={path} \
             {bin}/kbf-daemon --driver container \
             --server https://127.0.0.1:8981 --cas https://127.0.0.1:8981 \
             --ca-cert {w}/pki/ca.pem --cert {w}/pki/client.pem --key {w}/pki/client.key \
             --tls-server-name localhost --node-id cell-node-1 --data-dir {w}/data \
             --actions-memory-max-gib 8 --image docker://{IMAGE}",
            bin = cell.dir.join("bin").display(),
        )
    };
    let sudo = cell.args("sudo");
    assert_eq!(
        sudo,
        [
            unit("kbf-m2-daemon"),
            "-n systemctl kill --kill-whom=main -s SIGKILL kbf-m2-daemon".to_owned(),
            unit("kbf-m2-daemon-2"),
            "-n systemctl stop kbf-m2-daemon".to_owned(),
            "-n systemctl stop kbf-m2-daemon-2".to_owned(),
        ]
    );
    assert_eq!(
        cell.args("systemctl"),
        [
            "show -P ControlGroup kbf-m2-daemon",
            "show -P MainPID kbf-m2-daemon"
        ]
    );

    // Podman never ran (the fake one fails and is on record), and crun was asked its
    // version once, by the harness: every other call is the daemon's, through a runtime
    // of its own that is not this fake.
    assert_eq!(cell.args("podman"), Vec::<String>::new());
    assert_eq!(cell.args("crun"), ["--version"]);

    let buck2 = cell.calls("buck2");
    let project = format!("{w}/project");
    assert!(
        buck2[1..].iter().all(|(cwd, _)| *cwd == project),
        "{buck2:?}"
    );
    let args: Vec<&str> = buck2.iter().map(|(_, a)| a.as_str()).collect();
    assert_eq!(args.len(), 8, "{args:?}");
    assert_eq!(args[0], "--version");
    let logs = format!("{w}/logs");
    assert_eq!(
        args[1],
        format!("build --console simple --event-log {logs}/buck2-first.json-lines.gz //...")
    );
    assert_eq!(args[2], "clean");
    assert_eq!(
        args[3],
        format!("build --console simple --event-log {logs}/buck2-second.json-lines.gz //...")
    );
    assert_eq!(
        args[4],
        "build --console none --show-full-simple-output //:hog"
    );
    let mut salts = Vec::new();
    for pause in &args[5..7] {
        let salt = pause
            .strip_prefix("build --console simple -c kbf_m2.sleep_secs=1 -c kbf_m2.salt=")
            .and_then(|rest| rest.strip_suffix(" //:pause"))
            .unwrap_or_else(|| panic!("a pause build: {pause}"));
        assert_eq!(salt.len(), 16, "{salt}");
        assert!(
            salt.bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        );
        salts.push(salt);
    }
    assert_ne!(salts[0], salts[1]);
    assert_eq!(args[7], "kill");

    // The copy holds the project and `kbf-m2-act`, executable.
    let act = work.join("project/tools/kbf-m2-act");
    assert_eq!(std::fs::read(&act).expect("the copy"), elf(&[PHDR, LOAD]));
    let mode = std::fs::metadata(&act).expect("the copy").permissions();
    assert_eq!(mode.mode() & 0o777, 0o755);
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("m2/buck2/BUCK");
    let copy = work.join("project/BUCK");
    assert_eq!(std::fs::read(copy).ok(), std::fs::read(source).ok());
    assert!(work.join("project/platforms/defs.bzl").is_file());
    for log in [
        "buck2-first.log",
        "buck2-second.log",
        "buck2-pause.log",
        "buck2-restart.log",
        "server.out",
        "server.err",
        "daemon.err",
    ] {
        assert!(work.join("logs").join(log).is_file(), "{log}");
    }
}

/// Catches: an image pin that drifts from the sample's `container_image` (every build
/// would fail on a node that pulled the other one), or a pin by tag or index.
#[test]
fn the_pulled_image_is_the_samples_manifest_pin() {
    let config = Path::new(env!("CARGO_MANIFEST_DIR")).join("m2/buck2/.buckconfig");
    let config = std::fs::read_to_string(config).expect("the sample's .buckconfig");
    let named = config
        .lines()
        .find_map(|l| l.trim().strip_prefix("container_image = docker://"));
    assert_eq!(named, Some(IMAGE));
    let (name, digest) = IMAGE.split_once("@sha256:").expect("a digest pin");
    assert_eq!(name, "gcr.io/distroless/base-debian12");
    assert_eq!(digest.len(), 64);
    assert!(
        digest
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    );
}

/// Catches: a static-link check that reads the wrong field or stops at the first
/// header, takes anything for an ELF file, or reads past a cut-off file.
#[test]
fn the_interpreter_check_reads_every_program_header() {
    assert_eq!(has_interpreter(&elf(&[PHDR, LOAD])), Some(false));
    assert_eq!(has_interpreter(&elf(&[LOAD, LOAD, INTERP])), Some(true));
    assert_eq!(has_interpreter(&elf(&[])), Some(false));
    let mut cut = elf(&[PHDR, INTERP]);
    cut.truncate(64 + 56 + 2);
    assert_eq!(has_interpreter(&cut), None);
    assert_eq!(has_interpreter(b"\x7fELF"), None);
    let mut elf32 = elf(&[INTERP]);
    elf32[4] = 1;
    assert_eq!(has_interpreter(&elf32), None);
    let mut big = elf(&[INTERP]);
    big[5] = 2;
    assert_eq!(has_interpreter(&big), None);
    // This test binary is linked dynamically, as cargo builds it.
    let me = std::fs::read(std::env::current_exe().expect("me")).expect("this binary");
    assert_eq!(has_interpreter(&me), Some(true));
}

/// Catches: a harness that copies a dynamically linked `kbf-m2-act` into the project
/// (the distroless image may not hold its libraries).
#[test]
fn a_dynamically_linked_act_fails() {
    let (cell, _) = fails(
        "act-dynamic",
        1,
        "FAIL: kbf-m2-act is dynamically linked; the image may not hold its libraries",
    );
    assert!(cell.args("kbf-server").is_empty());
}

/// Catches: a harness that takes a file it cannot read as an ELF program as static.
#[test]
fn an_act_that_is_not_elf_is_refused() {
    fails("act-not-elf", 2, "is not a 64-bit little-endian ELF file");
}

/// Catches: a harness that builds in a work directory an earlier run left.
#[test]
fn a_used_work_directory_is_refused() {
    let cell = Cell::new("used-work", "none");
    std::fs::create_dir_all(cell.work().join("project")).expect("an old project");
    // Without `--data-dir`: the daemon's goes under the system's temporary directory.
    let out = cell
        .command_without_data_dir()
        .output()
        .expect("run kbf-cell m2");
    assert_eq!(out.status.code(), Some(2), "{}", both(&out));
    assert!(
        text(&out.stderr).contains("project exists; give a fresh --work"),
        "{}",
        both(&out)
    );
    assert!(cell.args("kbf-server").is_empty());
}

/// Catches: a harness that goes on without `crun` (the runtime is not on the runner).
#[test]
fn a_runner_without_crun_fails() {
    let (cell, _) = fails("crun-fails", 1, "FAIL: crun --version failed");
    assert!(cell.args("kbf-server").is_empty());
    assert!(cell.args("podman").is_empty());
}

/// Catches: a harness that accepts a daemon that joined without the image it was given
/// in its store (every lease would fetch it, on the action path).
#[test]
fn a_daemon_that_did_not_unpack_its_image_fails() {
    fails("image-not-unpacked", 1, "FAIL: the daemon joined, but ");
}

/// Catches: a harness that goes on without the server's start line.
#[test]
fn a_server_that_does_not_start_fails() {
    fails("server-silent", 1, "FAIL: kbf-server did not start");
}

/// Catches: a harness that goes on without the daemon's `welcomed`, or whose failure
/// hides why (here, the daemon's refusal of a unit without `Delegate=yes`).
#[test]
fn a_daemon_that_does_not_join_fails_with_its_last_line() {
    fails(
        "daemon-refuses",
        1,
        "FAIL: the daemon did not join; the last line of its log: Error: cgroup \
         /kbf.slice/kbf-daemon.slice/kbf-m2-daemon.service does not offer memory",
    );
}

/// Catches: a cgroup check that does not look for the daemon in its `supervisor` leaf.
#[test]
fn a_daemon_outside_its_supervisor_leaf_fails() {
    fails(
        "no-supervisor",
        1,
        &format!(
            "FAIL: {CGROUP}/supervisor does not hold the daemon (pid 4242): its unit needs \
             Delegate=yes"
        ),
    );
}

/// Catches: a cgroup check that skips the unit cgroup's own controllers.
#[test]
fn a_unit_cgroup_without_memory_for_its_children_fails() {
    fails(
        "root-no-controllers",
        1,
        &format!("FAIL: {CGROUP} does not enable memory for its children"),
    );
}

/// Catches: a cgroup check that skips `actions/`.
#[test]
fn an_actions_cgroup_without_memory_fails() {
    fails(
        "actions-no-memory",
        1,
        &format!("FAIL: {CGROUP}/actions does not enable memory for its children"),
    );
}

/// Catches: a harness that ignores a failed build.
#[test]
fn a_failed_build_fails() {
    fails(
        "build-fails",
        1,
        "FAIL: the first buck2 build of //... failed",
    );
}

/// Catches: a harness that waits on a build without end.
#[test]
fn a_build_past_its_timeout_fails() {
    let cell = Cell::new("build-hangs", "build-hangs");
    let start = Instant::now();
    let out = cell
        .command()
        .args(["--build-timeout", "1"])
        .output()
        .expect("run kbf-cell m2");
    assert_eq!(out.status.code(), Some(1), "{}", both(&out));
    let why = "FAIL: the first buck2 build of //... failed (or ran past 1 s)";
    assert!(text(&out.stderr).contains(why), "{}", both(&out));
    assert!(start.elapsed() < Duration::from_secs(20), "{}", both(&out));
}

/// Catches: a harness that ignores a failed clean.
#[test]
fn a_failed_clean_fails() {
    fails("clean-fails", 1, "FAIL: buck2 clean failed");
}

/// Catches: a harness that skips the M1 rule: a rebuild that ran an action again.
#[test]
fn a_rebuild_that_runs_an_action_fails() {
    let (_, out) = fails(
        "second-runs",
        1,
        "FAIL: the two buck2 builds break the M1 rule",
    );
    assert!(text(&out.stdout).contains("FAIL Buck2"), "{}", both(&out));
}

/// Catches: a harness that does not require the hog's own-limit kill and rerun.
#[test]
fn a_hog_never_requeued_fails() {
    fails(
        "no-requeue",
        1,
        "FAIL: the server requeued an action from 1 GiB to 2 GiB 0 times, not once",
    );
}

/// Catches: a harness that accepts a hog requeued more than once (a doubling that did
/// not fit).
#[test]
fn a_hog_requeued_twice_fails() {
    fails(
        "two-requeues",
        1,
        "FAIL: the server requeued an action from 1 GiB to 2 GiB 2 times, not once",
    );
}

/// Catches: a harness that does not read the hog's output.
#[test]
fn a_hog_with_the_wrong_output_fails() {
    fails("hog-short", 1, "is not 3072");
}

/// Catches: a harness that looks for a container with no lease in flight, or waits for
/// one forever after the build has ended.
#[test]
fn a_pause_that_starts_no_lease_fails() {
    fails(
        "no-lease",
        1,
        "FAIL: //:pause started no lease on the daemon",
    );
}

/// Catches: a harness that does not look for the lease's container.
#[test]
fn a_lease_with_no_running_container_fails() {
    fails(
        "no-container",
        1,
        "FAIL: while //:pause ran, 0 crun containers existed, not one",
    );
}

/// Catches: a container check that accepts more than the one lease's container.
#[test]
fn a_lease_with_two_running_containers_fails() {
    fails(
        "two-containers",
        1,
        "FAIL: while //:pause ran, 2 crun containers existed, not one",
    );
}

/// Catches: a harness that does not look for the lease's cgroup under `actions/`.
#[test]
fn a_lease_with_no_lease_cgroup_fails() {
    fails(
        "no-lease-cgroup",
        1,
        &format!("{CGROUP}/actions held no kbf-lease-* cgroup"),
    );
}

/// Catches: a harness that ignores a failed `//:pause`.
#[test]
fn a_failed_pause_fails() {
    fails("pause-fails", 1, "FAIL: //:pause failed");
}

/// Catches: a harness that does not time `//:pause` (a lease that ended at once was not
/// held in flight).
#[test]
fn a_pause_shorter_than_its_sleep_fails() {
    fails("pause-quick", 1, "under its 1 s sleep");
}

/// Catches: a harness that does not check that the containers go.
#[test]
fn a_container_left_behind_fails() {
    fails("container-left", 1, "FAIL: 1 crun containers are left");
}

/// Catches: a harness that does not check the lease scratch directory, or counts the
/// daemon's own directories in it (the claim, the image store) as left behind.
#[test]
fn a_lease_directory_left_behind_fails() {
    fails(
        "lease-left",
        1,
        "FAIL: lease scratch directories are left: kbf-lease-3-1",
    );
}

/// Catches: a harness that goes on when it could not kill the daemon.
#[test]
fn a_failed_kill_fails() {
    fails(
        "kill-fails",
        1,
        "FAIL: sudo -n systemctl kill kbf-m2-daemon failed",
    );
}

/// Catches: a harness that does not require the server's rerun of the killed daemon's
/// lease.
#[test]
fn a_lease_never_rerun_fails() {
    fails(
        "no-restart",
        1,
        "FAIL: the server logged 0 restart requeues, not one",
    );
}

/// Catches: a harness that accepts the lease rerun twice.
#[test]
fn a_lease_rerun_twice_fails() {
    fails(
        "two-restarts",
        1,
        "FAIL: the server logged 2 restart requeues, not one",
    );
}

/// Catches: a harness that does not time the rerun (a server that waited out the
/// handover grace would pass).
#[test]
fn a_lease_rerun_late_fails() {
    fails("late-restart", 1, "ms after the welcome");
}

/// Catches: a harness that ignores the build that lost its daemon.
#[test]
fn a_pause_that_fails_after_the_restart_fails() {
    fails(
        "restart-fails",
        1,
        "FAIL: //:pause failed after the restart",
    );
}

/// Catches: a harness that accepts the rerun anywhere but the farm.
#[test]
fn a_rerun_that_ran_locally_fails() {
    fails(
        "restart-local",
        1,
        "FAIL: the build after the restart ran a command locally, or none remotely",
    );
}
