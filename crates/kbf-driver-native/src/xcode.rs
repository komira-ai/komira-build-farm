//! The Xcodes on this Mac, and the one an action names.
//!
//! A Mac may have several Xcodes installed side by side (`/Applications/Xcode.app`,
//! `/Applications/Xcode_16.2.app`, ...). [`discover`] finds every `Xcode*.app` in a
//! directory and asks each, under that Xcode's `DEVELOPER_DIR`, for its build
//! (`xcodebuild -version`), whether its licence is accepted (`xcodebuild -license
//! check`) and whether its compiler can be found (`xcrun --find clang`). An Xcode whose
//! licence is not accepted still answers `-version` with exit 0, while the other two
//! (and `cc`, `swiftc`, `xcrun` in every action) exit 69; so an Xcode is kept only when
//! all three exit 0, each within [`ANSWER_WITHIN`], and one that does not is left out
//! and logged with the question it failed and why. The driver reports one `xcode`
//! entry per build ([`CAPABILITY`]), which `kbf-caps` matches by membership, so an
//! action that names a build runs on any Mac that has it.
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

/// Where macOS keeps `xcodebuild` (a shim that runs the `DEVELOPER_DIR` Xcode's).
pub const XCODEBUILD: &str = "/usr/bin/xcodebuild";

/// Where macOS keeps `xcrun` (a shim that finds tools in the `DEVELOPER_DIR` Xcode).
pub const XCRUN: &str = "/usr/bin/xcrun";

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

/// How long an Xcode has to answer each question at daemon start. A first
/// run after an install can take some seconds; one that takes longer is hung (waiting
/// on a licence prompt or a broken install), and is left out rather than holding the
/// node out of the farm.
pub const ANSWER_WITHIN: Duration = Duration::from_secs(60);

/// Every Xcode in `apps` that answers, by build, as its `DEVELOPER_DIR`: the app's real
/// path (links resolved) joined with `Contents/Developer`. An Xcode answers when, with
/// that `DEVELOPER_DIR`, `xcodebuild -version` (`xcodebuild` is the program run) prints
/// a build and it, `xcodebuild -license check` and `xcrun --find clang` (`xcrun`) each
/// exit 0 within `within`; they are asked in that order and the first that fails
/// decides. Apps are tried in name order; of two with one build (a link `Xcode.app` to
/// `Xcode_16.2.app`), the first is kept. A directory that cannot be read has none. An
/// Xcode that does not answer is left out and logged with the question and why. A
/// question not answered in time (exit and close its output) is killed if still
/// running, so each Xcode delays the return by up to `within` per question it is asked.
#[must_use]
pub fn discover(
    apps: &Path,
    xcodebuild: &Path,
    xcrun: &Path,
    within: Duration,
) -> BTreeMap<String, PathBuf> {
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
        match developer_dir_of(&app, xcodebuild, xcrun, within) {
            Ok((build, dir)) => {
                tracing::info!(app = %app.display(), build, "Xcode");
                found.entry(build).or_insert(dir);
            }
            Err(why) => tracing::warn!(app = %app.display(), "Xcode left out: {why}"),
        }
    }
    found
}

/// The build and `DEVELOPER_DIR` of the Xcode at `app`, or why it is left out.
fn developer_dir_of(
    app: &Path,
    xcodebuild: &Path,
    xcrun: &Path,
    within: Duration,
) -> Result<(String, PathBuf), String> {
    let dir = std::fs::canonicalize(app)
        .map_err(|e| e.to_string())?
        .join("Contents")
        .join("Developer");
    let version = answer(xcodebuild, &["-version"], &dir, within)?;
    let build = build_of(&version)
        .ok_or_else(|| format!("no build in xcodebuild -version: {:?}", version.trim()))?
        .to_owned();
    answer(xcodebuild, &["-license", "check"], &dir, within)?;
    answer(xcrun, &["--find", "clang"], &dir, within)?;
    Ok((build, dir))
}

