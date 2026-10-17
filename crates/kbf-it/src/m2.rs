//! `kbf-cell m2`: the M2 test. Real buck2, through `kbf-server`, to `kbf-daemon --driver
//! container`, every action in a rootless `crun` container of a pinned distroless
//! image.
//!
//! The cell: one `kbf-server` (blobs and metadata in directories of the cell, REAPI on 127.0.0.1:8980,
//! the worker listener on 127.0.0.1:8981 over mutual TLS, which also serves the daemon
//! its blobs) and one `kbf-daemon` with the container driver, run as a system unit with
//! `Delegate=yes` as the calling user (`sudo -n systemd-run`), the way a build host runs
//! it: it sets up its own cgroup (a `supervisor` leaf and `actions` beside it) and makes
//! a lease cgroup under `actions` for each container. Its leases may use 8 GiB together.
//!
//! First the versions (`buck2`, `crun`) and the host's swap are printed (the own-limit
//! kill of `//:hog` pushes into swap).
//!
//! What is checked, in order; the first failure ends the run with exit status 1, after
//! printing the last lines of the server's and the daemon's logs:
//! 1. The daemon is given [`IMAGE`] (`--image`, as a farm's unit is) and, before it says
//!    `Hello`, fetches it from its registry by its linux/amd64 manifest digest, checks
//!    every byte against a digest and unpacks it into its image store
//!    (`<data>/leases/oci-images/<digest>/rootfs`). As on a farm, the node's pin and the
//!    client's `container-image` (the `container_image` value in
//!    `m2/buck2/.buckconfig`) are separate settings, and the store is keyed by the
//!    digest: a client that names another image makes the node fetch that one.
//! 2. The sample project is copied under `--work` with `kbf-m2-act` from `--bin-dir` as
//!    its `tools/kbf-m2-act`, executable, which must be linked statically (no
//!    `PT_INTERP` program header: the image may not hold its libraries).
//! 3. `kbf-server` prints its ` reapi=` start line within 10 s, and the daemon's unit
//!    logs `welcomed` within 30 s (or the failure quotes its log's last line).
//! 4. The daemon's unit cgroup has the daemon's own layout: `supervisor/` holds its main
//!    process, and the cgroup and its `actions/` both enable `cpu memory pids` for their
//!    children.
//! 5. The project is built remote-only, cleaned, and built again; the M1 rule
//!    ([`check_logs`], what `kbf-cell check` applies) then requires that the first
//!    build ran every action on the farm and the second answered every one from the
//!    action cache.
//! 6. `//:hog`, built in step 5, passed its first memory cap: the server's log holds
//!    exactly one requeue of it from 1 GiB booked to 2 GiB booked (the own-limit kill's
//!    doubling), and its output is `3072`, so the second run succeeded.
//! 7. `//:pause`, under a fresh salt so it runs, holds a lease for its sleep: while the
//!    build runs, exactly one `crun` container exists (one state directory under
//!    `<lease>/crun/` of the daemon's leases), a `kbf-lease-*` cgroup is under
//!    `actions/`, and the build takes at least the sleep.
//! 8. Afterwards (within 10 s) no container is left and no lease directory is: the
//!    daemon's scratch directory holds no `kbf-lease-*`.
//! 9. A daemon killed mid-lease is rerun at once: `//:pause`, under a fresh salt, holds
//!    a lease; the daemon is killed (SIGKILL to its unit's main process) and a new one
//!    started on the same scratch directory, which names the killed one as its
//!    predecessor. The server's log holds exactly one `Restarted` requeue, logged less
//!    than 10 s after the new daemon was welcomed (the handover grace it replaces is
//!    45 s), the build succeeds with every command remote, and step 8 holds again. With
//!    systemd's default `KillMode` the killed unit's own processes, the container among
//!    them, are killed by systemd, so this exercises the sweep, the predecessor claim
//!    and the server's rerun, not the keeper.
//!
//! Each buck2 build is bounded by `--build-timeout`, so a farm that reruns an action
//! without end fails here instead of at the job's timeout. Every log is under
//! `<work>/logs`. Needs `crun`, `newuidmap` and `newgidmap` (package `uidmap`), this
//! user's subordinate ids, systemd, and `sudo -n` for `systemd-run` and `systemctl`.

