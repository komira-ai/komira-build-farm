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

/// A link to this test binary named `kbf-act`, made once: run through it, the binary
/// runs kbf-testkit's act steps (`kbf_testkit::act`) instead of the tests.
pub(crate) fn act() -> PathBuf {
    static LINK: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    LINK.get_or_init(|| {
        let link = scratch("act").join(kbf_testkit::act::NAME);
        kbf_testkit::act::link_self(&link).expect("link");
        link
    })
    .clone()
}

/// Makes `path` a link to this test binary, which answers as the fake tool its base
/// name names (`xcrun`, `xcodebuild` or `sandbox-exec`), and gives it `rules`
/// (`kbf_testkit::fake`). Returns `path`.
pub(crate) fn act_fake(path: &Path, rules: &str) -> PathBuf {
    kbf_testkit::act::link_self(path).expect("link");
    set_rules(path, rules);
    path.to_owned()
}

/// Replaces the rules of the fake at `path` with `rules` in one rename, so a run of it
/// reads the old rules or the new ones, never part of either.
pub(crate) fn set_rules(path: &Path, rules: &str) {
    let (new, rules_file) = (
        format!("{}.rules.new", path.display()),
        format!("{}.rules", path.display()),
    );
    std::fs::write(&new, rules).expect("rules");
    std::fs::rename(&new, rules_file).expect("rules in place");
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
    let profile = format!(
        "{}{}{}",
        crate::network::NO_NETWORK_PROFILE,
        crate::network::NO_DEVICE_RULES,
        sandbox.rules
    );
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
/// by a lookup that fails or hangs, a hung lookup not killed at the limit it was
/// given (it then finishes and logs `late`), the directory left behind, and `xcrun`
/// run where there is none or where the directory cannot be made.
#[test]
fn warm_looks_up_every_tool_for_every_xcode_sandboxed() {
    let dir = scratch("warm");
    let log = dir.join("log");
    // Logs `<DEVELOPER_DIR, empty when unset> <arguments>`; `ld` is not found, and
    // `swift` hangs for the Xcode `/hung`: past `UNTIL_KILLED`, short of `WITHIN`.
    let logged = format!("append {} ${{DEVELOPER_DIR}} $*", log.display());
    let xcrun = act_fake(
        &dir.join("xcrun"),
        &format!(
            "--find ld\t-\t{logged} ; echo --stderr not found ; exit 1\n\
             --find swift\tDEVELOPER_DIR=/hung\t{logged} ; sleep 40 ; append {} late\n\
             *\t-\t{logged}\n",
            log.display()
        ),
    );
    // Logs its lease parameter and the command's TMPDIR, then runs the command.
    let sandbox_log = dir.join("sandbox-log");
    let sandbox_exec = act_fake(
        &dir.join("sandbox-exec"),
        &format!(
            "*\t-\tappend {} $2 TMPDIR=${{TMPDIR}}\n",
            sandbox_log.display()
        ),
    );
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
        UNTIL_KILLED,
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
    // The node's own Xcode is looked up with no DEVELOPER_DIR: empty in the log.
    let want: Vec<String> = ["", "/x1", "/hung"]
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

/// How long a question meant to answer has: the daemon's own limit. A hosted macOS
/// runner has taken some 13 s to answer questions that answer at once (issue #481), so
/// a limit of a few seconds failed Xcodes and lookups that never hung. Only a test
/// whose fake hangs on purpose waits for a limit, and it gives [`UNTIL_KILLED`].
const WITHIN: Duration = ANSWER_WITHIN;

/// The limit in a test whose fake hangs on purpose: what hangs is killed only when it
/// is up, so the test waits for it once, and every question there that answers must
/// answer within it. Past the slowest answer seen on a hosted macOS runner (some 13 s,
/// issue #481), and short of [`WITHIN`] to keep such a test short.
const UNTIL_KILLED: Duration = Duration::from_secs(20);

/// A stand-in for `sandbox-exec` that logs the lease it is given, `TMPDIR` and
/// whether the rules deny the network, and the program with its arguments, to `log`,
/// then runs the program. It runs it as `sh PROGRAM`, not by exec: the programs are
/// scripts, and a script another thread's fork still holds open for writing (it was
/// written a moment ago) cannot be exec'd (ETXTBSY, exit 126) but is read by `sh` all
/// the same. The survey's own spawns are started again when busy; this one is not the
/// survey's.
fn fake_sandbox_exec(dir: &Path, log: &Path) -> PathBuf {
    fake(
        dir,
        "sandbox-exec",
        &format!(
            "#!/bin/sh\n\
             case \"$4\" in *'(deny network*)'*'(rules)'*) net=off ;; *) net=on ;; esac\n\
             lease=\"$2\"; shift 4\n\
             echo \"$lease TMPDIR=$TMPDIR net=$net $*\" >> '{}'\n\
             exec /bin/sh \"$@\"\n",
            log.display()
        ),
    )
}

/// Catches the stand-in sandbox failing a question only because its program is busy
/// (ETXTBSY), which made the survey test below fail now and then: a script held open
/// for writing, as a child forked while it was written holds it, is run all the same.
#[cfg(target_os = "linux")]
#[test]
fn the_fake_sandbox_runs_a_program_held_open_for_writing() {
    let dir = scratch("sandbox-busy");
    let program = fake(&dir, "program", "#!/bin/sh\necho ran \"$@\"\n");
    let log = dir.join("log");
    let sandbox = fake_sandbox_exec(&dir, &log);
    let _writer = std::fs::OpenOptions::new()
        .append(true)
        .open(&program)
        .expect("open");
    // Exec'd, the program is busy.
    let busy = std::process::Command::new(&program)
        .output()
        .expect_err("busy");
    assert_eq!(busy.raw_os_error(), Some(libc::ETXTBSY));
    // Through `sh`, as the sandbox is itself a script just written.
    let out = std::process::Command::new("/bin/sh")
        .arg(&sandbox)
        .args(["-D", "lease", "-p", "(rules)"])
        .arg(&program)
        .arg("x")
        .output()
        .expect("sandbox");
    assert!(out.status.success(), "{:?}", out.status);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "ran x\n");
    kbf_outputs::remove_tree(&dir).expect("clean");
}

/// Catches (CEO decision on issue #164): a question of the survey, `xcrun`'s lookup
/// above all (it reads and fills a cache leases can write), run outside the sandbox,
/// with the network on, without the user-folder rules, in another lease directory or
/// `TMPDIR`, or with `--no-cache` (the fake `xcrun` refuses it); the same of the Metal
/// questions a node that requires the toolchain asks, `xcodebuild -showComponent
/// MetalToolchain` (which starts `xcrun` and reads and writes its cache) and, for an
/// Xcode before 26 (`Xcode_old`, which refuses it), `xcrun --find metal`; an Xcode
/// asked again through a link to it (`Xcode.app`), or the link not reported; the
/// survey's directory left behind; and an Xcode asked, or reported as anything but
/// failed with why, when that directory cannot be made. The sandbox program is a fake
/// that logs what it runs and how, then runs it: a question asked outside it is
/// missing from its log.
#[test]
fn the_survey_asks_every_question_under_its_sandbox() {
    let dir = scratch("sandboxed");
    let apps = dir.join("Applications");
    let xcodebuild = fake_xcodebuild(&dir);
    link_xcodebuild(&xcodebuild, &apps.join("Xcode_good.app"));
    link_xcodebuild(&xcodebuild, &apps.join("Xcode_old.app"));
    std::os::unix::fs::symlink(apps.join("Xcode_good.app"), apps.join("Xcode.app")).expect("link");
    let log = dir.join("sandbox-log");
    let sandbox_exec = fake_sandbox_exec(&dir, &log);
    let sandbox = Sandbox {
        isolation: Isolation::Sandbox(sandbox_exec),
        dir: dir.join("scratch/lease-survey"),
        rules: "(rules)\n".to_owned(),
    };
    let probe = Probe {
        xcrun: fake_xcrun(&dir),
        within: WITHIN,
        sandbox: Some(sandbox.clone()),
        ..Probe::system(true)
    };
    let surveyed = survey(&apps, &probe);
    let real_apps = std::fs::canonicalize(&apps).expect("real");
    let ready = |name: &str, build: &str| Xcode {
        app: apps.join(name),
        developer_dir: Some(real_apps.join(name).join("Contents/Developer")),
        build: Some(build.to_owned()),
        state: State::Ready,
        reason: String::new(),
    };
    let good = ready("Xcode_good.app", "16C5032a");
    let link = Xcode {
        app: apps.join("Xcode.app"),
        ..good.clone()
    };
    assert_eq!(surveyed, [link, good, ready("Xcode_old.app", "15A1")]);
    let real = std::fs::canonicalize(&dir)
        .expect("real")
        .join("scratch/lease-survey");
    let how = format!("KBF_LEASE={0} TMPDIR={0} net=off", real.display());
    let xcrun = probe.xcrun.display();
    // In any order: an Xcode's questions are asked at once, and both Xcodes at once.
    let mut want: BTreeSet<String> = ["Xcode_good.app", "Xcode_old.app"]
        .iter()
        .flat_map(|name| {
            let own = real_apps
                .join(name)
                .join("Contents/Developer")
                .join(XCODEBUILD);
            [
                format!("{} -version", own.display()),
                format!("{} -license check", own.display()),
                format!("{} -checkFirstLaunchStatus", own.display()),
                format!("{} -showComponent MetalToolchain", own.display()),
                format!("{xcrun} --find clang"),
            ]
        })
        .map(|ran| format!("{how} {ran}"))
        .collect();
    // Two lines alike (each Xcode's clang lookup) are one in the set; the count below
    // takes them as two.
    assert_eq!(want.len(), 9);
    // Only the Xcode before 26 is asked for its bundled Metal.
    want.insert(format!("{how} {xcrun} --find metal"));
    let read = || -> Vec<String> {
        std::fs::read_to_string(&log)
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    };
    let ran = read();
    assert_eq!(
        ran.len(),
        11,
        "a question asked twice, or not at all: {ran:#?}"
    );
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
    assert_eq!(refused.len(), 3, "{refused:?}");
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
    let found = discover(&apps, xcodebuild, &xcrun, UNTIL_KILLED);
    let took = started.elapsed();
    assert!(
        took >= UNTIL_KILLED,
        "the hung Xcode was not given its time: {took:?}"
    );
    // Its fake sleeps for a minute, three times the limit.
    assert!(
        took < UNTIL_KILLED * 2,
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
/// when `xcrun` does not, the Xcodes asked one after another (then each Xcode's
/// questions are paid in turn before any Xcode is ready), and
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
        within: UNTIL_KILLED,
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
                "{} -version: no answer within {UNTIL_KILLED:?}; killed",
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
                "{} -license check: no answer within {UNTIL_KILLED:?}; killed",
                own("Xcode_slowlicence.app")
            ),
        ),
        xcode(
            "Xcode_slowmetal.app",
            Some("26B1"),
            State::Failed,
            &format!(
                "{} -showComponent MetalToolchain: no answer within {UNTIL_KILLED:?}; killed",
                own("Xcode_slowmetal.app")
            ),
        ),
    ];
    let started = Instant::now();
    assert_eq!(survey(&apps, &probe), want);
    // Three Xcodes each hang until killed at `UNTIL_KILLED` on one question: one after
    // the other would take three times that.
    let took = started.elapsed();
    assert!(
        took < UNTIL_KILLED * 2,
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
    assert_eq!(fix(State::NotSurveyed), None);
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
        (State::NotSurveyed, XcodeState::NotSurveyed),
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
    let long = "x".repeat(REASON_BYTES * 4);
    let act = act();
    let args = ["echo", "--stderr", &long, ";", "exit", "3"];
    let refused = answer(&act, &args, Path::new("/"), WITHIN, None);
    let want = format!(
        "{} {} exited with exit status: 3: {}...",
        act.display(),
        args.join(" "),
        "x".repeat(REASON_BYTES)
    );
    assert_eq!(refused, Err(Unanswered::Refused(want)));
}

