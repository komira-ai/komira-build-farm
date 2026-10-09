//! Tests of [`super`]: what each Xcode is asked, what decides its state, the fix
//! named, and the Xcode an action names.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::os::unix::fs::PermissionsExt as _;

use kbf_proto::reapi::platform::Property;

use super::*;

pub(crate) fn scratch(name: &str) -> PathBuf {
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
pub(crate) fn fake(dir: &Path, name: &str, script: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, script).expect("script");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    path
}

/// The licence prompt's refusal, as a real Xcode prints it.
pub(crate) const NOT_AGREED: &str = "You have not agreed to the Xcode license agreements.";

/// What `-checkFirstLaunchStatus` prints when the first launch was not run.
const FIRST_LAUNCH: &str = "Install additional required components? Run -runFirstLaunch.";

/// A stand-in for `xcodebuild` (linked into each app by [`install`], where [`survey`]
/// runs it), which writes the path it was run by (`$0`) to `argv0` in `dir`, answering
/// `-version`, `-license check`,
/// `-checkFirstLaunchStatus` and `-showComponent MetalToolchain` only (any other
/// question exits 64, as a real one does for an option it does not know) and only for
/// an app's `DEVELOPER_DIR` (none exits 70). `-version` prints a build and exits 0 for
/// every app, as a real Xcode does whether or not its licence is accepted (so only the
/// name filter keeps `Safari.app` out), except that it prints none for `mute` and
/// prints one and then hangs for `hung` (so only the timeout keeps it out). The rest
/// answer 0, except: `-license check` exits 69 for `broken`, whose licence is not
/// accepted, and hangs for `slowlicence`; `-checkFirstLaunchStatus` exits 69 for
/// `firstlaunch`; `-showComponent` hangs for `slowmetal`,
/// says `Status: uninstalled` for `nometal` and is an unknown option (64) for `old` and
/// `oldnometal` (Xcodes before 26, whose Metal is bundled).
fn fake_xcodebuild(dir: &Path) -> PathBuf {
    fake(
        dir,
        "xcodebuild",
        &format!(
            "#!/bin/sh\n\
            echo \"$0\" >> '{argv0}'\n\
            licence=0 first=0 metal=installed\n\
            case \"$DEVELOPER_DIR\" in\n\
            */Xcode_good.app/Contents/Developer) build=16C5032a ;;\n\
            */Xcode_twin.app/Contents/Developer) build=16C5032a ;;\n\
            */Xcode_new.app/Contents/Developer) build=16E140 ;;\n\
            */Xcode_noclang.app/Contents/Developer) build=16F6 ;;\n\
            */Xcode_broken.app/Contents/Developer) build=16B40; licence=69 ;;\n\
            */Xcode_firstlaunch.app/Contents/Developer) build=16G1; first=69 ;;\n\
            */Xcode_nometal.app/Contents/Developer) build=26A1; metal=uninstalled ;;\n\
            */Xcode_old.app/Contents/Developer) build=15A1; metal= ;;\n\
            */Xcode_oldnometal.app/Contents/Developer) build=15B1; metal= ;;\n\
            */Xcode_slowlicence.app/Contents/Developer) build=16H1; licence=hang ;;\n\
            */Xcode_slowmetal.app/Contents/Developer) build=26B1; metal=hang ;;\n\
            */Xcode_mute.app/Contents/Developer) build= ;;\n\
            */Xcode_hung.app/Contents/Developer) echo 'Build version 16A242d'; exec sleep 60 ;;\n\
            */Contents/Developer) build=99Z999 ;;\n\
            *) echo 'no DEVELOPER_DIR' >&2; exit 70 ;;\n\
            esac\n\
            case \"$*\" in\n\
            -version) echo 'Xcode 16'; [ -z \"$build\" ] || echo \"Build version $build\" ;;\n\
            '-license check') [ \"$licence\" = hang ] && exec sleep 60\n\
              [ \"$licence\" = 0 ] || {{ echo '{NOT_AGREED}' >&2; exit \"$licence\"; }} ;;\n\
            -checkFirstLaunchStatus) [ \"$first\" = 0 ] || {{ echo '{FIRST_LAUNCH}' >&2; exit \"$first\"; }} ;;\n\
            '-showComponent MetalToolchain') [ \"$metal\" = hang ] && exec sleep 60\n\
              [ -n \"$metal\" ] || {{ echo 'invalid option' >&2; exit 64; }}\n\
              echo 'Build Version: 17C48'; echo \"Status: $metal\" ;;\n\
            *) echo \"unknown: $*\" >&2; exit 64 ;;\n\
            esac\n",
            argv0 = dir.join("argv0").display()
        ),
    )
}

