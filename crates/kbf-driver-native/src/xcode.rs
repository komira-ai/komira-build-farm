//! The Xcodes on this Mac, and the one an action names.
//!
//! A Mac may have several Xcodes installed side by side (`/Applications/Xcode.app`,
//! `/Applications/Xcode_16.2.app`, ...). [`discover`] finds every `Xcode*.app` in a
//! directory and asks each for its build with its own `xcodebuild -version` (the one
//! inside the app, never the `/usr/bin` shim, which may find it through `xcrun`'s
//! cache, a file any lease can write: `crate::user_folders`) under that Xcode's
//! `DEVELOPER_DIR`; one that does not answer (not set up, licence not
//! accepted, or no answer within [`ANSWER_WITHIN`]) is left out and logged. The
//! driver reports one `xcode` entry per build ([`CAPABILITY`]), which `kbf-caps`
//! matches by membership, so an action that names a build runs on any Mac that has it.
//!
//! An action names its Xcode with the platform property `xcode` (the name in any case,
//! as the front reads it) and runs with `DEVELOPER_DIR` set to that Xcode, so `xcrun`,
//! `cc`, `swiftc` and `xcodebuild` are that Xcode's ([`developer_dir`]). The property
//! wins over a `DEVELOPER_DIR` in the Command's environment: the build the action names
//! is the one the scheduler matched and the one in its digest.

use std::collections::BTreeMap;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use kbf_daemon::RuntimeError;
use kbf_proto::reapi::{Action, Command, Platform};

/// The platform property, and the node report key, that names an Xcode build.
pub const CAPABILITY: &str = "xcode";

/// The variable that selects an Xcode for `xcrun` and the tools behind it.
pub const DEVELOPER_DIR: &str = "DEVELOPER_DIR";

/// Where an Xcode keeps its `xcodebuild`, inside its `DEVELOPER_DIR`. The daemon runs
/// it there, never through the `/usr/bin/xcodebuild` shim: the shim may look the tool
/// up in `xcrun`'s cache, which leases can rewrite (`crate::user_folders`), and the
/// daemon runs it outside the sandbox.
pub const XCODEBUILD: &str = "usr/bin/xcodebuild";

/// The directory Xcodes are installed in.
pub const APPLICATIONS: &str = "/Applications";

/// The build in `xcodebuild -version` output (`Build version 16C5032a`), if any.
#[must_use]
pub fn build_of(version: &str) -> Option<&str> {
    version
        .lines()
        .find_map(|line| line.trim().strip_prefix("Build version "))
        .map(str::trim)
        .filter(|build| !build.contains(char::is_whitespace))
}

/// How long one Xcode has to answer `xcodebuild -version` at daemon start. A first
/// run after an install can take some seconds; one that takes longer is hung (waiting
/// on a licence prompt or a broken install), and is left out rather than holding the
/// node out of the farm.
pub const ANSWER_WITHIN: Duration = Duration::from_secs(60);

/// Every Xcode in `apps` that answers `xcodebuild -version` within `within` each
/// (`xcodebuild` is the program's path inside the Xcode's `DEVELOPER_DIR`, as
/// [`XCODEBUILD`]; a path that leads out of it, an absolute one included, is never
/// run), by build, as its `DEVELOPER_DIR`: the app's real
/// path (links resolved) joined with `Contents/Developer`. Apps are tried in name
/// order; of two with one build (a link `Xcode.app` to `Xcode_16.2.app`), the first is
/// kept. A directory that cannot be read has none. One that does not answer in time
/// (exit and close its output) is killed if still running, left out and logged; each
/// such Xcode delays the return by up to `within`.
#[must_use]
pub fn discover(apps: &Path, xcodebuild: &Path, within: Duration) -> BTreeMap<String, PathBuf> {
    let mut found = BTreeMap::new();
    let names = match std::fs::read_dir(apps) {
        Ok(entries) => {
            let mut names: Vec<_> = entries
                .filter_map(Result::ok)
                .map(|entry| entry.file_name())
                .filter(|name| {
                    name.to_str()
                        .is_some_and(|n| n.starts_with("Xcode") && n.ends_with(".app"))
                })
                .collect();
            names.sort();
            names
        }
        Err(e) => {
            tracing::info!(apps = %apps.display(), "no Xcode: {e}");
            return found;
        }
    };
    for name in names {
        let app = apps.join(name);
        match developer_dir_of(&app, xcodebuild, within) {
            Ok((build, dir)) => {
                tracing::info!(app = %app.display(), build, "Xcode");
                found.entry(build).or_insert(dir);
            }
            Err(why) => tracing::warn!(app = %app.display(), "Xcode left out: {why}"),
        }
    }
    found
}