/// `echo` and `words` words of 999 `x`s: `words` thousand bytes, the newline included.
fn thousands(stream: &str, words: usize) -> Vec<String> {
    let word = "x".repeat(999);
    std::iter::once("echo".to_owned())
        .chain((stream == "stderr").then(|| "--stderr".to_owned()))
        .chain(std::iter::repeat_n(word, words))
        .collect()
}

/// Catches an answer cut short (stdout and stderr read in one buffer's worth, or the
/// one for the other), the files the output went to left behind in their directory,
/// and a directory they cannot be made in taken as an answer.
#[test]
fn a_long_answer_is_read_whole() {
    let dir = scratch("long");
    let mut command = std::process::Command::new(act());
    command
        .args(thousands("stdout", 300))
        .arg(";")
        .args(thousands("stderr", 200));
    let out = output_within(command, WITHIN, &dir).expect("answers");
    assert!(out.status.success());
    assert_eq!((out.stdout.len(), out.stderr.len()), (300_000, 200_000));
    let left: Vec<_> = std::fs::read_dir(&dir).expect("dir").collect();
    assert!(left.is_empty(), "left behind: {left:?}");
    let nowhere = output_within(
        std::process::Command::new(act()),
        WITHIN,
        &dir.join("missing"),
    );
    assert_eq!(
        nowhere.expect_err("no files").kind(),
        io::ErrorKind::NotFound
    );
    kbf_outputs::remove_tree(&dir).expect("clean");
}

