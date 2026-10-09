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

/// The newest Xcode on this runner, if it has one.
fn newest_xcode() -> Option<(String, PathBuf)> {
    xcode::discover(
        Path::new(xcode::APPLICATIONS),
        Path::new(xcode::XCODEBUILD),
        xcode::ANSWER_WITHIN,
    )
    .into_iter()
    .last()
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

/// What [`the_compiler_shims_print_nothing_on_stderr`] runs: each shim's `--version`
/// with its stderr (but the version line `swiftc` always prints there) copied to
/// stdout, then, as the last line, the seconds five `cc --version` take and the
/// seconds five of the `clang` it runs take, called directly.
const SHIMS: &str = r#"for tool in cc clang swiftc; do
  $tool --version > /dev/null 2> "err-$tool" || echo "$tool failed"
done
cat err-cc err-clang; grep -v '^swift-driver version' err-swiftc
direct=$(xcrun --find clang) || exit 1
perl -MTime::HiRes=time -e '
  sub t { my $s = time; for (1..5) { system("$_[0] --version > /dev/null 2>&1") == 0 or die "$_[0]" } time - $s }
  my ($shim, $direct) = (t("cc"), t($ARGV[0]));
  printf "%.3f %.3f\n", $shim, $direct;
' "$direct"
"#;

/// The output of [`SHIMS`] but its last line, and the two times on that line.
fn shim_times(out: &str) -> (String, f64, f64) {
    let (rest, last) = out
        .trim_end()
        .rsplit_once('\n')
        .unwrap_or(("", out.trim_end()));
    let times: Vec<f64> = last
        .split(' ')
        .map(|t| t.parse().unwrap_or_else(|_| panic!("times in {out:?}")))
        .collect();
    (rest.to_owned(), times[0], times[1])
}

/// Catches the `/usr/bin` compiler shims failing to write `xcrun`'s cache in the
/// user's temporary folder: every `cc`, `clang` or `swiftc` call then prints
/// "couldn't create cache file" and runs several times slower (0.31 s a call against
/// 0.065 s for `clang` called directly, in issue #163). Checked with the node's own
/// Xcode and with each Xcode an action can name (the oldest and the newest here),
/// whose lookups no earlier, unsandboxed call has cached. The sandboxed `cc` must take
/// at most twice as long as the same action's unsandboxed `cc`, run after it, plus a
/// quarter second; with the node's own Xcode, also at most twice `clang` called
/// directly plus a quarter second.
#[tokio::test]
async fn the_compiler_shims_print_nothing_on_stderr() {
    let dir = scratch("shims");
    let mut config: NativeConfig = config(&dir);
    let all = xcode::discover(
        Path::new(xcode::APPLICATIONS),
        Path::new(xcode::XCODEBUILD),
        xcode::ANSWER_WITHIN,
    );
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
        let mut spec = Spec::sh(SHIMS).env("PATH", PATH);
        if let Some(build) = build {
            spec = spec.property("xcode", build);
        }
        let seq = u64::try_from(seq).expect("small") + 1;
        let result = run_long(&rt, &cas, seq, &spec).await;
        let (said, shim, direct) = shim_times(&stdout(&cas, &result));
        let control = run_long(&open, &cas, seq, &spec).await;
        let (_, open_shim, _) = shim_times(&stdout(&cas, &control));
        let times = format!("cc {shim}s, unsandboxed cc {open_shim}s, clang {direct}s");
        assert_eq!(
            (said.as_str(), stderr(&cas, &result).as_str()),
            ("", ""),
            "xcode={build:?} {times}\n{}",
            sandbox_denials()
        );
        assert!(shim <= 2.0 * open_shim + 0.25, "xcode={build:?} {times}");
        if build.is_none() {
            assert!(shim <= 2.0 * direct + 0.25, "xcode={build:?} {times}");
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

/// What [`temporary_items_itself_cannot_be_removed_or_replaced`] runs: each way to take
/// `$T/TemporaryItems` away or swap it for a link (to the lease's working directory),
/// then a write below it, each with its exit status.
const REPLACE_ITEMS: &str = r#"rmdir "$T/TemporaryItems" 2>/dev/null; echo "rmdir=$?"
mv "$T/TemporaryItems" "$T/moved" 2>/dev/null; echo "mv=$?"
ln -sfF "$PWD" "$T/TemporaryItems" 2>/dev/null; echo "ln=$?"
mkdir "$T/TemporaryItems/inside"; echo "inside=$?"
"#;

/// Catches a profile that lets an action write `TemporaryItems` itself, not only what
/// is below it (the review of issue #163): an action could remove it and put a
/// symlink in its place, and the daemon's sweep, outside the sandbox, would follow it.
/// The folders are stand-ins in the scratch directory, made by the daemon at start as
/// it makes the real one. Control: the same action unsandboxed removes and replaces
/// the folder, so the check can fail.
#[tokio::test]
async fn temporary_items_itself_cannot_be_removed_or_replaced() {
    let dir = std::fs::canonicalize(scratch("items-itself")).expect("real");
    let cas = Arc::new(MemoryCas::default());
    let mut runs = Vec::new();
    for sandboxed in [true, false] {
        let base = dir.join(if sandboxed { "sandboxed" } else { "control" });
        let (temp, cache) = (base.join("T"), base.join("C"));
        std::fs::create_dir_all(&temp).expect("T");
        std::fs::create_dir_all(&cache).expect("C");
        let mut config: NativeConfig = config(&base);
        config.user_folders = UserFolders::new(temp.clone(), cache);
        if !sandboxed {
            config.isolation = kbf_driver_native::network::Isolation::None;
        }
        let rt = runtime(config, &cas);
        let items = temp.join("TemporaryItems");
        assert!(items.is_dir(), "not made at start: {}", items.display());
        let spec = Spec::sh(REPLACE_ITEMS)
            .env("PATH", PATH)
            .env("T", &temp.display().to_string());
        let result = run_long(&rt, &cas, 1, &spec).await;
        let kind = std::fs::symlink_metadata(&items).map(|m| m.file_type().is_dir());
        runs.push((stdout(&cas, &result), kind.ok()));
    }
    assert_eq!(
        runs,
        [
            (
                "rmdir=1\nmv=1\nln=1\ninside=0\n".to_owned(),
                Some(true)
            ),
            (
                "rmdir=0\nmv=1\nln=0\ninside=0\n".to_owned(),
                Some(false)
            ),
        ],
        "{}",
        sandbox_denials()
    );
}