use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use xtask::crun_tests::host_report;

use crate::cell::{
    CellError, LEASE_WAIT, OrBroken as _, POLL, Process, absolute, copy_tree, ensure,
    leases_started, plain, print_tails, random_hex, read, status, to_log, wait_for_line,
};
use crate::pki::PkiFiles;
use crate::summary::{Tool, check_logs, parse};

/// The image every action runs in: distroless base-debian12, its linux/amd64 manifest
/// (not the index). The sample's `.buckconfig` names the same one.
pub const IMAGE: &str = concat!(
    "gcr.io/distroless/base-debian12",
    "@sha256:d2add786f2a5f43d1ab3ae54cd3193de929d0d12c378ff60891921f56f3e47ff"
);
/// The REAPI listener, as the sample project's `.buckconfig` names it.
const REAPI: &str = "127.0.0.1:8980";
/// The worker listener.
const WORKER: &str = "127.0.0.1:8981";
/// The node id the daemon's certificate names.
const NODE_ID: &str = "cell-node-1";
/// The daemon's unit; the one started after the kill is `<UNIT>-2`.
const UNIT: &str = "kbf-m2-daemon";
/// The programs found on `PATH`.
const SUDO: &str = "sudo";
const CRUN: &str = "crun";
const SYSTEMCTL: &str = "systemctl";
/// How long a daemon's unit has to be welcomed: it fetches [`IMAGE`] first.
const JOIN_WAIT: Duration = Duration::from_secs(120);
/// How long the containers and lease directories may take to go after a build.
const LEFT_WAIT: Duration = Duration::from_secs(10);
/// How long the server has to log the killed daemon's requeue.
const REQUEUE_WAIT: Duration = Duration::from_secs(15);
/// How soon after the new daemon's welcome the requeue must be logged.
const REQUEUE_WITHIN: Duration = Duration::from_secs(10);
/// The controllers the daemon enables in its cgroup and in `actions/`.
const CONTROLLERS: [&str; 3] = ["cpu", "memory", "pids"];
/// The server's line for the hog's own-limit kill.
const HOG_REQUEUE: &str =
    "the action passed its memory limit with 1 GiB booked; it runs again with 2 GiB booked";
/// The server's line for a lease the killed daemon's successor reported.
const RESTARTED: &str = "the daemon restarted, and its successor killed the run before registering";
/// The prefix of a lease's scratch directory in the daemon's.
const LEASE_PREFIX: &str = "kbf-lease-";
/// How many lines of each log a failed run prints.
const TAIL: usize = 80;
/// `PT_INTERP`: the program header that names a program interpreter.
const PT_INTERP: u64 = 3;

/// `kbf-cell m2` flags.
#[derive(Clone, Debug, clap::Args)]
pub struct M2Args {
    /// The directory holding the built `kbf-server`, `kbf-daemon` and `kbf-m2-act`.
    #[arg(long)]
    pub bin_dir: PathBuf,
    /// The sample project (`crates/kbf-it/m2/buck2`). It is built in a copy under
    /// `--work`.
    #[arg(long)]
    pub project: PathBuf,
    /// The cell's directory: certificates, the project's copy, the daemon's leases, and
    /// `logs/`. It must not hold a `project/` yet.
    #[arg(long)]
    pub work: PathBuf,
    /// The pinned buck2.
    #[arg(long)]
    pub buck2: PathBuf,
    /// The longest any one buck2 build may take, in seconds.
    #[arg(long, default_value_t = 300)]
    pub build_timeout: u64,
    /// How long `//:pause` sleeps in step 7, in seconds.
    #[arg(long, default_value_t = 8)]
    pub pause_secs: u64,
    /// How long `//:pause` sleeps in step 9, in seconds: long enough for the kill and
    /// the new daemon's start to land in the lease.
    #[arg(long, default_value_t = 20)]
    pub restart_pause_secs: u64,
    /// Where cgroup v2 is mounted.
    #[arg(long, default_value = "/sys/fs/cgroup")]
    pub cgroup_root: PathBuf,
    /// The daemon's data directory (its leases, image store and caches). By default a
    /// directory of its own under the system's temporary directory, not under `--work`:
    /// the container driver refuses a data directory with an ancestor closed to other
    /// users (its containers' ids are not the daemon's), as a runner's home directory
    /// may be.
    #[arg(long)]
    pub data_dir: Option<PathBuf>,
}