/// Catches an answer that waits for its output to close rather than for its process
/// to exit (issue #481). On macOS, which has no `pipe2`, the standard library makes a
/// pipe and only then marks it close-on-exec, so a child another thread starts in
/// between inherits it: a hung Xcode's question then held a good Xcode's output open
/// until it was killed, and the good one was reported as failed. Here the process
/// leaves a child holding stdout and stderr until the test releases it, after the
/// answer (the effect of that leak made certain): an answer that waited for it would
/// not come within the limit.
#[test]
fn an_answer_is_read_when_its_process_exits_though_another_holds_its_output() {
    let dir = scratch("held-answer");
    let release = dir.join("release");
    let mut command = std::process::Command::new(act());
    command
        .args(["spawn-child", "wait-for"])
        .arg(&release)
        .args([HELD_SECS, ";", "echo", "Build version 1A1"]);
    let out = output_within(command, WITHIN, &dir);
    std::fs::write(&release, "").expect("release");
    let out = out.expect("answered at exit");
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout), "Build version 1A1\n");
    kbf_outputs::remove_tree(&dir).expect("clean");
}

/// How long, in seconds, a process holding another's output waits to be released:
/// past [`WITHIN`], so a wait for it would end in a timeout, not an answer.
const HELD_SECS: &str = "120";

