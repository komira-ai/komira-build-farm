//! On macOS, the tools that write in the daemon user's own temporary and cache folders
//! (`confstr(_CS_DARWIN_USER_TEMP_DIR)` and `_CS_DARWIN_USER_CACHE_DIR`, under
//! `/var/folders`) whatever `TMPDIR` says still work under the driver's sandbox
//! (issue #163): a Foundation atomic save, `swift build`, `xcodebuild`, and the
//! `/usr/bin` compiler shims, whose `xcrun` keeps its cache there.

#![cfg(target_os = "macos")]

mod support;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use kbf_daemon::Runtime;
use kbf_driver_native::user_folders::UserFolders;
use kbf_driver_native::xcode;
use kbf_driver_native::{NativeConfig, NativeRuntime};
use support::{
    MemoryCas, Spec, config, runtime, sandbox_denials, scratch, stderr, stdout, user_folder, work,
};

/// The actions' `PATH`.
const PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin";

/// Held to read by each test here that runs a developer tool (`swiftc`, `swift build`,
/// `xcodebuild`), and to write by [`the_compiler_shims_print_nothing_on_stderr`], which
/// times the compiler shims. Every `xcrun` call rewrites the user's whole `xcrun_db`
/// (it writes `xcrun_db-<random>` and renames it over), so `xcrun` calls running at the
/// same moment drop each other's entries. On the macOS runner, with `swift build`
/// running beside it, `xcrun_db` was replaced 18 times and shrank 4 times during the
/// timed run, and each of its `cc` calls missed the cache: 6.2 s for five, against
/// 0.2 s alone (PR #172).
static DEVELOPER_TOOLS: tokio::sync::RwLock<()> = tokio::sync::RwLock::const_new(());

/// Runs `spec` as lease `1.seq` with up to ten minutes: a cold `xcodebuild` is slow.
async fn run_long(
    rt: &NativeRuntime<MemoryCas>,
    cas: &MemoryCas,
    seq: u64,
    spec: &Spec,
) -> kbf_proto::reapi::ActionResult {
    let action = spec.clone().timeout(Duration::from_secs(600)).store(cas);
    tokio::time::timeout(Duration::from_secs(630), rt.run(work(seq, action, 0)))
        .await
        .expect("the run ends")
        .expect("ran")
}

/// A Swift program that saves a file atomically and asks for an item replacement
/// directory, the two ways Foundation writes through the user's temporary folder,
/// printing `atomic=ok` and `replacement=ok` or the error.
const ATOMIC_SWIFT: &str = r#"import Foundation
let here = FileManager.default.currentDirectoryPath
do {
    try "x".write(toFile: here + "/atomic", atomically: true, encoding: .utf8)
    print("atomic=ok")
} catch {
    print("atomic=\(error)")
}
do {
    let dir = try FileManager.default.url(
        for: .itemReplacementDirectory, in: .userDomainMask,
        appropriateFor: URL(fileURLWithPath: here), create: true)
    print("replacement=ok")
    try? FileManager.default.removeItem(at: dir)
} catch {
    print("replacement=\(error)")
}
"#;

/// Catches a sandbox that refuses Foundation's atomic save and item replacement
/// directory (both write under the user's temporary folder, not `TMPDIR`): every tool
/// that saves a file safely fails under the driver.
#[tokio::test]
async fn a_foundation_atomic_save_works() {
    let _tools = DEVELOPER_TOOLS.read().await;
    let dir = scratch("atomic");
    let cas = Arc::new(MemoryCas::default());
    let rt = runtime(config(&dir), &cas);
    let mut spec = Spec::sh(
        "xcrun swiftc -o atomic-probe atomic.swift && ./atomic-probe && cat atomic && echo",
    )
    .env("PATH", PATH);
    spec.inputs = vec![("atomic.swift".to_owned(), ATOMIC_SWIFT.as_bytes().to_vec())];
    let result = run_long(&rt, &cas, 1, &spec).await;
    assert_eq!(
        stdout(&cas, &result).trim(),
        "atomic=ok\nreplacement=ok\nx",
        "{}\n{}",
        stderr(&cas, &result),
        sandbox_denials()
    );
    assert_eq!(result.exit_code, 0);
}

