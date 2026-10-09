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

/// Catches a sandbox under which `xcodebuild` (derived data in the lease) cannot
/// build a one-file package: it saves its log store atomically, through the user's
/// temporary folder, and exits 74 when that fails. The action points Foundation's
/// home into the lease (`CFFIXED_USER_HOME`): `xcodebuild` resolves a package with
/// SwiftPM's caches in `~/Library/Caches`, and finds `~` through the user database,
/// not `HOME`, so by default it writes outside the lease, which the sandbox refuses as
/// it refuses every other write there.
#[tokio::test]
async fn xcodebuild_builds_a_package() {
    let Some((build, developer_dir)) = newest_xcode() else {
        eprintln!("no Xcode on this runner: nothing to check");
        return;
    };
    let dir = scratch("xcodebuild");
    let mut config: NativeConfig = config(&dir);
    config.xcodes = [(build.clone(), developer_dir)].into_iter().collect();
    let cas = Arc::new(MemoryCas::default());
    let rt = runtime(config, &cas);
    let mut spec = Spec::sh(&format!(
        "{LAYOUT}CFFIXED_USER_HOME=\"$HOME\" xcodebuild -scheme hello \
         -destination platform=macOS -derivedDataPath \"$TMPDIR/dd\" build && echo built"
    ))
    .env("PATH", PATH)
    .property("xcode", &build);
    spec.inputs = package();
    let result = run_long(&rt, &cas, 1, &spec).await;
    let out = stdout(&cas, &result);
    assert_eq!(
        (result.exit_code, out.lines().last()),
        (0, Some("built")),
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
/// stdout, then five `cc --version` against five of the `clang` it runs, called
/// directly, printing the times when the shim takes more than twice as long and a
/// quarter second (an uncached lookup took 0.31 s a call against 0.065 s).
const SHIMS: &str = r#"for tool in cc clang swiftc; do
  $tool --version > /dev/null 2> "err-$tool" || echo "$tool failed"
done
cat err-cc err-clang; grep -v '^swift-driver version' err-swiftc
direct=$(xcrun --find clang) || exit 1
perl -MTime::HiRes=time -e '
  sub t { my $s = time; for (1..5) { system("$_[0] --version > /dev/null 2>&1") == 0 or die "$_[0]" } time - $s }
  my ($shim, $direct) = (t("cc"), t($ARGV[0]));
  printf "slow: cc %.3fs, clang %.3fs\n", $shim, $direct if $shim > 2 * $direct + 0.25;
' "$direct"
"#;

/// Catches the `/usr/bin` compiler shims failing to write `xcrun`'s cache in the
/// user's temporary folder: every `cc`, `clang` or `swiftc` call then prints
/// "couldn't create cache file" and runs several times slower. Checked with the
/// node's own Xcode and with each Xcode an action can name (the oldest and the newest
/// here), whose lookups no earlier, unsandboxed call has cached.
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
    let cas = Arc::new(MemoryCas::default());
    let rt = runtime(config, &cas);
    let mut cases: Vec<Option<String>> = vec![None];
    cases.extend(picked.into_iter().map(Some));
    for (seq, build) in cases.iter().enumerate() {
        let mut spec = Spec::sh(SHIMS).env("PATH", PATH);
        if let Some(build) = build {
            spec = spec.property("xcode", build);
        }
        let result = run_long(&rt, &cas, u64::try_from(seq).expect("small") + 1, &spec).await;
        assert_eq!(
            (
                stdout(&cas, &result).as_str(),
                stderr(&cas, &result).as_str()
            ),
            ("", ""),
            "xcode={build:?}\n{}",
            sandbox_denials()
        );
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