/// Runs the M2 test (the module docs say what it checks). Prints what it does on
/// stdout.
///
/// # Errors
/// [`CellError::Failed`] when a check fails, [`CellError::Broken`] when the cell cannot
/// be set up or driven.
pub fn run(args: &M2Args) -> Result<(), CellError> {
    let mut cell = Cell::new(args)?;
    // The server and the builds are stopped when `steps` returns; the units here.
    let result = cell.steps();
    cell.stop();
    if result.is_err() {
        print_tails(&cell.logs, TAIL);
    }
    result
}

/// Whether the 64-bit little-endian ELF file `bytes` has a `PT_INTERP` program header:
/// a dynamically linked program. `None` when `bytes` is not such a file, or its
/// program headers are cut off.
#[must_use]
pub fn has_interpreter(bytes: &[u8]) -> Option<bool> {
    let elf64le = bytes.get(..6) == Some(b"\x7fELF\x02\x01".as_slice());
    let offset = usize::try_from(field(bytes, 0x20, 8)?).ok()?;
    let size = usize::try_from(field(bytes, 0x36, 2)?).ok()?;
    let count = usize::try_from(field(bytes, 0x38, 2)?).ok()?;
    let mut interp = false;
    for i in 0..count {
        let at = size.checked_mul(i)?.checked_add(offset)?;
        interp |= field(bytes, at, 4)? == PT_INTERP;
    }
    elf64le.then_some(interp)
}

/// The little-endian number of `n` bytes at `at`.
fn field(bytes: &[u8], at: usize, n: usize) -> Option<u64> {
    let bytes = bytes.get(at..at.checked_add(n)?)?;
    Some(bytes.iter().rev().fold(0, |v, &b| (v << 8) | u64::from(b)))
}

/// What one run works with. Paths are absolute: buck2 runs in the copy.
struct Cell {
    bin_dir: PathBuf,
    source: PathBuf,
    work: PathBuf,
    logs: PathBuf,
    leases: PathBuf,
    project: PathBuf,
    buck2: PathBuf,
    build_timeout: Duration,
    pause: Duration,
    restart_pause: Duration,
    cgroup_root: PathBuf,
    uid: u32,
    gid: u32,
    /// The units started, stopped at the end.
    units: Vec<String>,
}

impl Cell {
    fn new(args: &M2Args) -> Result<Self, CellError> {
        let uid = rustix::process::getuid().as_raw();
        let work = absolute(&args.work)?;
        let logs = work.join("logs");
        let data = match &args.data_dir {
            Some(dir) => absolute(dir)?,
            None => std::env::temp_dir().join(format!("kbf-m2-data-{uid}-{}", std::process::id())),
        };
        let leases = data.join("leases");
        for dir in [&logs, &leases] {
            std::fs::create_dir_all(dir).or_broken(&dir.display())?;
        }
        let daemon_log = logs.join("daemon.err");
        std::fs::write(&daemon_log, b"").or_broken(&daemon_log.display())?;
        Ok(Self {
            bin_dir: absolute(&args.bin_dir)?,
            source: absolute(&args.project)?,
            project: work.join("project"),
            work,
            logs,
            leases,
            buck2: absolute(&args.buck2)?,
            build_timeout: Duration::from_secs(args.build_timeout),
            pause: Duration::from_secs(args.pause_secs),
            restart_pause: Duration::from_secs(args.restart_pause_secs),
            cgroup_root: args.cgroup_root.clone(),
            uid,
            gid: rustix::process::getgid().as_raw(),
            units: Vec::new(),
        })
    }