/// A Swift package with one executable target, `hello`, that prints `hello`.
fn package() -> Vec<(String, Vec<u8>)> {
    let manifest = "// swift-tools-version:5.9\n\
                    import PackageDescription\n\
                    let package = Package(name: \"hello\", targets: [\n\
                    .executableTarget(name: \"hello\", path: \"Sources/hello\")])\n";
    vec![
        ("Package.swift".to_owned(), manifest.as_bytes().to_vec()),
        ("main.swift".to_owned(), b"print(\"hello\")\n".to_vec()),
    ]
}

/// Lays [`package`]'s files out as SwiftPM wants them.
const LAYOUT: &str = "mkdir -p Sources/hello && mv main.swift Sources/hello/ && ";

/// Catches a sandbox under which `swift build` (its own sandbox off, its build
/// directory in the lease) cannot build a one-file package: SwiftPM writes its
/// files atomically, through the user's temporary folder.
#[tokio::test]
async fn swift_build_builds_a_package() {
    let _tools = DEVELOPER_TOOLS.read().await;
    let dir = scratch("swiftpm");
    let cas = Arc::new(MemoryCas::default());
    let rt = runtime(config(&dir), &cas);
    let mut spec = Spec::sh(&format!(
        "{LAYOUT}swift build --disable-sandbox --scratch-path \"$TMPDIR/build\" \
         && \"$TMPDIR/build/debug/hello\""
    ))
    .env("PATH", PATH);
    spec.inputs = package();
    let result = run_long(&rt, &cas, 1, &spec).await;
    let out = stdout(&cas, &result);
    assert_eq!(
        (result.exit_code, out.lines().last()),
        (0, Some("hello")),
        "{out}{}\n{}",
        stderr(&cas, &result),
        sandbox_denials()
    );
}

/// The Xcodes on this runner that answer ([`xcode::discover`]). Fails when an Xcode is
/// installed (an `/Applications/Xcode*.app` with its own `xcodebuild`) and none
/// answers, so a `discover` that runs the wrong program cannot turn the tests that
/// need an Xcode into tests that check nothing.
fn xcodes() -> std::collections::BTreeMap<String, PathBuf> {
    let all = xcode::discover(
        Path::new(xcode::APPLICATIONS),
        Path::new(xcode::XCODEBUILD),
        Path::new(xcode::XCRUN),
        xcode::ANSWER_WITHIN,
    );
    let installed: Vec<String> = std::fs::read_dir(xcode::APPLICATIONS)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|app| {
            let name = app.file_name().unwrap_or_default().to_string_lossy();
            name.starts_with("Xcode") && name.ends_with(".app")
        })
        .filter(|app| {
            app.join("Contents/Developer")
                .join(xcode::XCODEBUILD)
                .is_file()
        })
        .map(|app| app.display().to_string())
        .collect();
    assert!(
        !all.is_empty() || installed.is_empty(),
        "installed: {installed:?}; none answers"
    );
    all
}