/// The build and `DEVELOPER_DIR` of the Xcode at `app`.
fn developer_dir_of(
    app: &Path,
    xcodebuild: &Path,
    within: Duration,
) -> Result<(String, PathBuf), String> {
    let dir = std::fs::canonicalize(app)
        .map_err(|e| e.to_string())?
        .join("Contents")
        .join("Developer");
    if !xcodebuild
        .components()
        .all(|part| matches!(part, std::path::Component::Normal(_)))
    {
        return Err(format!("{} is not inside the Xcode", xcodebuild.display()));
    }
    let program = dir.join(xcodebuild);
    let mut command = std::process::Command::new(&program);
    command.arg("-version").env(DEVELOPER_DIR, &dir);
    let out = output_within(command, within)
        .map_err(|e| format!("{} -version: {e}", program.display()))?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    if !out.status.success() {
        return Err(format!(
            "xcodebuild -version exited with {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let build = build_of(&stdout)
        .ok_or_else(|| format!("no build in xcodebuild -version: {:?}", stdout.trim()))?;
    Ok((build.to_owned(), dir))
}

/// Where macOS keeps `xcrun`, behind every `/usr/bin` developer tool shim.
pub const XCRUN: &str = "/usr/bin/xcrun";

/// The tools [`warm`] has `xcrun` look up: the compilers and linker builds call most.
pub const WARM_TOOLS: [&str; 6] = ["cc", "clang", "clang++", "swift", "swiftc", "ld"];

/// On a thread of its own, has `xcrun` look up each of [`WARM_TOOLS`] for the node's
/// own Xcode (no `DEVELOPER_DIR`) and for each of `developer_dirs`, each lookup given
/// `within` to answer, so `xcrun`'s cache (`crate::user_folders`) holds them before
/// the first action asks: a lookup it has not cached takes seconds. A lookup that
/// fails is logged and the rest go on. `None`, and nothing run, where `xcrun` is not a
/// file (off macOS).
///
/// `xcrun` here is the `/usr/bin` one, outside the sandbox: the cache it fills is
/// that one's, and no Xcode carries its own. `--find` only prints a path, which is
/// dropped, and runs nothing it finds, so a cache a lease rewrote during the warm-up
/// misleads no program of the daemon's; the daemon also removes the cache before it
/// starts ([`crate::user_folders::UserFolders::forget_xcrun_cache`]).
#[must_use]
pub fn warm(
    xcrun: &Path,
    developer_dirs: Vec<PathBuf>,
    within: Duration,
) -> Option<std::thread::JoinHandle<()>> {
    if !xcrun.is_file() {
        return None;
    }
    let xcrun = xcrun.to_owned();
    Some(std::thread::spawn(move || {
        let dirs = std::iter::once(None).chain(developer_dirs.into_iter().map(Some));
        for dir in dirs {
            for tool in WARM_TOOLS {
                let failed = match output_within(lookup(&xcrun, dir.as_deref(), tool), within) {
                    Ok(out) if out.status.success() => continue,
                    Ok(out) => format!(
                        "{}: {}",
                        out.status,
                        String::from_utf8_lossy(&out.stderr).trim()
                    ),
                    Err(e) => e.to_string(),
                };
                tracing::info!(tool, developer_dir = ?dir, "xcrun --find: {failed}");
            }
        }
    }))
}

/// `xcrun --find <tool>` for the Xcode at `developer_dir`, or for the node's own Xcode
/// (the one `xcode-select` names) with `DEVELOPER_DIR` removed from what the daemon was
/// started with, which would otherwise pick the Xcode instead.
fn lookup(xcrun: &Path, developer_dir: Option<&Path>, tool: &str) -> std::process::Command {
    let mut command = std::process::Command::new(xcrun);
    command.args(["--find", tool]);
    match developer_dir {
        Some(dir) => command.env(DEVELOPER_DIR, dir),
        None => command.env_remove(DEVELOPER_DIR),
    };
    command
}

/// How often [`output_within`] looks whether its process has exited.
const POLL: Duration = Duration::from_millis(10);

/// What `command` prints and how it exits (stdin `/dev/null`), if it exits and closes
/// its output within `within`; otherwise this is a `TimedOut` error, and the process is
/// killed if it has not exited. Its stdout and stderr are read on two threads while it
/// runs, so a full pipe cannot stall it. A process it started and left holding the
/// pipes keeps those threads reading; they are waited for only until `within` is up
/// and are left behind after that (each ends when the pipe it reads closes).
fn output_within(mut command: std::process::Command, within: Duration) -> io::Result<Output> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let deadline = Instant::now() + within;
    let stdout = read_all(child.stdout.take().expect("stdout is piped"));
    let stderr = read_all(child.stderr.take().expect("stderr is piped"));
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            child.wait()?;
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("no answer within {within:?}; killed"),
            ));
        }
        std::thread::sleep(POLL);
    };
    let unclosed = || {
        io::Error::new(
            io::ErrorKind::TimedOut,
            format!("exited, but its output was still open after {within:?}"),
        )
    };
    let left = || deadline.saturating_duration_since(Instant::now());
    let [stdout, stderr] =
        [stdout, stderr].map(|pipe| pipe.recv_timeout(left()).map_err(|_| unclosed()));
    Ok(Output {
        status,
        stdout: stdout?,
        stderr: stderr?,
    })
}