    /// Everything after setup. The processes it starts are stopped as it returns.
    fn steps(&mut self) -> Result<(), CellError> {
        self.setup_runner()?;
        self.copy_project()?;
        let pki = crate::pki::write(&self.work.join("pki"), NODE_ID);
        let pki = pki.or_broken(&"the certificates")?;
        let _server = self.start_server(&pki)?;
        self.start_daemon(UNIT, &pki)?;
        self.image_unpacked()?;
        let cgroup = self.cgroup_layout(UNIT)?;
        self.two_builds()?;
        self.hog()?;
        self.lease_in_container(&cgroup)?;
        self.nothing_left()?;
        self.restart_mid_lease(&pki)?;
        self.nothing_left()
    }

    /// What the log should show of the tools and the host.
    fn setup_runner(&self) -> Result<(), CellError> {
        print!(
            "{}",
            output(Command::new(CRUN).arg("--version"), "crun --version")?
        );
        let version = status(Command::new(&self.buck2).arg("--version"))?;
        ensure(version, "buck2 --version failed".to_owned())?;
        println!("{}", host_report(Path::new("/")));
        Ok(())
    }

    /// Copies the sample project to `<work>/project`, with `kbf-m2-act` as its
    /// `tools/kbf-m2-act`, executable, and requires that copy to be linked statically.
    fn copy_project(&self) -> Result<(), CellError> {
        if self.project.exists() {
            let why = format!("{} exists; give a fresh --work", self.project.display());
            return Err(CellError::Broken(why));
        }
        copy_tree(&self.source, &self.project)?;
        let tools = self.project.join("tools");
        std::fs::create_dir_all(&tools).or_broken(&tools.display())?;
        let act = tools.join("kbf-m2-act");
        std::fs::copy(self.bin_dir.join("kbf-m2-act"), &act).or_broken(&act.display())?;
        let executable = std::fs::Permissions::from_mode(0o755);
        std::fs::set_permissions(&act, executable).or_broken(&act.display())?;
        let interp = has_interpreter(&std::fs::read(&act).or_broken(&act.display())?);
        let why = format!("{} is not a 64-bit little-endian ELF file", act.display());
        let interp = interp.ok_or(CellError::Broken(why))?;
        let why = "kbf-m2-act is dynamically linked; the image may not hold its libraries";
        ensure(!interp, why.to_owned())
    }

    fn start_server(&self, pki: &PkiFiles) -> Result<Process, CellError> {
        let mut server = Command::new(self.bin_dir.join("kbf-server"));
        server
            .args(["--listen", REAPI, "--worker-listen", WORKER])
            .arg("--cold-store")
            .arg(format!(
                "file://{}",
                self.work.join("server-blobs").display()
            ))
            .arg("--meta-dir")
            .arg(self.work.join("server-meta"))
            .arg("--worker-tls-cert")
            .arg(&pki.server_cert)
            .arg("--worker-tls-key")
            .arg(&pki.server_key)
            .arg("--worker-client-ca")
            .arg(&pki.ca_cert)
            // The hog touches 3 GiB: a first booking of the default 4 GiB would pass
            // its cap, so the server starts actions at 1 GiB and climbs 2 and 4.
            .args(["--memory-ladder-gib", "1,2,4"])
            .stdout(self.log_file("server.out")?)
            .stderr(self.log_file("server.err")?);
        let server = Process::spawn(server, "kbf-server")?;
        let out = self.logs.join("server.out");
        let line = wait_for_line(&out, " reapi=", "kbf-server did not start")?;
        println!("{line}");
        Ok(server)
    }