/// Catches a sandbox under which the daemon's survey (every question asked as an
/// action runs: `xcodebuild -version`, `-license check`, `-checkFirstLaunchStatus`
/// and `xcrun --find clang`, the network off, the user-folder rules) answers otherwise
/// than the same survey run without it: an Xcode the sandbox makes look not ready, or
/// another build. Fails when no Xcode is ready, so it cannot pass by comparing nothing.
/// Writes how long each survey took to stderr (not captured) for the CI log.
#[tokio::test]
async fn the_sandboxed_survey_answers_as_an_unsandboxed_one() {
    use std::io::Write as _;

    let _tools = DEVELOPER_TOOLS.read().await;
    let dir = scratch("survey");
    let cas = Arc::new(MemoryCas::default());
    let rt = runtime(config(&dir), &cas);
    let apps = Path::new(xcode::APPLICATIONS);
    let key = |x: &xcode::Xcode| {
        (
            x.app.clone(),
            x.developer_dir.clone(),
            x.build.clone(),
            x.state,
        )
    };
    let started = std::time::Instant::now();
    let plain = xcode::survey(apps, &xcode::Probe::system(false));
    let plain_took = started.elapsed();
    let probe = xcode::Probe {
        sandbox: Some(rt.sandbox(kbf_driver_native::SURVEY_DIR)),
        ..xcode::Probe::system(false)
    };
    let started = std::time::Instant::now();
    let sandboxed = xcode::survey(apps, &probe);
    let sandboxed_took = started.elapsed();
    let _ = writeln!(
        std::io::stderr(),
        "user_folders.rs: {} Xcodes surveyed in {plain_took:.1?} unsandboxed, \
         {sandboxed_took:.1?} sandboxed",
        sandboxed.len()
    );
    assert!(
        plain.iter().any(|x| x.state == xcode::State::Ready),
        "no Xcode is ready: {plain:#?}"
    );
    assert_eq!(
        sandboxed.iter().map(key).collect::<Vec<_>>(),
        plain.iter().map(key).collect::<Vec<_>>(),
        "{sandboxed:#?}\n{}",
        sandbox_denials()
    );
    assert!(
        !dir.join("leases")
            .join(kbf_driver_native::SURVEY_DIR)
            .exists(),
        "the survey's directory stays"
    );
}

/// The newest Xcode on this runner, if it has one.
fn newest_xcode() -> Option<(String, PathBuf)> {
    xcodes().into_iter().last()
}

/// A project with one target, `hello`, a command-line tool built from `main.c`.
const PBXPROJ: &str = r#"// !$*UTF8*$!
{
	archiveVersion = 1;
	classes = {
	};
	objectVersion = 56;
	objects = {
		A10000000000000000000001 = {isa = PBXBuildFile; fileRef = A10000000000000000000002; };
		A10000000000000000000002 = {isa = PBXFileReference; lastKnownFileType = sourcecode.c.c; path = main.c; sourceTree = "<group>"; };
		A10000000000000000000003 = {isa = PBXFileReference; explicitFileType = "compiled.mach-o.executable"; includeInIndex = 0; path = hello; sourceTree = BUILT_PRODUCTS_DIR; };
		A10000000000000000000004 = {isa = PBXGroup; children = (A10000000000000000000002, A10000000000000000000005, ); sourceTree = "<group>"; };
		A10000000000000000000005 = {isa = PBXGroup; children = (A10000000000000000000003, ); name = Products; sourceTree = "<group>"; };
		A10000000000000000000006 = {isa = PBXSourcesBuildPhase; buildActionMask = 2147483647; files = (A10000000000000000000001, ); runOnlyForDeploymentPostprocessing = 0; };
		A10000000000000000000007 = {isa = PBXNativeTarget; buildConfigurationList = A10000000000000000000008; buildPhases = (A10000000000000000000006, ); buildRules = ( ); dependencies = ( ); name = hello; productName = hello; productReference = A10000000000000000000003; productType = "com.apple.product-type.tool"; };
		A10000000000000000000008 = {isa = XCConfigurationList; buildConfigurations = (A10000000000000000000009, ); defaultConfigurationIsVisible = 0; defaultConfigurationName = Release; };
		A10000000000000000000009 = {isa = XCBuildConfiguration; buildSettings = {PRODUCT_NAME = "$(TARGET_NAME)"; CODE_SIGN_IDENTITY = "-"; }; name = Release; };
		A1000000000000000000000A = {isa = PBXProject; buildConfigurationList = A1000000000000000000000B; compatibilityVersion = "Xcode 14.0"; mainGroup = A10000000000000000000004; productRefGroup = A10000000000000000000005; projectDirPath = ""; projectRoot = ""; targets = (A10000000000000000000007, ); };
		A1000000000000000000000B = {isa = XCConfigurationList; buildConfigurations = (A1000000000000000000000C, ); defaultConfigurationIsVisible = 0; defaultConfigurationName = Release; };
		A1000000000000000000000C = {isa = XCBuildConfiguration; buildSettings = {SDKROOT = macosx; MACOSX_DEPLOYMENT_TARGET = 13.0; }; name = Release; };
	};
	rootObject = A1000000000000000000000A;
}
"#;