/// What `program args` prints to stdout, run with `DEVELOPER_DIR` set to
/// `developer_dir`, if it exits 0 within `within`; otherwise what was asked and why it
/// did not answer (with its stderr when it exited non-zero).
fn answer(
    program: &Path,
    args: &[&str],
    developer_dir: &Path,
    within: Duration,
) -> Result<String, String> {
    let asked = format!("{} {}", program.display(), args.join(" "));
    let mut command = std::process::Command::new(program);
    command.args(args).env(DEVELOPER_DIR, developer_dir);
    let out = output_within(command, within).map_err(|e| format!("{asked}: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "{asked} exited with {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
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

    /// Writes an executable shell script `name` in `dir`.
    fn fake(dir: &Path, name: &str, script: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, script).expect("script");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        path
    }

    /// The licence prompt's refusal, as a real Xcode prints it.
    const NOT_AGREED: &str = "You have not agreed to the Xcode license agreements.";

    /// A stand-in for `xcodebuild`, answering `-version` and `-license check` only (any
    /// other question exits 64) and only for an app's `DEVELOPER_DIR` (none exits 70).
    /// `-version` prints a build and exits 0 for every app, as a real Xcode does whether
    /// or not its licence is accepted (so only the name filter keeps `Safari.app` out),
    /// except that it prints none for `mute` and prints one and then hangs for `hung` (so
    /// only the timeout keeps it out). `-license check` exits 69 for `broken`, whose
    /// licence is not accepted, and 0 for the rest.
    fn fake_xcodebuild(dir: &Path) -> PathBuf {
        fake(
            dir,
            "xcodebuild",
            &format!(
                "#!/bin/sh\n\
                licence=0\n\
                case \"$DEVELOPER_DIR\" in\n\
                */Xcode_good.app/Contents/Developer) build=16C5032a ;;\n\
                */Xcode_twin.app/Contents/Developer) build=16C5032a ;;\n\
                */Xcode_new.app/Contents/Developer) build=16E140 ;;\n\
                */Xcode_noclang.app/Contents/Developer) build=16F6 ;;\n\
                */Xcode_broken.app/Contents/Developer) build=16B40; licence=69 ;;\n\
                */Xcode_mute.app/Contents/Developer) build= ;;\n\
                */Xcode_hung.app/Contents/Developer) echo 'Build version 16A242d'; exec sleep 60 ;;\n\
                */Contents/Developer) build=99Z999 ;;\n\
                *) echo 'no DEVELOPER_DIR' >&2; exit 70 ;;\n\
                esac\n\
                case \"$*\" in\n\
                -version) echo 'Xcode 16'; [ -z \"$build\" ] || echo \"Build version $build\" ;;\n\
                '-license check') [ \"$licence\" = 0 ] || {{ echo '{NOT_AGREED}' >&2; exit \"$licence\"; }} ;;\n\
                *) echo \"unknown: $*\" >&2; exit 64 ;;\n\
                esac\n"
            ),
        )
    }

    /// A stand-in for `xcrun`, answering `--find clang` only (any other question exits
    /// 64) and only for an app's `DEVELOPER_DIR` (none exits 70): it exits 69 for
    /// `broken`, as every tool of an Xcode whose licence is not accepted does, 72 for
    /// `noclang` (licence accepted, compiler missing: only this question keeps it out),
    /// and prints the compiler's path for the rest.
    fn fake_xcrun(dir: &Path) -> PathBuf {
        fake(
            dir,
            "xcrun",
            &format!(
                "#!/bin/sh\n\
                [ \"$*\" = '--find clang' ] || {{ echo \"unknown: $*\" >&2; exit 64; }}\n\
                case \"$DEVELOPER_DIR\" in\n\
                */Xcode_broken.app/Contents/Developer) echo '{NOT_AGREED}' >&2; exit 69 ;;\n\
                */Xcode_noclang.app/Contents/Developer) echo 'xcrun: error: unable to find utility \"clang\"' >&2; exit 72 ;;\n\
                */Contents/Developer) echo \"$DEVELOPER_DIR/usr/bin/clang\" ;;\n\
                *) echo 'no DEVELOPER_DIR' >&2; exit 70 ;;\n\
                esac\n"
            ),
        )
    }

    /// How long the fake Xcodes have to answer.
    const WITHIN: Duration = Duration::from_secs(2);

    /// Catches: an Xcode left out that answers, one kept that does not answer, has no
    /// build, has its licence not accepted (`-version` still exits 0, issue #164), has
    /// no compiler, hangs, or is a dangling link, another app taken for an Xcode, a
    /// question asked without the Xcode's `DEVELOPER_DIR` or with other arguments, the
    /// `DEVELOPER_DIR` not the app's real `Contents/Developer`, a later twin replacing
    /// the first, a missing directory or program treated as anything but "no Xcode", a
    /// hung Xcode waited for past its time (its fake sleeps for a minute), and a log
    /// that does not say which question an Xcode failed and why.
    #[test]
    fn every_xcode_that_answers_is_found() {
        let dir = scratch("discover");
        let apps = dir.join("Applications");
        for app in [
            "Xcode_good.app",
            "Xcode_twin.app",
            "Xcode_new.app",
            "Xcode_broken.app",
            "Xcode_noclang.app",
            "Xcode_mute.app",
            "Xcode_hung.app",
            "Safari.app",
        ] {
            std::fs::create_dir_all(apps.join(app).join("Contents/Developer")).expect("app");
        }
        std::os::unix::fs::symlink(apps.join("Xcode_new.app"), apps.join("Xcode.app"))
            .expect("link");
        std::os::unix::fs::symlink(apps.join("nowhere"), apps.join("Xcode_gone.app"))
            .expect("dangling link");
        let xcodebuild = fake_xcodebuild(&dir);
        let xcrun = fake_xcrun(&dir);
        let real = std::fs::canonicalize(&apps).expect("real");
        let started = Instant::now();
        let found = discover(&apps, &xcodebuild, &xcrun, WITHIN);
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

        assert!(discover(&dir.join("missing"), &xcodebuild, &xcrun, WITHIN).is_empty());
        assert!(discover(&apps, &dir.join("no-xcodebuild"), &xcrun, WITHIN).is_empty());

        // What the log says of each one left out.
        let why = |app: &str, xcrun: &Path| {
            developer_dir_of(&apps.join(app), &xcodebuild, xcrun, WITHIN).expect_err("left out")
        };
        let (xcodebuild_at, xcrun_at) = (xcodebuild.display(), xcrun.display());
        assert_eq!(
            why("Xcode_broken.app", &xcrun),
            format!("{xcodebuild_at} -license check exited with exit status: 69: {NOT_AGREED}")
        );
        assert_eq!(
            why("Xcode_noclang.app", &xcrun),
            format!(
                "{xcrun_at} --find clang exited with exit status: 72: \
                 xcrun: error: unable to find utility \"clang\""
            )
        );
        assert_eq!(
            why("Xcode_mute.app", &xcrun),
            "no build in xcodebuild -version: \"Xcode 16\""
        );
        assert_eq!(
            why("Xcode_hung.app", &xcrun),
            format!("{xcodebuild_at} -version: no answer within 2s; killed")
        );
        let no_xcrun = dir.join("no-xcrun");
        assert_eq!(
            why("Xcode_good.app", &no_xcrun),
            format!(
                "{} --find clang: No such file or directory (os error 2)",
                no_xcrun.display()
            )
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