    /// Starts the daemon as the system unit `unit` and waits until the server welcomes
    /// it (one more `welcomed` in its log than before).
    fn start_daemon(&mut self, unit: &str, pki: &PkiFiles) -> Result<(), CellError> {
        let log = self.logs.join("daemon.err");
        let before = welcomed(&read(&log)?);
        let mut start = Command::new(SUDO);
        start.arg("-n").args(self.unit_args(unit, pki));
        let started = status(&mut start)?;
        self.units.push(unit.to_owned());
        ensure(started, format!("sudo -n systemd-run --unit={unit} failed"))?;
        let deadline = Instant::now() + JOIN_WAIT;
        loop {
            let text = plain(&read(&log)?);
            let welcome = welcomed(&text) > before;
            let late = Instant::now() >= deadline;
            if welcome | late {
                let last = text.lines().next_back().unwrap_or_default();
                let why = format!("the daemon did not join; the last line of its log: {last}");
                ensure(welcome, why)?;
                println!("daemon joined ({unit})");
                return Ok(());
            }
            std::thread::sleep(POLL);
        }
    }

    /// The `systemd-run` arguments that start the daemon as `unit`: in
    /// `kbf-daemon.slice` with `Delegate=yes` and the daemon's `OOMScoreAdjust=-900`,
    /// as this user, its stderr appended to `daemon.err`.
    fn unit_args(&self, unit: &str, pki: &PkiFiles) -> Vec<OsString> {
        let worker = format!("https://{WORKER}");
        let mut args: Vec<OsString> = vec![
            "systemd-run".into(),
            "--quiet".into(),
            "--collect".into(),
            format!("--unit={unit}").into(),
            "--slice=kbf-daemon.slice".into(),
            "-p".into(),
            "Delegate=yes".into(),
            "-p".into(),
            "OOMScoreAdjust=-900".into(),
            "-p".into(),
            joined("StandardError=append:", &self.logs.join("daemon.err")),
            format!("--uid={}", self.uid).into(),
            format!("--gid={}", self.gid).into(),
            joined("--working-directory=", &self.work),
        ];
        let home = std::env::var_os("HOME").unwrap_or_default();
        let path = std::env::var_os("PATH").unwrap_or_default();
        for env in [
            joined("HOME=", Path::new(&home)),
            joined("PATH=", Path::new(&path)),
        ] {
            args.extend(["-E".into(), env]);
        }
        args.push(self.bin_dir.join("kbf-daemon").into());
        args.extend(
            [
                "--driver",
                "container",
                "--server",
                worker.as_str(),
                "--cas",
                worker.as_str(),
            ]
            .map(OsString::from),
        );
        for (flag, file) in [
            ("--ca-cert", &pki.ca_cert),
            ("--cert", &pki.client_cert),
            ("--key", &pki.client_key),
        ] {
            args.extend([flag.into(), file.into()]);
        }
        args.extend(["--tls-server-name", "localhost", "--node-id", NODE_ID].map(OsString::from));
        // The leases are `<data dir>/leases`.
        let data = self.leases.parent().expect("a data directory");
        args.extend(["--data-dir".into(), data.into()]);
        args.extend(["--actions-memory-max-gib", "8"].map(OsString::from));
        // The image, fetched and unpacked before the daemon says `Hello`.
        args.extend(["--image".into(), format!("docker://{IMAGE}").into()]);
        args
    }

    /// The image the daemon was given is in its store, unpacked, under its digest.
    fn image_unpacked(&self) -> Result<(), CellError> {
        let hex = IMAGE.rsplit_once("@sha256:").map_or("", |(_, hex)| hex);
        let rootfs = self.leases.join("oci-images").join(hex).join("rootfs");
        let why = format!("the daemon joined, but {} is not there", rootfs.display());
        ensure(rootfs.is_dir(), why)?;
        println!("OK: the daemon fetched {IMAGE} and unpacked it");
        Ok(())
    }