/// A stand-in for `xcrun`, answering `--find clang` and `--find metal` only (any other
/// question exits 64, `--no-cache` included: the survey's lookups use the cache, under
/// the sandbox) and only for an app's `DEVELOPER_DIR` (none exits 70): it exits
/// 69 for `broken`, as every tool of an Xcode whose licence is not accepted does, 72
/// for `noclang`'s clang (licence accepted, compiler missing: only this question keeps
/// it out) and for `oldnometal`'s metal, and prints the tool's path for the rest.
fn fake_xcrun(dir: &Path) -> PathBuf {
    fake(
        dir,
        "xcrun",
        &format!(
            "#!/bin/sh\n\
            case \"$*\" in '--find clang'|'--find metal') ;; \
            *) echo \"unknown: $*\" >&2; exit 64 ;; esac\n\
            case \"$DEVELOPER_DIR:$2\" in\n\
            */Xcode_broken.app/Contents/Developer:*) echo '{NOT_AGREED}' >&2; exit 69 ;;\n\
            */Xcode_noclang.app/Contents/Developer:clang|*/Xcode_oldnometal.app/Contents/Developer:metal)\n\
              echo \"xcrun: error: unable to find utility \\\"$2\\\"\" >&2; exit 72 ;;\n\
            */Contents/Developer:*) echo \"$DEVELOPER_DIR/usr/bin/$2\" ;;\n\
            *) echo 'no DEVELOPER_DIR' >&2; exit 70 ;;\n\
            esac\n"
        ),
    )
}

/// Catches a warm-up lookup run outside the sandbox (the review of issue #163:
/// the `/usr/bin/xcrun` the warm-up runs reads a cache leases can write, while the
/// node already serves), with the network on, without the user-folder rules that
/// let `xcrun` write its cache, or with another lease directory or `TMPDIR`; the
/// node's own Xcode looked up with a `DEVELOPER_DIR` the daemon was started with
/// (the warm-up then fills another Xcode's entries), and a named Xcode looked up
/// with any other.
#[test]
fn a_lookup_runs_sandboxed_and_sets_or_removes_developer_dir() {
    let xcrun = Path::new("/x/xcrun");
    let sandbox = Sandbox {
        isolation: Isolation::Sandbox(PathBuf::from(crate::network::SANDBOX_EXEC)),
        dir: PathBuf::from("/s/lease-warm-up"),
        rules: "(allow file-write* (literal \"/c\"))\n".to_owned(),
    };
    let own = lookup(xcrun, None, "cc", &sandbox);
    assert_eq!(own.get_program(), crate::network::SANDBOX_EXEC);
    let profile = format!("{}{}", crate::network::NO_NETWORK_PROFILE, sandbox.rules);
    assert_eq!(
        own.get_args().collect::<Vec<_>>(),
        [
            "-D",
            "KBF_LEASE=/s/lease-warm-up",
            "-p",
            &profile,
            "/x/xcrun",
            "--find",
            "cc"
        ]
    );
    let tmpdir = (OsStr::new("TMPDIR"), Some(sandbox.dir.as_os_str()));
    assert_eq!(
        own.get_envs().collect::<Vec<_>>(),
        [(OsStr::new(DEVELOPER_DIR), None), tmpdir]
    );
    let dir = Path::new("/A/Xcode.app/Contents/Developer");
    let named = lookup(xcrun, Some(dir), "swiftc", &sandbox);
    assert_eq!(named.get_args().last(), Some(OsStr::new("swiftc")));
    assert_eq!(
        named.get_envs().collect::<Vec<_>>(),
        [(OsStr::new(DEVELOPER_DIR), Some(dir.as_os_str())), tmpdir]
    );
}