/// A thread that reads `pipe` to its end and sends what it read.
fn read_all(mut pipe: impl Read + Send + 'static) -> Receiver<Vec<u8>> {
    let (send, receive) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = pipe.read_to_end(&mut bytes);
        // The receiver is gone once its wait timed out; then nobody wants the bytes.
        let _ = send.send(bytes);
    });
    receive
}

/// The `DEVELOPER_DIR` of the Xcode the action's platform names (the Action's
/// platform, or the Command's for clients older than REAPI 2.2), from `xcodes`; `None`
/// when it names none.
///
/// # Errors
/// [`RuntimeError::Failed`] when the named build is not one this node has: the
/// scheduler placed the action here because the node reported it, so the node, not the
/// action, is at fault, and the action is run elsewhere.
#[allow(deprecated)]
pub fn developer_dir(
    xcodes: &BTreeMap<String, PathBuf>,
    action: &Action,
    command: &Command,
) -> Result<Option<PathBuf>, RuntimeError> {
    let platform: Option<&Platform> = action
        .platform
        .as_ref()
        .filter(|p| !p.properties.is_empty())
        .or(command.platform.as_ref());
    let Some(build) = platform
        .into_iter()
        .flat_map(|p| &p.properties)
        .find(|p| p.name.eq_ignore_ascii_case(CAPABILITY))
        .map(|p| p.value.as_str())
    else {
        return Ok(None);
    };
    xcodes.get(build).cloned().map(Some).ok_or_else(|| {
        let have: Vec<&str> = xcodes.keys().map(String::as_str).collect();
        RuntimeError::Failed(format!(
            "the action names Xcode build {build:?}; this node has {have:?}"
        ))
    })
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use kbf_proto::reapi::platform::Property;

    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::current_exe()
            .expect("test binary")
            .parent()
            .expect("deps")
            .join("kbf-driver-native-unit")
            .join(format!("xcode-{name}-{}", std::process::id()));
        // Absent unless a run with this pid left it.
        let _ = kbf_outputs::remove_tree(&dir);
        std::fs::create_dir_all(&dir).expect("scratch");
        dir
    }

    /// Catches: a build read from the wrong line, kept with its spacing, or made up
    /// from output that carries none.
    #[test]
    fn the_build_is_read_from_xcodebuild_version() {
        assert_eq!(
            build_of("Xcode 16.2\nBuild version 16C5032a\n"),
            Some("16C5032a")
        );
        assert_eq!(build_of("  Build version 15F31d  \n"), Some("15F31d"));
        for none in [
            "",
            "Xcode 16.2\n",
            "Build version \n",
            "Build version a b\n",
        ] {
            assert_eq!(build_of(none), None, "{none:?}");
        }
    }

    /// A stand-in for `xcodebuild`, linked into each app in `apps` at [`XCODEBUILD`]:
    /// writes the path it was run by (`$0`) to the returned log, then prints a build
    /// for every `DEVELOPER_DIR` (so only the name filter keeps `Safari.app` out),
    /// prints one and then fails for `broken` (so only the exit status keeps it out),
    /// prints none for `mute`, and prints one and then hangs for `hung` (so only the
    /// timeout keeps it out).
    fn fake_xcodebuild(dir: &Path, apps: &[PathBuf]) -> PathBuf {
        let path = dir.join("xcodebuild");
        let log = dir.join("argv0");
        let script = format!(
            "#!/bin/sh\n\
            echo \"$0\" >> '{}'\n\
            case \"$DEVELOPER_DIR\" in\n\
            *Xcode_good.app/Contents/Developer) echo 'Xcode 16.2'; echo 'Build version 16C5032a' ;;\n\
            *Xcode_twin.app/Contents/Developer) echo 'Build version 16C5032a' ;;\n\
            *Xcode_new.app/Contents/Developer) echo 'Build version 16E140' ;;\n\
            *Xcode_broken.app/Contents/Developer) echo 'Build version 16B40'; echo 'licence not accepted' >&2; exit 69 ;;\n\
            *Xcode_mute.app/Contents/Developer) echo 'Xcode ?' ;;\n\
            *Xcode_hung.app/Contents/Developer) echo 'Build version 16A242d'; exec sleep 60 ;;\n\
            *) echo 'Build version 99Z999' ;;\n\
            esac\n",
            log.display()
        );
        std::fs::write(&path, script).expect("script");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        for app in apps {
            let at = app.join("Contents/Developer").join(XCODEBUILD);
            std::fs::create_dir_all(at.parent().expect("usr/bin")).expect("usr/bin");
            std::os::unix::fs::symlink(&path, at).expect("link");
        }
        log
    }

    /// Catches the node's own Xcode looked up with a `DEVELOPER_DIR` the daemon was
    /// started with (the warm-up then fills another Xcode's entries), and a named
    /// Xcode looked up with any other.
    #[test]
    fn a_lookup_sets_or_removes_developer_dir() {
        let xcrun = Path::new("/x/xcrun");
        let own = lookup(xcrun, None, "cc");
        assert_eq!(own.get_program(), xcrun);
        assert_eq!(own.get_args().collect::<Vec<_>>(), ["--find", "cc"]);
        assert_eq!(
            own.get_envs().collect::<Vec<_>>(),
            [(std::ffi::OsStr::new(DEVELOPER_DIR), None)]
        );
        let dir = Path::new("/A/Xcode.app/Contents/Developer");
        let named = lookup(xcrun, Some(dir), "swiftc");
        assert_eq!(named.get_args().collect::<Vec<_>>(), ["--find", "swiftc"]);
        assert_eq!(
            named.get_envs().collect::<Vec<_>>(),
            [(std::ffi::OsStr::new(DEVELOPER_DIR), Some(dir.as_os_str()))]
        );
    }

    /// Catches: a lookup missed for the node's own Xcode or for one of the others, the
    /// warm stopped by a lookup that fails or hangs, and `xcrun` run where there is
    /// none.
    #[test]
    fn warm_looks_up_every_tool_for_every_xcode() {
        let dir = scratch("warm");
        let log = dir.join("log");
        let xcrun = dir.join("xcrun");
        let script = format!(
            "#!/bin/sh\n\
             echo \"${{DEVELOPER_DIR-none}} $*\" >> {}\n\
             case \"$2\" in\n\
             ld) echo 'not found' >&2; exit 1 ;;\n\
             swift) case \"$DEVELOPER_DIR\" in /hung) exec sleep 60 ;; esac ;;\n\
             esac\n",
            log.display()
        );
        std::fs::write(&xcrun, script).expect("script");
        std::fs::set_permissions(&xcrun, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        let thread = warm(
            &xcrun,
            vec![PathBuf::from("/x1"), PathBuf::from("/hung")],
            Duration::from_millis(500),
        )
        .expect("a thread");
        thread.join().expect("warmed");
        let want: Vec<String> = ["none", "/x1", "/hung"]
            .iter()
            .flat_map(|dir| WARM_TOOLS.map(|tool| format!("{dir} --find {tool}")))
            .collect();
        assert_eq!(
            std::fs::read_to_string(&log)
                .expect("log")
                .lines()
                .collect::<Vec<_>>(),
            want
        );
        assert!(warm(&dir.join("missing"), Vec::new(), WITHIN).is_none());
    }

    /// How long the fake Xcodes have to answer.
    const WITHIN: Duration = Duration::from_secs(2);

    /// Catches: an Xcode left out that answers, one kept that does not answer, has no
    /// build, hangs, or is a dangling link, another app taken for an Xcode, the
    /// `DEVELOPER_DIR` not the app's real `Contents/Developer`, a later twin replacing
    /// the first, a missing directory or program treated as anything but "no Xcode",
    /// and a hung Xcode waited for past its time (its fake sleeps for a minute).
    /// Catches too `xcodebuild` run other than from inside each Xcode (the review of
    /// issue #163: the `/usr/bin` shim may find it through `xcrun`'s cache, which
    /// leases can write), and a program path that leads out of the Xcode run at all.
    #[test]
    fn every_xcode_that_answers_is_found() {
        let dir = scratch("discover");
        let apps = dir.join("Applications");
        let names = [
            "Xcode_good.app",
            "Xcode_twin.app",
            "Xcode_new.app",
            "Xcode_broken.app",
            "Xcode_mute.app",
            "Xcode_hung.app",
            "Safari.app",
        ];
        let made: Vec<PathBuf> = names.iter().map(|app| apps.join(app)).collect();
        let ran = fake_xcodebuild(&dir, &made);
        std::os::unix::fs::symlink(apps.join("Xcode_new.app"), apps.join("Xcode.app"))
            .expect("link");
        std::os::unix::fs::symlink(apps.join("nowhere"), apps.join("Xcode_gone.app"))
            .expect("dangling link");
        let xcodebuild = Path::new(XCODEBUILD);
        let real = std::fs::canonicalize(&apps).expect("real");
        let started = Instant::now();
        let found = discover(&apps, xcodebuild, WITHIN);
        let took = started.elapsed();
        assert!(
            took >= WITHIN,
            "the hung Xcode was not given its time: {took:?}"
        );
        assert!(
            took < WITHIN * 10,
            "the hung Xcode was waited for: {took:?}"
        );
        let want = BTreeMap::from([
            (
                "16C5032a".to_owned(),
                real.join("Xcode_good.app/Contents/Developer"),
            ),
            (
                "16E140".to_owned(),
                real.join("Xcode_new.app/Contents/Developer"),
            ),
        ]);
        assert_eq!(found, want);
        // Each Xcode's own, by its real path, in name order (`Xcode.app` is the new one).
        let inside = |app: &str| {
            real.join(app)
                .join("Contents/Developer")
                .join(XCODEBUILD)
                .display()
                .to_string()
        };
        let want_ran: Vec<String> = [
            "Xcode_new.app",
            "Xcode_broken.app",
            "Xcode_good.app",
            "Xcode_hung.app",
            "Xcode_mute.app",
            "Xcode_new.app",
            "Xcode_twin.app",
        ]
        .map(inside)
        .into();
        let read_ran = || -> Vec<String> {
            std::fs::read_to_string(&ran)
                .expect("ran")
                .lines()
                .map(str::to_owned)
                .collect()
        };
        assert_eq!(read_ran(), want_ran);

        assert!(discover(&dir.join("missing"), xcodebuild, WITHIN).is_empty());
        assert!(discover(&apps, Path::new("usr/bin/missing"), WITHIN).is_empty());
        // A program outside the Xcode (the fake itself, as a shim would be) is not run.
        let outside = dir.join("xcodebuild");
        assert!(discover(&apps, &outside, WITHIN).is_empty());
        let up = Path::new("../../../../xcodebuild");
        assert!(discover(&apps, up, WITHIN).is_empty());
        assert_eq!(read_ran(), want_ran);
        assert_eq!(
            developer_dir_of(&apps.join("Xcode_good.app"), &outside, WITHIN),
            Err(format!("{} is not inside the Xcode", outside.display()))
        );

        // What the log says of the hung one.
        let hung = developer_dir_of(&apps.join("Xcode_hung.app"), xcodebuild, WITHIN);
        assert_eq!(
            hung,
            Err(format!(
                "{} -version: no answer within 2s; killed",
                inside("Xcode_hung.app")
            ))
        );
        kbf_outputs::remove_tree(&dir).expect("clean");
    }

    /// Catches output read only after the process exits: a process that prints more
    /// than a pipe holds would block on the full pipe and be killed as hung.
    #[test]
    fn a_long_answer_is_read_whole() {
        let mut command = std::process::Command::new("/bin/sh");
        command.args([
            "-c",
            "head -c 300000 /dev/zero; head -c 200000 /dev/zero >&2",
        ]);
        let out = output_within(command, WITHIN).expect("answers");
        assert!(out.status.success());
        assert_eq!((out.stdout.len(), out.stderr.len()), (300_000, 200_000));
    }

    /// Catches a time limit that covers the process but not its output: an Xcode that
    /// exits but leaves a child holding stdout and stderr (here for 8 s, four times the
    /// limit) must be given up on when the limit is up, not when the child ends.
    #[test]
    fn output_held_open_past_the_limit_is_not_waited_for() {
        let mut command = std::process::Command::new("/bin/sh");
        command.args(["-c", "sleep 8 & echo 'Build version 1A1'"]);
        let started = Instant::now();
        let out = output_within(command, WITHIN);
        let took = started.elapsed();
        assert!(
            took < WITHIN * 2,
            "the held output was waited for: {took:?}"
        );
        let err = out.expect_err("the output is still open at the limit");
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        assert_eq!(
            err.to_string(),
            "exited, but its output was still open after 2s"
        );
    }

    fn platform(name: &str, value: &str) -> Option<Platform> {
        Some(Platform {
            properties: vec![Property {
                name: name.to_owned(),
                value: value.to_owned(),
            }],
        })
    }

    /// Catches: the named Xcode not selected, the name read in one spelling only (the
    /// front accepted `XCODE` and the scheduler matched it), the Command's deprecated
    /// platform ignored for an old client or preferred over the Action's, and a build
    /// the node lacks run with another Xcode instead of failed.
    #[test]
    #[allow(deprecated)]
    fn the_action_names_its_xcode() {
        let xcodes = BTreeMap::from([
            (
                "16C5032a".to_owned(),
                PathBuf::from("/A/Xcode_16.2.app/Contents/Developer"),
            ),
            (
                "16E140".to_owned(),
                PathBuf::from("/A/Xcode_16.3.app/Contents/Developer"),
            ),
        ]);
        let none = Command::default();
        let named = |name: &str, build: &str| Action {
            platform: platform(name, build),
            ..Action::default()
        };
        assert_eq!(
            developer_dir(&xcodes, &Action::default(), &none).ok(),
            Some(None)
        );
        assert_eq!(
            developer_dir(&xcodes, &named("network", "on"), &none).ok(),
            Some(None)
        );
        for name in ["xcode", "XCODE", "Xcode"] {
            assert_eq!(
                developer_dir(&xcodes, &named(name, "16E140"), &none).ok(),
                Some(Some(PathBuf::from("/A/Xcode_16.3.app/Contents/Developer"))),
                "{name}"
            );
        }
        let old_client = Command {
            platform: platform("xcode", "16C5032a"),
            ..Command::default()
        };
        assert_eq!(
            developer_dir(&xcodes, &Action::default(), &old_client).ok(),
            Some(Some(PathBuf::from("/A/Xcode_16.2.app/Contents/Developer")))
        );
        assert_eq!(
            developer_dir(&xcodes, &named("xcode", "16E140"), &old_client).ok(),
            Some(Some(PathBuf::from("/A/Xcode_16.3.app/Contents/Developer")))
        );
        let missing = developer_dir(&xcodes, &named("xcode", "15F31d"), &none);
        assert_eq!(
            missing.map_err(|e| e.to_string()),
            Err(
                "the action names Xcode build \"15F31d\"; this node has [\"16C5032a\", \"16E140\"]"
                    .to_owned()
            )
        );
    }
}