    /// Checks the cgroup the daemon set up in its unit `unit`; returns its directory.
    fn cgroup_layout(&self, unit: &str) -> Result<PathBuf, CellError> {
        let cgroup = self.show(unit, "ControlGroup")?;
        let pid = self.show(unit, "MainPID")?;
        let dir = self.cgroup_root.join(cgroup.trim_start_matches('/'));
        let procs = read_or_empty(&dir.join("supervisor/cgroup.procs"));
        let held = procs.split_whitespace().any(|p| p == pid);
        let why = format!(
            "{cgroup}/supervisor does not hold the daemon (pid {pid}): its unit needs \
             Delegate=yes, so that the daemon can make its own layout"
        );
        ensure(held, why)?;
        for (at, name) in [
            (dir.clone(), cgroup.clone()),
            (dir.join("actions"), format!("{cgroup}/actions")),
        ] {
            let enabled = read_or_empty(&at.join("cgroup.subtree_control"));
            let on: Vec<&str> = enabled.split_whitespace().collect();
            let missing: Vec<&str> = CONTROLLERS
                .into_iter()
                .filter(|c| !on.contains(c))
                .collect();
            let why = format!(
                "{name} does not enable {} for its children (it enables {:?})",
                missing.join(" "),
                enabled.trim()
            );
            ensure(missing.is_empty(), why)?;
        }
        println!(
            "OK: the daemon's cgroup {cgroup} holds supervisor/ (pid {pid}), and it and \
             actions/ enable cpu memory pids"
        );
        Ok(dir)
    }

    /// `systemctl show -P <property> <unit>`, trimmed.
    fn show(&self, unit: &str, property: &str) -> Result<String, CellError> {
        let mut show = Command::new(SYSTEMCTL);
        show.args(["show", "-P", property, unit]);
        Ok(output(&mut show, "systemctl show")?.trim().to_owned())
    }

    /// Builds `//...`, cleans, builds again, and applies the M1 rule to the two builds.
    fn two_builds(&self) -> Result<(), CellError> {
        let first = self.build("first")?;
        let cleaned = status(self.buck2().arg("clean"))?;
        ensure(cleaned, "buck2 clean failed".to_owned())?;
        let second = self.build("second")?;
        let (report, passed) = check_logs(Tool::Buck2, &first, &second);
        print!("{report}");
        let why = "the two buck2 builds break the M1 rule (above)";
        ensure(passed, why.to_owned())
    }

    /// One build of `//...`, its console in `buck2-<round>.log` (and printed) and its
    /// event log beside it (`--event-log`, gzipped JSON lines, the input of
    /// kbf-baseline); returns the console.
    fn build(&self, round: &str) -> Result<String, CellError> {
        let log = self.logs.join(format!("buck2-{round}.log"));
        let events = self.logs.join(format!("buck2-{round}.json-lines.gz"));
        let mut command = self.buck2();
        command
            .args(["build", "--console", "simple", "--event-log"])
            .arg(events)
            .arg("//...");
        to_log(&mut command, &log)?;
        let mut build = Process::spawn(command, "a buck2 build")?;
        let ended = build.wait_within(self.build_timeout)?;
        let text = read(&log)?;
        print!("{text}");
        let secs = self.build_timeout.as_secs();
        let why = format!("the {round} buck2 build of //... failed (or ran past {secs} s)");
        ensure(ended.as_ref().is_some_and(ExitStatus::success), why)?;
        Ok(text)
    }

    /// The hog's doubling: one requeue from 1 GiB to 2 GiB, then its output.
    fn hog(&self) -> Result<(), CellError> {
        let server = plain(&read(&self.logs.join("server.err"))?);
        let requeues = server.matches(HOG_REQUEUE).count();
        let why =
            format!("the server requeued an action from 1 GiB to 2 GiB {requeues} times, not once");
        ensure(requeues == 1, why)?;
        let mut command = self.buck2();
        command.args([
            "build",
            "--console",
            "none",
            "--show-full-simple-output",
            "//:hog",
        ]);
        let out = output(&mut command, "buck2 build //:hog")?;
        let path = out.trim();
        let held = read_or_empty(Path::new(path));
        let why = format!("//:hog's output {path} is not 3072");
        ensure(held.trim_end_matches('\n') == "3072", why)?;
        println!(
            "OK: //:hog was killed at its own 2 GiB cap, run again with 2 GiB booked, and passed"
        );
        Ok(())
    }