/// Catches a sandbox under which `xcodebuild` (derived data in the lease) cannot
/// build a one-file target: it saves its log store atomically, through the user's
/// temporary folder, and exits 74 when that fails (issue #163).
#[tokio::test]
async fn xcodebuild_builds_a_target() {
    let _tools = DEVELOPER_TOOLS.read().await;
    let Some((build, developer_dir)) = newest_xcode() else {
        eprintln!("no Xcode on this runner: nothing to check");
        return;
    };
    let dir = scratch("xcodebuild");
    let mut config: NativeConfig = config(&dir);
    config.xcodes = [(build.clone(), developer_dir)].into_iter().collect();
    let cas = Arc::new(MemoryCas::default());
    let rt = runtime(config, &cas);
    let mut spec = Spec::sh(
        "mkdir hello.xcodeproj && mv project.pbxproj hello.xcodeproj/ \
         && xcodebuild -project hello.xcodeproj -scheme hello -configuration Release \
         -derivedDataPath \"$TMPDIR/dd\" build \
         && \"$TMPDIR/dd/Build/Products/Release/hello\"",
    )
    .env("PATH", PATH)
    .property("xcode", &build);
    spec.inputs = vec![
        ("project.pbxproj".to_owned(), PBXPROJ.as_bytes().to_vec()),
        (
            "main.c".to_owned(),
            b"#include <stdio.h>\nint main(void) { puts(\"hello\"); return 0; }\n".to_vec(),
        ),
    ];
    let result = run_long(&rt, &cas, 1, &spec).await;
    let out = stdout(&cas, &result);
    assert_eq!(
        (result.exit_code, out.lines().last()),
        (0, Some("hello")),
        "{}\n{}\n{}",
        tail(&out),
        tail(&stderr(&cas, &result)),
        sandbox_denials()
    );
}

/// The last 40 lines of `text`: `xcodebuild` says a lot.
fn tail(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(40)..].join("\n")
}

/// How many times [`SHIMS`] times each compiler, one call at a time.
const CALLS: usize = 7;

/// What [`the_compiler_shims_print_nothing_on_stderr`] runs: each shim's `--version`
/// with its stderr (but the version line `swiftc` always prints there) copied to
/// stdout, then, as the last line, the seconds each of `$CALLS` `cc --version` calls
/// takes and the seconds each of as many calls of the `clang` it runs takes, called
/// directly: two comma-separated lists.
const SHIMS: &str = r#"for tool in cc clang swiftc; do
  $tool --version > /dev/null 2> "err-$tool" || echo "$tool failed"
done
cat err-cc err-clang; grep -v '^swift-driver version' err-swiftc
direct=$(xcrun --find clang) || exit 1
perl -MTime::HiRes=time -e '
  sub t {
    my @t;
    for (1..$ENV{CALLS}) {
      my $s = time;
      system("$_[0] --version > /dev/null 2>&1") == 0 or die "$_[0]";
      push @t, sprintf "%.3f", time - $s;
    }
    join ",", @t
  }
  # MUTANT: half a second more for each cc call, sandboxed or not.
  printf "%s %s\n", t("sleep 0.5; cc"), t($ARGV[0]);
' "$direct"
"#;

/// The output of [`SHIMS`] but its last line, and the call times on that line: those
/// of `cc`, then those of `clang` called directly.
fn shim_times(out: &str) -> (String, Vec<f64>, Vec<f64>) {
    let (rest, last) = out
        .trim_end()
        .rsplit_once('\n')
        .unwrap_or(("", out.trim_end()));
    let times: Vec<Vec<f64>> = last
        .split(' ')
        .map(|list| {
            list.split(',')
                .map(|t| t.parse().unwrap_or_else(|_| panic!("times in {out:?}")))
                .collect()
        })
        .collect();
    match <[Vec<f64>; 2]>::try_from(times) {
        Ok([shim, direct]) if shim.len() == CALLS && direct.len() == CALLS => {
            (rest.to_owned(), shim, direct)
        }
        _ => panic!("{CALLS} times twice in {out:?}"),
    }
}