/// Catches: a lookup missed for the node's own Xcode or for one of the others, or
/// run other than through the sandbox program in the warm-up's own directory
/// (here a fake that logs its lease parameter and runs the rest), the warm stopped
/// by a lookup that fails or hangs, the directory left behind, and `xcrun` run
/// where there is none or where the directory cannot be made.
#[test]
fn warm_looks_up_every_tool_for_every_xcode_sandboxed() {
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
    let sandbox_log = dir.join("sandbox-log");
    let sandbox_exec = dir.join("sandbox-exec");
    let script = format!(
        "#!/bin/sh\necho \"$2 TMPDIR=$TMPDIR\" >> {}\nshift 4\nexec \"$@\"\n",
        sandbox_log.display()
    );
    std::fs::write(&sandbox_exec, script).expect("script");
    std::fs::set_permissions(&sandbox_exec, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    let sandbox = Sandbox {
        isolation: Isolation::Sandbox(sandbox_exec),
        dir: dir.join("scratch/lease-warm-up"),
        rules: String::new(),
    };
    let real = std::fs::canonicalize(&dir)
        .expect("real")
        .join("scratch/lease-warm-up");
    let thread = warm(
        &xcrun,
        vec![
            None,
            Some(PathBuf::from("/x1")),
            Some(PathBuf::from("/hung")),
        ],
        Duration::from_millis(500),
        &sandbox,
    )
    .expect("a thread");
    thread.join().expect("warmed");
    let read = |path: &Path| -> Vec<String> {
        std::fs::read_to_string(path)
            .expect("log")
            .lines()
            .map(str::to_owned)
            .collect()
    };
    let want: Vec<String> = ["none", "/x1", "/hung"]
        .iter()
        .flat_map(|dir| WARM_TOOLS.map(|tool| format!("{dir} --find {tool}")))
        .collect();
    assert_eq!(read(&log), want);
    let lease = format!("KBF_LEASE={0} TMPDIR={0}", real.display());
    assert_eq!(read(&sandbox_log), vec![lease; want.len()]);
    assert!(!real.exists(), "the warm-up's directory stays");

    assert!(warm(&dir.join("missing"), Vec::new(), WITHIN, &sandbox).is_none());
    let unmakeable = Sandbox {
        dir: log.join("under-a-file"),
        ..sandbox
    };
    assert!(warm(&xcrun, Vec::new(), WITHIN, &unmakeable).is_none());
}

/// How long the fake Xcodes have to answer.
const WITHIN: Duration = Duration::from_secs(2);

/// Catches (CEO decision on issue #164): a question of the survey, `xcrun`'s lookup
/// above all (it reads and fills a cache leases can write), run outside the sandbox,
/// with the network on, without the user-folder rules, in another lease directory or
/// `TMPDIR`, or with `--no-cache` (the fake `xcrun` refuses it); an Xcode asked again
/// through a link to it (`Xcode.app`), or the link not reported; the survey's
/// directory left behind; and an Xcode asked, or reported as anything but failed with
/// why, when that directory cannot be made. The sandbox program is a fake that logs
/// what it runs and how, then runs it.
#[test]
fn the_survey_asks_every_question_under_its_sandbox() {
    let dir = scratch("sandboxed");
    let apps = dir.join("Applications");
    let xcodebuild = fake_xcodebuild(&dir);
    link_xcodebuild(&xcodebuild, &apps.join("Xcode_good.app"));
    std::os::unix::fs::symlink(apps.join("Xcode_good.app"), apps.join("Xcode.app")).expect("link");
    let log = dir.join("sandbox-log");
    let sandbox_exec = fake(
        &dir,
        "sandbox-exec",
        &format!(
            "#!/bin/sh\n\
             case \"$4\" in *'(deny network*)'*'(rules)'*) net=off ;; *) net=on ;; esac\n\
             lease=\"$2\"; shift 4\n\
             echo \"$lease TMPDIR=$TMPDIR net=$net $*\" >> '{}'\n\
             exec \"$@\"\n",
            log.display()
        ),
    );
    let sandbox = Sandbox {
        isolation: Isolation::Sandbox(sandbox_exec),
        dir: dir.join("scratch/lease-survey"),
        rules: "(rules)\n".to_owned(),
    };
    let probe = Probe {
        xcrun: fake_xcrun(&dir),
        within: WITHIN,
        sandbox: Some(sandbox.clone()),
        ..Probe::system(false)
    };
    let surveyed = survey(&apps, &probe);
    let real_apps = std::fs::canonicalize(&apps).expect("real");
    let developer_dir = real_apps.join("Xcode_good.app/Contents/Developer");
    let good = Xcode {
        app: apps.join("Xcode_good.app"),
        developer_dir: Some(developer_dir.clone()),
        build: Some("16C5032a".to_owned()),
        state: State::Ready,
        reason: String::new(),
    };
    let link = Xcode {
        app: apps.join("Xcode.app"),
        ..good.clone()
    };
    assert_eq!(surveyed, [link, good]);
    let real = std::fs::canonicalize(&dir)
        .expect("real")
        .join("scratch/lease-survey");
    let how = format!("KBF_LEASE={0} TMPDIR={0} net=off", real.display());
    let own = developer_dir.join(XCODEBUILD);
    // In any order: an Xcode's questions are asked at once.
    let want: BTreeSet<String> = [
        format!("{} -version", own.display()),
        format!("{} -license check", own.display()),
        format!("{} -checkFirstLaunchStatus", own.display()),
        format!("{} --find clang", probe.xcrun.display()),
    ]
    .iter()
    .map(|ran| format!("{how} {ran}"))
    .collect();
    let read = || -> Vec<String> {
        std::fs::read_to_string(&log)
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    };
    let ran = read();
    assert_eq!(ran.len(), want.len(), "a question asked twice: {ran:#?}");
    assert_eq!(ran.into_iter().collect::<BTreeSet<_>>(), want);
    assert!(!real.exists(), "the survey's directory stays");

    std::fs::remove_file(&log).expect("log");
    let unmakeable = Probe {
        sandbox: Some(Sandbox {
            dir: xcodebuild.join("under-a-file"),
            ..sandbox
        }),
        ..probe
    };
    let refused = survey(&apps, &unmakeable);
    assert_eq!(refused.len(), 2, "{refused:?}");
    for xcode in &refused {
        assert_eq!((xcode.state, xcode.build.as_deref()), (State::Failed, None));
        assert!(xcode.reason.contains("under-a-file"), "{}", xcode.reason);
    }
    assert_eq!(
        read(),
        Vec::<String>::new(),
        "asked with no sandbox to ask in"
    );
    kbf_outputs::remove_tree(&dir).expect("clean");
}

/// Catches the survey's `xcrun` lookups run two at once: `xcrun` rewrites its whole
/// cache with each, so two at once drop each other's entries and the next lookups
/// miss, each then taking seconds (a native daemon on the macOS runner took 16.5 s to
/// survey its Xcodes so). The fake `xcrun` refuses to answer while another runs.
#[test]
fn the_surveys_xcrun_lookups_run_one_at_a_time() {
    let dir = scratch("one-lookup");
    let apps = dir.join("Applications");
    let xcodebuild = fake_xcodebuild(&dir);
    let names = [
        "Xcode_good.app",
        "Xcode_new.app",
        "Xcode_nometal.app",
        "Xcode_old.app",
    ];
    for name in names {
        link_xcodebuild(&xcodebuild, &apps.join(name));
    }
    let busy = dir.join("busy");
    let xcrun = fake(
        &dir,
        "xcrun",
        &format!(
            "#!/bin/sh\n\
             mkdir '{0}' 2>/dev/null || {{ echo 'two lookups at once' >&2; exit 75; }}\n\
             sleep 0.3; rmdir '{0}'; echo /x/clang\n",
            busy.display()
        ),
    );
    let probe = Probe {
        xcrun,
        within: WITHIN,
        ..Probe::system(false)
    };
    let surveyed = survey(&apps, &probe);
    let states: Vec<(State, &str)> = surveyed
        .iter()
        .map(|x| (x.state, x.reason.as_str()))
        .collect();
    assert_eq!(states, vec![(State::Ready, ""); names.len()]);
    kbf_outputs::remove_tree(&dir).expect("clean");
}

/// Links `program` into `app` as its own `xcodebuild`: [`XCODEBUILD`] inside its
/// `DEVELOPER_DIR`, where [`survey`] runs it.
pub(crate) fn link_xcodebuild(program: &Path, app: &Path) {
    let at = app.join("Contents/Developer").join(XCODEBUILD);
    std::fs::create_dir_all(at.parent().expect("usr/bin")).expect("usr/bin");
    std::os::unix::fs::symlink(program, at).expect("link");
}

/// Makes `apps` with each of `names` as an app holding `Contents/Developer` and
/// `xcodebuild` linked in ([`link_xcodebuild`]), plus a link `Xcode.app` to
/// `Xcode_new.app` and a dangling link `Xcode_gone.app`.
fn install(apps: &Path, names: &[&str], xcodebuild: &Path) {
    for app in names {
        link_xcodebuild(xcodebuild, &apps.join(app));
    }
    std::os::unix::fs::symlink(apps.join("Xcode_new.app"), apps.join("Xcode.app")).expect("link");
    std::os::unix::fs::symlink(apps.join("nowhere"), apps.join("Xcode_gone.app"))
        .expect("dangling link");
}

/// Catches: an Xcode left out that answers, one kept that does not answer, has no
/// build, has its licence not accepted (`-version` still exits 0, issue #164), has
/// not had its first launch, has no compiler, hangs, or is a dangling link, another
/// app taken for an Xcode, a question asked without the Xcode's `DEVELOPER_DIR` or
/// with other arguments, the `DEVELOPER_DIR` not the app's real `Contents/Developer`, a
/// later twin replacing the first, a missing directory or program treated as anything
/// but "no Xcode", the Metal toolchain asked for by `discover` (no node requires it
/// there), and a hung Xcode waited for past its time (its fake sleeps for a minute).
/// Catches too `xcodebuild` run other than from inside each Xcode (the review of
/// issue #163: the `/usr/bin` shim may find it through `xcrun`'s cache, which leases
/// can write), and a program path that leads out of the Xcode run at all.
#[test]
fn every_xcode_that_answers_is_found() {
    let dir = scratch("discover");
    let apps = dir.join("Applications");
    let fake = fake_xcodebuild(&dir);
    install(
        &apps,
        &[
            "Xcode_good.app",
            "Xcode_twin.app",
            "Xcode_new.app",
            "Xcode_broken.app",
            "Xcode_firstlaunch.app",
            "Xcode_noclang.app",
            "Xcode_nometal.app",
            "Xcode_mute.app",
            "Xcode_hung.app",
            "Safari.app",
        ],
        &fake,
    );
    let xcodebuild = Path::new(XCODEBUILD);
    let xcrun = fake_xcrun(&dir);
    let real = std::fs::canonicalize(&apps).expect("real");
    let started = Instant::now();
    let found = discover(&apps, xcodebuild, &xcrun, WITHIN);
    let took = started.elapsed();
    assert!(
        took >= WITHIN,
        "the hung Xcode was not given its time: {took:?}"
    );
    assert!(
        took < WITHIN * 10,
        "the hung Xcode was waited for: {took:?}"
    );
    let developer = |app: &str| real.join(app).join("Contents/Developer");
    let want = BTreeMap::from([
        ("16C5032a".to_owned(), developer("Xcode_good.app")),
        ("16E140".to_owned(), developer("Xcode_new.app")),
        ("26A1".to_owned(), developer("Xcode_nometal.app")),
    ]);
    assert_eq!(found, want);
    // Each Xcode's own, by its real path (`Xcode.app` is the new one), never Safari's.
    let ran = dir.join("argv0");
    let read_ran = || -> BTreeSet<String> {
        std::fs::read_to_string(&ran)
            .expect("ran")
            .lines()
            .map(str::to_owned)
            .collect()
    };
    let want_ran: BTreeSet<String> = [
        "Xcode_broken.app",
        "Xcode_firstlaunch.app",
        "Xcode_good.app",
        "Xcode_hung.app",
        "Xcode_mute.app",
        "Xcode_new.app",
        "Xcode_noclang.app",
        "Xcode_nometal.app",
        "Xcode_twin.app",
    ]
    .iter()
    .map(|app| developer(app).join(XCODEBUILD).display().to_string())
    .collect();
    assert_eq!(read_ran(), want_ran);

    assert!(discover(&dir.join("missing"), xcodebuild, &xcrun, WITHIN).is_empty());
    assert!(discover(&apps, Path::new("usr/bin/missing"), &xcrun, WITHIN).is_empty());
    // A program outside the Xcode (the fake itself, as a shim would be) is not run.
    std::fs::remove_file(&ran).expect("ran");
    assert!(discover(&apps, &fake, &xcrun, WITHIN).is_empty());
    assert!(discover(&apps, Path::new("../../../../xcodebuild"), &xcrun, WITHIN).is_empty());
    assert!(!ran.exists(), "a program outside the Xcode ran");
    let outside = Probe {
        xcodebuild: fake.clone(),
        ..Probe::system(false)
    };
    let refused = survey(&apps, &outside);
    let good = refused
        .iter()
        .find(|x| x.app.ends_with("Xcode_good.app"))
        .expect("good");
    assert_eq!(
        (good.state, good.reason.clone()),
        (
            State::Failed,
            format!("{} is not inside the Xcode", fake.display())
        )
    );
    kbf_outputs::remove_tree(&dir).expect("clean");
}

/// Catches: an installed Xcode missing from the survey (left out rather than reported
/// with why), a failed check reported as another (the licence as a missing compiler,
/// a first launch as a licence), a check that times out reported as one a human can
/// fix, the reason not naming the question and its answer, a build or
/// `DEVELOPER_DIR` not kept once learnt, the Metal toolchain not asked for on a node
/// that requires it, asked for on one that does not, a Metal question not answered in
/// time taken as an Xcode before 26 or as missing Metal, an `Xcode` before 26 (no
/// `-showComponent`) taken as missing Metal when `xcrun` finds it, or as having Metal
/// when `xcrun` does not, the Xcodes asked one after another (then an uncached lookup,
/// which takes seconds, is paid once per Xcode before the node says `Hello`), and
/// `xcrun` asked with `--no-cache` (the survey's lookups use the cache, under the
/// sandbox: without it each takes seconds).
#[test]
fn every_installed_xcode_is_reported_with_its_state() {
    let dir = scratch("survey");
    let apps = dir.join("Applications");
    install(
        &apps,
        &[
            "Xcode_good.app",
            "Xcode_new.app",
            "Xcode_broken.app",
            "Xcode_firstlaunch.app",
            "Xcode_noclang.app",
            "Xcode_nometal.app",
            "Xcode_old.app",
            "Xcode_oldnometal.app",
            "Xcode_slowlicence.app",
            "Xcode_slowmetal.app",
            "Xcode_mute.app",
            "Xcode_hung.app",
        ],
        &fake_xcodebuild(&dir),
    );
    let probe = Probe {
        xcrun: fake_xcrun(&dir),
        within: WITHIN,
        ..Probe::system(true)
    };
    let real = std::fs::canonicalize(&apps).expect("real");
    // Each Xcode's own `xcodebuild`, by its real path.
    let own = |app: &str| {
        real.join(app)
            .join("Contents/Developer")
            .join(XCODEBUILD)
            .display()
            .to_string()
    };
    let xcrun_at = probe.xcrun.display();
    let xcode = |name: &str, build: Option<&str>, state, reason: &str| Xcode {
        app: apps.join(name),
        developer_dir: Some(real.join(name).join("Contents/Developer")),
        build: build.map(str::to_owned),
        state,
        reason: reason.to_owned(),
    };
    let want = vec![
        Xcode {
            app: apps.join("Xcode.app"),
            ..xcode("Xcode_new.app", Some("16E140"), State::Ready, "")
        },
        xcode(
            "Xcode_broken.app",
            Some("16B40"),
            State::LicenseNotAccepted,
            &format!(
                "{} -license check exited with exit status: 69: {NOT_AGREED}",
                own("Xcode_broken.app")
            ),
        ),
        xcode(
            "Xcode_firstlaunch.app",
            Some("16G1"),
            State::FirstLaunchNotRun,
            &format!(
                "{} -checkFirstLaunchStatus exited with exit status: 69: {FIRST_LAUNCH}",
                own("Xcode_firstlaunch.app")
            ),
        ),
        Xcode {
            app: apps.join("Xcode_gone.app"),
            developer_dir: None,
            build: None,
            state: State::Failed,
            reason: "No such file or directory (os error 2)".to_owned(),
        },
        xcode("Xcode_good.app", Some("16C5032a"), State::Ready, ""),
        xcode(
            "Xcode_hung.app",
            None,
            State::Failed,
            &format!(
                "{} -version: no answer within 2s; killed",
                own("Xcode_hung.app")
            ),
        ),
        xcode(
            "Xcode_mute.app",
            None,
            State::Failed,
            "no build in xcodebuild -version: \"Xcode 16\"",
        ),
        xcode("Xcode_new.app", Some("16E140"), State::Ready, ""),
        xcode(
            "Xcode_noclang.app",
            Some("16F6"),
            State::Failed,
            &format!(
                "{xcrun_at} --find clang exited with exit status: 72: \
                 xcrun: error: unable to find utility \"clang\""
            ),
        ),
        xcode(
            "Xcode_nometal.app",
            Some("26A1"),
            State::MetalToolchainMissing,
            "xcodebuild -showComponent MetalToolchain says Status: uninstalled",
        ),
        xcode("Xcode_old.app", Some("15A1"), State::Ready, ""),
        xcode(
            "Xcode_oldnometal.app",
            Some("15B1"),
            State::MetalToolchainMissing,
            &format!(
                "{xcrun_at} --find metal exited with exit status: 72: \
                 xcrun: error: unable to find utility \"metal\""
            ),
        ),
        xcode(
            "Xcode_slowlicence.app",
            Some("16H1"),
            State::Failed,
            &format!(
                "{} -license check: no answer within 2s; killed",
                own("Xcode_slowlicence.app")
            ),
        ),
        xcode(
            "Xcode_slowmetal.app",
            Some("26B1"),
            State::Failed,
            &format!(
                "{} -showComponent MetalToolchain: no answer within 2s; killed",
                own("Xcode_slowmetal.app")
            ),
        ),
    ];
    let started = Instant::now();
    assert_eq!(survey(&apps, &probe), want);
    // Three Xcodes each hang for `WITHIN` on one question: one after the other would
    // take three times that.
    let took = started.elapsed();
    assert!(
        took < WITHIN * 2,
        "the Xcodes were asked one by one: {took:?}"
    );
    let developer = |app: &str| real.join(app).join("Contents/Developer");
    assert_eq!(
        ready(&want),
        BTreeMap::from([
            ("15A1".to_owned(), developer("Xcode_old.app")),
            ("16C5032a".to_owned(), developer("Xcode_good.app")),
            ("16E140".to_owned(), developer("Xcode_new.app")),
        ])
    );
    kbf_outputs::remove_tree(&dir).expect("clean");
}

/// Catches: a not-ready Xcode with no fix named, a fix run through the `/usr/bin` shim
/// (`sudo` drops `DEVELOPER_DIR`, so it would fix another Xcode), the licence or first
/// launch fixed without root or the Metal toolchain downloaded as root (it is the
/// daemon's user's), a path a shell would split or expand left unquoted, a fix named for
/// a check no command fixes, and a state carried to the status as another.
#[test]
fn a_fix_runs_the_xcodes_own_xcodebuild() {
    let at = |dir: &str, state| Xcode {
        app: PathBuf::from("/Applications/X.app"),
        developer_dir: Some(PathBuf::from(dir)),
        build: Some("16B40".to_owned()),
        state,
        reason: "why".to_owned(),
    };
    let plain = "/Applications/Xcode_16.1.app/Contents/Developer";
    let fix = |state| at(plain, state).fix();
    let own = format!("{plain}/usr/bin/xcodebuild");
    assert_eq!(
        fix(State::LicenseNotAccepted),
        Some(format!("sudo {own} -license accept"))
    );
    assert_eq!(
        fix(State::FirstLaunchNotRun),
        Some(format!("sudo {own} -runFirstLaunch"))
    );
    assert_eq!(
        fix(State::MetalToolchainMissing),
        Some(format!("{own} -downloadComponent MetalToolchain"))
    );
    assert_eq!(fix(State::Ready), None);
    assert_eq!(fix(State::Failed), None);
    let spaced = at(
        "/Applications/Xcode 16 'b'.app/Contents/Developer",
        State::LicenseNotAccepted,
    );
    assert_eq!(
        spaced.fix().as_deref(),
        Some(
            "sudo '/Applications/Xcode 16 '\\''b'\\''.app/Contents/Developer/usr/bin/xcodebuild' \
             -license accept"
        )
    );
    let unresolved = Xcode {
        developer_dir: None,
        ..at(plain, State::LicenseNotAccepted)
    };
    assert_eq!(unresolved.fix(), None);

    let status = at(plain, State::LicenseNotAccepted).status();
    assert_eq!(
        status,
        XcodeStatus {
            app: "/Applications/X.app".to_owned(),
            build: "16B40".to_owned(),
            state: XcodeState::LicenseNotAccepted.into(),
            reason: "why".to_owned(),
            fix: format!("sudo {own} -license accept"),
        }
    );
    for (state, want) in [
        (State::Ready, XcodeState::Ready),
        (State::FirstLaunchNotRun, XcodeState::FirstLaunchNotRun),
        (
            State::MetalToolchainMissing,
            XcodeState::MetalToolchainMissing,
        ),
        (State::Failed, XcodeState::Failed),
    ] {
        assert_eq!(at(plain, state).status().state(), want);
    }
    assert_eq!(unresolved.status().fix, "");
    let unknown = Xcode {
        build: None,
        ..at(plain, State::Failed)
    };
    assert_eq!(unknown.status().build, "");
}

/// Catches a reason that carries a whole stderr (an Xcode can print pages), and one cut
/// inside a character.
#[test]
fn a_long_reason_is_cut() {
    assert_eq!(cut("short", 5), "short");
    assert_eq!(cut("abcdef", 3), "abc...");
    assert_eq!(cut("aé", 2), "a...");
    let script = "head -c 2000 /dev/zero | tr '\\0' x >&2; exit 3";
    let refused = answer(
        Path::new("/bin/sh"),
        &["-c", script],
        Path::new("/"),
        WITHIN,
        None,
    );
    let want = format!(
        "/bin/sh -c {script} exited with exit status: 3: {}...",
        "x".repeat(REASON_BYTES)
    );
    assert_eq!(refused, Err(Unanswered::Refused(want)));
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

/// Catches a question asked once only when its program is busy (ETXTBSY), so an
/// Xcode is reported as failed because another thread forked while its tool was being
/// written; and a program that stays busy waited for past the limit. Linux only: macOS
/// starts a program that is open for writing.
#[cfg(target_os = "linux")]
#[test]
fn a_busy_program_is_started_again_within_the_limit() {
    let dir = scratch("busy");
    let program = dir.join("busy");
    // Held open for writing, as a child forked before it execs holds a file just
    // written: starting it fails with ETXTBSY until the handle is closed.
    let writer = std::fs::File::create(&program).expect("create");
    std::fs::write(&program, "#!/bin/sh\necho 'Build version 1A1'\n").expect("script");
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    let held = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(200));
        drop(writer);
    });
    let out = output_within(std::process::Command::new(&program), WITHIN).expect("answers");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "Build version 1A1\n");
    held.join().expect("closed");

    let _writer = std::fs::OpenOptions::new()
        .append(true)
        .open(&program)
        .expect("open");
    let started = Instant::now();
    let limit = Duration::from_millis(300);
    let busy = output_within(std::process::Command::new(&program), limit).expect_err("busy");
    assert_eq!(busy.raw_os_error(), Some(libc::ETXTBSY));
    assert!(started.elapsed() < WITHIN, "waited past the limit");
    kbf_outputs::remove_tree(&dir).expect("clean");
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