/// Catches the twin of issue #481 (on native-macos, `Xcode_twin` was returned for
/// build 16C5032a where `Xcode_good` was expected): the good Xcode's `xcodebuild`
/// leaves a process holding its stdout and stderr until the test releases it, after
/// the survey, as a hung Xcode's question did when it inherited them, and the good
/// Xcode is still the one found, ready, for its build.
#[test]
fn an_xcode_whose_output_another_process_holds_is_still_found() {
    let dir = scratch("held");
    let apps = dir.join("Applications");
    let fake = fake_xcodebuild(&dir);
    let release = dir.join("release");
    let held = dir.join("held-xcodebuild");
    std::fs::write(
        &held,
        format!(
            "#!/bin/sh\n'{}' spawn-child wait-for '{}' {HELD_SECS}\nexec '{}' \"$@\"\n",
            act().display(),
            release.display(),
            fake.display()
        ),
    )
    .expect("held fake");
    std::fs::set_permissions(&held, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    link_xcodebuild(&held, &apps.join("Xcode_good.app"));
    link_xcodebuild(&fake, &apps.join("Xcode_twin.app"));
    let xcrun = fake_xcrun(&dir);
    let found = discover(&apps, Path::new(XCODEBUILD), &xcrun, WITHIN);
    std::fs::write(&release, "").expect("release");
    let real = std::fs::canonicalize(&apps).expect("real");
    assert_eq!(
        found,
        BTreeMap::from([(
            "16C5032a".to_owned(),
            real.join("Xcode_good.app/Contents/Developer")
        )])
    );
    kbf_outputs::remove_tree(&dir).expect("clean");
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
    // Its interpreter is the act steps' link: run, it echoes its own path.
    std::fs::write(&program, format!("#!{} echo\n", act().display())).expect("script");
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    let held = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(200));
        drop(writer);
    });
    let out = output_within(std::process::Command::new(&program), WITHIN, &dir).expect("answers");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        format!("{}\n", program.display())
    );
    held.join().expect("closed");

    let _writer = std::fs::OpenOptions::new()
        .append(true)
        .open(&program)
        .expect("open");
    let started = Instant::now();
    let limit = Duration::from_millis(300);
    let busy = output_within(std::process::Command::new(&program), limit, &dir).expect_err("busy");
    assert_eq!(busy.raw_os_error(), Some(libc::ETXTBSY));
    // Retrying a busy program stops at its own limit; one second covers the last
    // retry and the spawn, not a host stall (this test has no hung fake).
    let waited = started.elapsed();
    assert!(
        waited < limit + Duration::from_secs(1),
        "waited past the limit: {waited:?}"
    );
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