/// The fastest of `times`: what a call costs with the runner's noise (other tests,
/// other jobs on the host) taken out, as noise only ever adds.
fn fastest(times: &[f64]) -> f64 {
    times.iter().copied().fold(f64::INFINITY, f64::min)
}

/// How much slower than twice its reference the fastest sandboxed `cc` call may be.
/// The defect costs at least 0.31 s a call (issue #163, with an older cache still
/// readable), 1.2 s with the cache lost (PR #172) and 5.3 s with none; a healthy
/// sandboxed call costs well under 0.1 s on the runner (the times this test prints).
const SHIM_MARGIN: f64 = 0.1;

/// Catches the `/usr/bin` compiler shims failing to write `xcrun`'s cache in the
/// user's temporary folder: every `cc`, `clang` or `swiftc` call then prints
/// "couldn't create cache file" and runs several times slower (0.31 s a call against
/// 0.065 s for `clang` called directly, in issue #163). Checked with the node's own
/// Xcode and with each Xcode an action can name (the oldest and the newest here),
/// whose lookups no earlier, unsandboxed call has cached. The fastest of [`CALLS`]
/// sandboxed `cc` calls must take at most twice the fastest of the same action's
/// unsandboxed `cc` calls, run after it, plus [`SHIM_MARGIN`]; with the node's own
/// Xcode, also at most twice the fastest `clang` called directly plus [`SHIM_MARGIN`].
/// The fastest call, not the total: one call slowed by the shared runner failed a
/// bound on the total of five (0.485 s against 0.46 s on `main`).
/// It runs with no other developer tool running in this binary ([`DEVELOPER_TOOLS`]):
/// with them, `xcrun`'s own cache is lost to their rewrites whatever the sandbox does.
#[tokio::test]
async fn the_compiler_shims_print_nothing_on_stderr() {
    use std::io::Write as _;
    // Alone: no other test's `xcrun` calls drop this one's cache entries meanwhile.
    let _tools = DEVELOPER_TOOLS.write().await;
    let dir = scratch("shims");
    let mut config: NativeConfig = config(&dir);
    let all = xcodes();
    let mut picked: Vec<String> = all.keys().next().into_iter().cloned().collect();
    picked.extend(
        all.keys()
            .last()
            .filter(|b| picked.first() != Some(*b))
            .cloned(),
    );
    config.xcodes = all;
    let mut open = config.clone();
    open.scratch = dir.join("unsandboxed");
    open.isolation = kbf_driver_native::network::Isolation::None;
    let cas = Arc::new(MemoryCas::default());
    let (rt, open) = (runtime(config, &cas), runtime(open, &cas));
    let mut cases: Vec<Option<String>> = vec![None];
    cases.extend(picked.into_iter().map(Some));
    for (seq, build) in cases.iter().enumerate() {
        let mut spec = Spec::sh(SHIMS)
            .env("PATH", PATH)
            .env("CALLS", &CALLS.to_string());
        if let Some(build) = build {
            spec = spec.property("xcode", build);
        }
        let seq = u64::try_from(seq).expect("small") + 1;
        let result = run_long(&rt, &cas, seq, &spec).await;
        let (said, shim, direct) = shim_times(&stdout(&cas, &result));
        let control = run_long(&open, &cas, seq, &spec).await;
        let (_, open_shim, _) = shim_times(&stdout(&cas, &control));
        let (shim_min, open_min, direct_min) =
            (fastest(&shim), fastest(&open_shim), fastest(&direct));
        let times = format!(
            "fastest call: cc {shim_min}s, unsandboxed cc {open_min}s, clang {direct_min}s; \
             all: cc {shim:?}, unsandboxed cc {open_shim:?}, clang {direct:?}"
        );
        // Past the test harness's capture, so that every run's times are in the log:
        // they are what SHIM_MARGIN is set from.
        let _ = writeln!(std::io::stderr(), "shim times: xcode={build:?} {times}");
        assert_eq!(
            (said.as_str(), stderr(&cas, &result).as_str()),
            ("", ""),
            "xcode={build:?} {times}\n{}",
            sandbox_denials()
        );
        assert!(
            shim_min <= 2.0 * open_min + SHIM_MARGIN,
            "xcode={build:?} {times}"
        );
        if build.is_none() {
            assert!(
                shim_min <= 2.0 * direct_min + SHIM_MARGIN,
                "xcode={build:?} {times}"
            );
        }
    }
}
/// Catches the real temporary folder's leftovers kept past the lease that left them:
/// the action makes one in `TemporaryItems`, as a save it was killed in the middle of
/// would, old enough to sweep; the next lease must not find it.
#[tokio::test]
async fn a_leftover_in_the_temporary_folder_is_swept_after_the_lease() {
    let dir = scratch("leftover");
    let cas = Arc::new(MemoryCas::default());
    let rt = runtime(config(&dir), &cas);
    let left = user_folder("DARWIN_USER_TEMP_DIR")
        .join("TemporaryItems")
        .join(format!("kbf-leftover-{}", std::process::id()));
    let spec = Spec::sh(&format!(
        "mkdir -p '{0}' && echo x > '{0}/f' && touch -t 200001010000 '{0}' && echo made",
        left.display()
    ))
    .env("PATH", PATH);
    let result = run_long(&rt, &cas, 1, &spec).await;
    assert_eq!(
        stdout(&cas, &result).trim(),
        "made",
        "{}\n{}",
        stderr(&cas, &result),
        sandbox_denials()
    );
    assert!(!left.exists(), "kept past the lease: {}", left.display());
}