    /// `//:pause` holds a lease in one running container, under `actions/` of the
    /// daemon's cgroup `cgroup`, for at least its sleep.
    fn lease_in_container(&self, cgroup: &Path) -> Result<(), CellError> {
        let daemon_log = self.logs.join("daemon.err");
        let before = leases_started(&read(&daemon_log)?).len();
        let start = Instant::now();
        let mut build = self.pause("buck2-pause.log", self.pause)?;
        build.lease(&daemon_log, before)?;
        let deadline = Instant::now() + LEASE_WAIT;
        let running = loop {
            let running = self.containers();
            let gave_up = !build.running()? | (Instant::now() >= deadline);
            if (running > 0) | gave_up {
                break running;
            }
            std::thread::sleep(POLL);
        };
        let why = format!("while //:pause ran, {running} crun containers existed, not one");
        ensure(running == 1, why)?;
        let actions = cgroup.join("actions");
        let lease_cgroups = names(&actions, "kbf-lease-");
        let why = format!(
            "while //:pause ran, {} held no kbf-lease-* cgroup",
            actions.display()
        );
        ensure(!lease_cgroups.is_empty(), why)?;
        println!(
            "OK: //:pause holds a lease in one running container, in the cgroup actions/{}",
            lease_cgroups.join(" ")
        );
        let ended = build.wait_within(self.build_timeout)?;
        let elapsed = start.elapsed();
        let text = read(&self.logs.join("buck2-pause.log"))?;
        let why = format!("//:pause failed (or ran past its timeout):\n{text}");
        ensure(ended.as_ref().is_some_and(ExitStatus::success), why)?;
        let (ms, secs) = (elapsed.as_millis(), self.pause.as_secs());
        let why = format!("//:pause took {ms} ms, under its {secs} s sleep");
        ensure(elapsed >= self.pause, why)?;
        println!("OK: //:pause took {ms} ms");
        Ok(())
    }

    /// Nothing left behind (a few seconds allowed for the clean step to end) but the
    /// daemon's claim on the scratch directory.
    fn nothing_left(&self) -> Result<(), CellError> {
        let deadline = Instant::now() + LEFT_WAIT;
        let (left, dirs) = loop {
            let left = self.containers();
            let dirs = names(&self.leases, LEASE_PREFIX);
            let clean = (left == 0) & dirs.is_empty();
            if clean | (Instant::now() >= deadline) {
                break (left, dirs);
            }
            std::thread::sleep(POLL);
        };
        let why = format!("{left} crun containers are left");
        ensure(left == 0, why)?;
        let why = format!("lease scratch directories are left: {}", dirs.join(" "));
        ensure(dirs.is_empty(), why)?;
        println!("OK: no container and no lease directory left");
        Ok(())
    }

    /// The daemon killed mid-lease, and its lease run again at once.
    fn restart_mid_lease(&mut self, pki: &PkiFiles) -> Result<(), CellError> {
        let daemon_log = self.logs.join("daemon.err");
        let before = leases_started(&read(&daemon_log)?).len();
        let mut build = self.pause("buck2-restart.log", self.restart_pause)?;
        build.lease(&daemon_log, before)?;
        let mut kill = Command::new(SUDO);
        kill.args([
            "-n",
            SYSTEMCTL,
            "kill",
            "--kill-whom=main",
            "-s",
            "SIGKILL",
            UNIT,
        ]);
        ensure(
            status(&mut kill)?,
            format!("sudo -n systemctl kill {UNIT} failed"),
        )?;
        println!("killed the daemon mid-lease");
        self.start_daemon(&format!("{UNIT}-2"), pki)?;
        let welcomed = Instant::now();
        let server_log = self.logs.join("server.err");
        let restarts = loop {
            let restarts = plain(&read(&server_log)?).matches(RESTARTED).count();
            if (restarts > 0) | (welcomed.elapsed() >= REQUEUE_WAIT) {
                break restarts;
            }
            std::thread::sleep(POLL);
        };
        let after = welcomed.elapsed();
        let why = format!("the server logged {restarts} restart requeues, not one");
        ensure(restarts == 1, why)?;
        let ms = after.as_millis();
        let why = format!("the lease was requeued {ms} ms after the welcome");
        ensure(after < REQUEUE_WITHIN, why)?;
        println!("OK: the killed daemon's lease was requeued within {ms} ms");
        let ended = build.wait_within(self.build_timeout)?;
        let text = read(&self.logs.join("buck2-restart.log"))?;
        print!("{text}");
        let why = format!("//:pause failed after the restart (or ran past its timeout):\n{text}");
        ensure(ended.as_ref().is_some_and(ExitStatus::success), why)?;
        let spawns = parse(Tool::Buck2, &text).unwrap_or_default();
        let farm = (spawns.remote > 0) & spawns.other.is_empty();
        let why = "the build after the restart ran a command locally, or none remotely";
        ensure(farm, why.to_owned())?;
        println!("OK: //:pause succeeded on the farm after its daemon was killed");
        Ok(())
    }