/// Each way an action could take `$T/TemporaryItems` away, swap it for a link, or
/// write outside it through a name below it: a name, and the command whose exit
/// status says whether it worked. `$T/victim` holds `orig` before each.
const ESCAPES: [(&str, &str); 8] = [
    ("rmdir", r#"rmdir "$T/TemporaryItems""#),
    ("rmdir-slash", r#"rmdir "$T/TemporaryItems/""#),
    ("mv", r#"mv "$T/TemporaryItems" "$T/moved""#),
    ("ln-over", r#"ln -sfF "$PWD" "$T/TemporaryItems""#),
    ("dotdot", r#"echo x > "$T/TemporaryItems/../dotdot""#),
    (
        "symlink-through",
        r#"ln -s "$T/escape" "$T/TemporaryItems/l" && echo x > "$T/TemporaryItems/l""#,
    ),
    (
        "hardlink-through",
        r#"ln "$T/victim" "$T/TemporaryItems/h" && echo changed > "$T/TemporaryItems/h""#,
    ),
    ("below", r#"mkdir "$T/TemporaryItems/inside""#),
];

/// Whether `T` (a stand-in after an attempt) is as it was, but for what an action may
/// make below `TemporaryItems`: the folder a real directory, `victim` unchanged, and
/// nothing new beside them.
fn untouched(temp: &Path) -> bool {
    let items = std::fs::symlink_metadata(temp.join("TemporaryItems"));
    let victim = std::fs::read_to_string(temp.join("victim"));
    let mut names: Vec<String> = std::fs::read_dir(temp)
        .expect("T")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    items.is_ok_and(|m| m.file_type().is_dir())
        && victim.is_ok_and(|v| v == "orig")
        && names == ["TemporaryItems", "victim"]
}

/// Catches a profile that lets an action write `TemporaryItems` itself, not only what
/// is below it (the review of issue #163: an action could remove it and put a symlink
/// in its place, and the daemon's sweep, outside the sandbox, would follow it), and one
/// that lets a name below it reach outside: `..`, a symlink, a hard link to a file of
/// the daemon's user. Each attempt runs on fresh stand-in folders in the scratch
/// directory, made by the daemon at start as it makes the real one, sandboxed and,
/// as the control that shows the attempt can work, unsandboxed. Only a write below the
/// folder works sandboxed.
#[tokio::test]
async fn temporary_items_itself_cannot_be_removed_or_replaced() {
    let dir = std::fs::canonicalize(scratch("items-itself")).expect("real");
    let cas = Arc::new(MemoryCas::default());
    let mut got = Vec::new();
    let mut want = Vec::new();
    for (name, attempt) in ESCAPES {
        let mut outcome = Vec::new();
        for sandboxed in [true, false] {
            let base = dir
                .join(name)
                .join(if sandboxed { "sandboxed" } else { "control" });
            let (temp, cache) = (base.join("T"), base.join("C"));
            std::fs::create_dir_all(&temp).expect("T");
            std::fs::create_dir_all(&cache).expect("C");
            std::fs::write(temp.join("victim"), "orig").expect("victim");
            let mut config: NativeConfig = config(&base);
            config.user_folders = UserFolders::new(temp.clone(), cache);
            if !sandboxed {
                config.isolation = kbf_driver_native::network::Isolation::None;
            }
            let rt = runtime(config, &cas);
            assert!(untouched(&temp), "{name}: not as made at start");
            let spec = Spec::sh(&format!("( {attempt} ) 2>/dev/null; echo $?"))
                .env("PATH", PATH)
                .env("T", &temp.display().to_string());
            let result = run_long(&rt, &cas, 1, &spec).await;
            let worked = stdout(&cas, &result).trim() == "0";
            outcome.push((worked, untouched(&temp)));
        }
        got.push((name, outcome));
        // Sandboxed: refused and T as it was; unsandboxed: done, T changed. A write
        // below the folder works both ways and leaves T as it was. A folder the
        // control removed is made again by the sweep after its lease, so T looks as
        // it was; the control's exit status is the proof there.
        let below = name == "below";
        let remade = name.starts_with("rmdir");
        want.push((name, vec![(below, true), (true, below || remade)]));
    }
    assert_eq!(got, want, "{}", sandbox_denials());
}

/// Catches a sweep that takes a `TemporaryItems` the daemon's user does not own: here
/// the stand-in folder is made root's (and writable by all, so a sweep that took it
/// could remove the old leftover in it). Needs passwordless `sudo`, which the CI
/// runners have; elsewhere it says so and checks nothing.
#[tokio::test]
async fn a_temporary_items_of_another_user_is_not_swept() {
    if !std::process::Command::new("sudo")
        .args(["-n", "true"])
        .status()
        .is_ok_and(|s| s.success())
    {
        assert!(
            std::env::var_os("GITHUB_ACTIONS").is_none(),
            "the CI runner has passwordless sudo"
        );
        eprintln!("no passwordless sudo: nothing to check");
        return;
    }
    let dir = std::fs::canonicalize(scratch("items-owner")).expect("real");
    let (temp, cache) = (dir.join("T"), dir.join("C"));
    let items = temp.join("TemporaryItems");
    let left = items.join("old");
    std::fs::create_dir_all(&left).expect("left");
    std::fs::create_dir_all(&cache).expect("C");
    let old = std::process::Command::new("touch")
        .args(["-t", "200001010000"])
        .arg(&left)
        .status()
        .expect("touch");
    assert!(old.success());
    let sudo = |args: &[&str]| {
        let status = std::process::Command::new("sudo")
            .arg("-n")
            .args(args)
            .arg(&items)
            .status()
            .expect("sudo");
        assert!(status.success(), "sudo {args:?}");
    };
    sudo(&["chown", "0:0"]);
    sudo(&["chmod", "777"]);
    let mut config: NativeConfig = config(&dir);
    config.user_folders = UserFolders::new(temp, cache);
    let cas = Arc::new(MemoryCas::default());
    let _rt = runtime(config, &cas);
    let kept = left.exists();
    // The folder back to this user so the scratch can be removed.
    let me = std::process::Command::new("id")
        .arg("-u")
        .output()
        .expect("id");
    let me = String::from_utf8(me.stdout).expect("uid");
    sudo(&["chown", me.trim()]);
    assert!(kept, "swept a TemporaryItems owned by root");
}