    /// Starts `//:pause` sleeping `sleep`, under a fresh salt, its console in `log`.
    fn pause(&self, log: &str, sleep: Duration) -> Result<Process, CellError> {
        let salt = random_hex(8)?;
        let mut command = self.buck2();
        command
            .args(["build", "--console", "simple"])
            .args(["-c", &format!("kbf_m2.sleep_secs={}", sleep.as_secs())])
            .args(["-c", &format!("kbf_m2.salt={salt}"), "//:pause"]);
        to_log(&mut command, &self.logs.join(log))?;
        Process::spawn(command, "the //:pause build")
    }

    /// How many `crun` containers the daemon's leases hold: `crun` keeps a state
    /// directory per container in the `--root` the driver gives it, `<lease>/crun`, and
    /// removes it when the container ends (`crun run` deletes what it made).
    fn containers(&self) -> usize {
        names(&self.leases, LEASE_PREFIX)
            .iter()
            .map(|lease| names(&self.leases.join(lease).join("crun"), "").len())
            .sum()
    }

    /// buck2, in the project's copy.
    fn buck2(&self) -> Command {
        let mut buck2 = Command::new(&self.buck2);
        buck2.current_dir(&self.project);
        buck2
    }

    fn log_file(&self, name: &str) -> Result<std::fs::File, CellError> {
        let path = self.logs.join(name);
        std::fs::File::create(&path).or_broken(&path.display())
    }

    /// Stops what the run started: buck2's daemon and the daemon's units. Errors are
    /// left: a unit killed on purpose is gone already, and the project may not exist.
    fn stop(&self) {
        let _ = status(self.buck2().arg("kill"));
        for unit in &self.units {
            let _ = status(Command::new(SUDO).args(["-n", SYSTEMCTL, "stop", unit]));
        }
    }
}

/// How many times the server welcomed a daemon, by `log` (plain).
fn welcomed(log: &str) -> usize {
    log.matches("welcomed").count()
}

/// `<prefix><path>`, for a path that need not be UTF-8.
fn joined(prefix: &str, path: &Path) -> OsString {
    let mut out = OsString::from(prefix);
    out.push(path);
    out
}

/// Runs `command` (`what`, for messages): its stdout, when it exited 0. Its stderr is
/// this process's.
fn output(command: &mut Command, what: &str) -> Result<String, CellError> {
    let out = command.stderr(Stdio::inherit()).output();
    let out = out.or_broken(&format!("run {what}"))?;
    let why = format!("{what} failed: {}", out.status);
    ensure(out.status.success(), why)?;
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The file at `path` as text, or empty when it cannot be read: a cgroup file the
/// daemon did not make is a failed check, not a broken run.
fn read_or_empty(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

/// The names in the directory `dir` that start with `prefix`, sorted; none when it
/// cannot be read.
fn names(dir: &Path, prefix: &str) -> Vec<String> {
    let entries = std::fs::read_dir(dir).into_iter().flatten().flatten();
    let all = entries.map(|e| e.file_name().to_string_lossy().into_owned());
    let mut names: Vec<String> = all.filter(|n| n.starts_with(prefix)).collect();
    names.sort();
    names
}
