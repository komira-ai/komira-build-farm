//! On macOS, an action writes only inside its lease directory (and `/dev`), with or
//! without the network; inside, its working directory, `HOME`, `TMPDIR` and caches are
//! writable; and a C compile with modules, a Swift compile and `xcodebuild` still work.
//! The Xcode an action names is the one its tools run.
//!
//! Each refusal is checked against a control on the same host: the same action run by
//! the same driver without a sandbox must succeed, or the test proves nothing.

#![cfg(target_os = "macos")]

mod support;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use kbf_daemon::Runtime;
use kbf_driver_native::network::Isolation;
use kbf_driver_native::xcode;
use kbf_driver_native::{NativeConfig, NativeRuntime};
use support::{MemoryCas, Spec, config, run, runtime, scratch, stderr, stdout, work};

/// The actions' `PATH`.
const PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin";

/// Writes an action tries outside its lease, as `name command` pairs. `{OUT}` is a
/// directory outside every lease holding a file `victim`; `{TMP}` is a path in `/tmp`.
const OUTSIDE: [(&str, &str); 8] = [
    ("create", "echo x > {OUT}/new"),
    ("append", "echo x >> {OUT}/victim"),
    ("unlink", "rm -f {OUT}/victim"),
    ("rename", "mv {OUT}/victim {OUT}/moved"),
    ("chmod", "chmod 600 {OUT}/victim"),
    ("mkdir", "mkdir {OUT}/dir"),
    ("xattr", "xattr -w kbf.test x {OUT}/victim"),
    ("tmp", "echo x > {TMP}"),
];

/// A script that runs each of `ways` and prints `name=<exit status>` for each.
fn attempts(ways: &[(&str, &str)], out: &Path, tmp: &Path) -> String {
    ways.iter()
        .map(|(name, command)| {
            let command = command
                .replace("{OUT}", &out.to_string_lossy())
                .replace("{TMP}", &tmp.to_string_lossy());
            format!("{command} 2>/dev/null; echo {name}=$?; ")
        })
        .collect()
}

/// The `name=status` lines of `text`, as pairs.
fn statuses(text: &str) -> Vec<(String, i32)> {
    text.lines()
        .filter_map(|line| line.split_once('='))
        .map(|(name, status)| (name.to_owned(), status.parse().expect("a status")))
        .collect()
}

/// A directory outside every lease with a file `victim` holding `kept`.
fn outside(dir: &Path) -> PathBuf {
    let out = dir.join("outside");
    if out.exists() {
        kbf_outputs::remove_tree(&out).expect("clear outside");
    }
    std::fs::create_dir_all(&out).expect("outside");
    std::fs::write(out.join("victim"), "kept\n").expect("victim");
    out
}

/// Catches an action, with or without the network, that can create, append to,
/// remove, rename, chmod or tag a file outside its lease directory, or write in
/// `/tmp`: a tool that turned its own sandbox off (`swift build --disable-sandbox`)
/// could then change other leases, the daemon's files or the node. Control: the same
/// action without a sandbox does every one of those writes.
#[tokio::test]
async fn no_action_writes_outside_its_lease() {
    let dir = scratch("writes");
    let cas = Arc::new(MemoryCas::default());
    let sandboxed = config(&dir);
    assert!(matches!(sandboxed.isolation, Isolation::Sandbox(_)));
    let mut open = config(&dir.join("unsandboxed"));
    open.isolation = Isolation::None;
    let (sandboxed, open) = (runtime(sandboxed, &cas), runtime(open, &cas));
    let tmp = PathBuf::from(format!("/tmp/kbf-sandbox-test-{}", std::process::id()));
    let _ = std::fs::remove_file(&tmp);

    let out = outside(&dir);
    let script = attempts(&OUTSIDE, &out, &tmp);
    for (seq, network) in [(1, "off"), (2, "on")] {
        let spec = Spec::sh(&script)
            .env("PATH", PATH)
            .property("network", network);
        let result = run(&sandboxed, &cas, seq, &spec).await.expect("ran");
        let seen = statuses(&stdout(&cas, &result));
        assert_eq!(seen.len(), OUTSIDE.len(), "{}", stderr(&cas, &result));
        for (name, status) in seen {
            assert_ne!(
                status, 0,
                "an action with network={network} wrote outside its lease: {name}"
            );
        }
        let left: Vec<_> = std::fs::read_dir(&out)
            .expect("outside")
            .map(|e| e.expect("entry").file_name())
            .collect();
        assert_eq!(left, ["victim"], "network={network}");
        assert_eq!(
            std::fs::read_to_string(out.join("victim")).expect("victim"),
            "kept\n"
        );
        assert!(!tmp.exists(), "network={network} wrote {}", tmp.display());
    }

    // The control: unsandboxed, every write goes through, so the refusals above are
    // the sandbox's. `rename` runs on a fresh victim after `unlink` took the first.
    let out = outside(&dir);
    for (seq, (name, command)) in OUTSIDE.iter().enumerate() {
        std::fs::write(out.join("victim"), "kept\n").expect("victim");
        let script = attempts(&[(name, command)], &out, &tmp);
        let spec = Spec::sh(&script).env("PATH", PATH);
        let seq = 10 + u64::try_from(seq).expect("small");
        let result = run(&open, &cas, seq, &spec).await.expect("ran");
        assert_eq!(
            statuses(&stdout(&cas, &result)),
            [((*name).to_owned(), 0)],
            "control: {name} fails even unsandboxed: {}",
            stderr(&cas, &result)
        );
    }
    let _ = std::fs::remove_file(&tmp);
}

/// Catches a profile that also refuses the writes an action must make: in its working
/// directory, an output in a subdirectory, `HOME`, `TMPDIR`, its caches (the clang
/// module cache), `/dev/null`, and through `mktemp`.
#[tokio::test]
async fn an_action_writes_inside_its_lease() {
    let dir = scratch("inside");
    let config = config(&dir);
    let cas = Arc::new(MemoryCas::default());
    let rt = runtime(config, &cas);
    let ways = [
        ("work", "echo x > here"),
        ("output", "mkdir -p out/sub && echo x > out/sub/f"),
        (
            "home",
            "mkdir -p \"$HOME/Library/Caches\" && echo x > \"$HOME/Library/Caches/c\"",
        ),
        ("tmpdir", "echo x > \"$TMPDIR/t\""),
        ("mktemp", "mktemp \"$TMPDIR/kbf.XXXXXX\" > /dev/null"),
        ("cache", "echo x > \"$XDG_CACHE_HOME/c\""),
        ("modules", "echo x > \"$CLANG_MODULE_CACHE_PATH/m\""),
        ("devnull", "echo x > /dev/null"),
    ];
    let script = attempts(&ways, Path::new("/unused"), Path::new("/unused"));
    for (seq, network) in [(1, "off"), (2, "on")] {
        let spec = Spec::sh(&script)
            .env("PATH", PATH)
            .property("network", network)
            .outputs(&["out"]);
        let result = run(&rt, &cas, seq, &spec).await.expect("ran");
        let seen = statuses(&stdout(&cas, &result));
        assert_eq!(seen.len(), ways.len(), "{}", stderr(&cas, &result));
        for (name, status) in seen {
            assert_eq!(status, 0, "network={network}: {name} was refused");
        }
        assert_eq!(result.output_directories.len(), 1, "network={network}");
    }
}

/// Runs `spec` as lease `1.seq` with up to five minutes: a cold Swift compile is slow.
async fn run_long(
    rt: &NativeRuntime<MemoryCas>,
    cas: &MemoryCas,
    seq: u64,
    spec: &Spec,
) -> kbf_proto::reapi::ActionResult {
    let action = spec.clone().timeout(Duration::from_secs(300)).store(cas);
    tokio::time::timeout(Duration::from_secs(330), rt.run(work(seq, action, 0)))
        .await
        .expect("the run ends")
        .expect("ran")
}

/// Catches a profile that breaks the compilers phase 1 runs: `cc` with clang modules
/// (it writes a module cache, which must land in the lease: the test finds the
/// `.pcm` files there) and `swiftc`, each building a program that runs.
#[tokio::test]
async fn compiles_still_work_and_their_caches_stay_in_the_lease() {
    let dir = scratch("compile");
    let config = config(&dir);
    let cas = Arc::new(MemoryCas::default());
    let rt = runtime(config, &cas);
    let c = "printf '#include <stdio.h>\\nint main(void) { printf(\"c\\\\n\"); return 0; }\\n' > hello.c \
             && cc -fmodules -o hello hello.c && ./hello \
             && find \"$CLANG_MODULE_CACHE_PATH\" -name '*.pcm' | grep -q . && echo pcm";
    let swift = "printf 'print(\"swift\")\\n' > hello.swift \
                 && xcrun swiftc -o hello hello.swift && ./hello";
    for (seq, (script, want)) in [(c, "c\npcm"), (swift, "swift")].into_iter().enumerate() {
        let spec = Spec::sh(script).env("PATH", PATH);
        let result = run_long(&rt, &cas, u64::try_from(seq).expect("small") + 1, &spec).await;
        assert_eq!(
            stdout(&cas, &result).trim(),
            want,
            "{}",
            stderr(&cas, &result)
        );
    }
}

/// The Xcodes on this runner, at most two: the lowest build and the highest.
fn some_xcodes() -> Vec<(String, PathBuf)> {
    let all: Vec<(String, PathBuf)> =
        xcode::discover(Path::new(xcode::APPLICATIONS), Path::new(xcode::XCODEBUILD))
            .into_iter()
            .collect();
    let mut picked: Vec<_> = all.first().into_iter().cloned().collect();
    if all.len() > 1 {
        picked.extend(all.last().cloned());
    }
    picked
}

/// Catches: an Xcode on the host not found (the node would not report it), the build
/// an action names not the one its tools run (`DEVELOPER_DIR` not set, or set to
/// another Xcode), the node report missing a build, and `xcodebuild` itself broken by
/// the sandbox.
#[tokio::test]
async fn the_xcode_an_action_names_is_the_one_it_runs() {
    let xcodes = some_xcodes();
    assert!(!xcodes.is_empty(), "no Xcode found in /Applications");
    let dir = scratch("xcode");
    let mut config: NativeConfig = config(&dir);
    config.xcodes = xcodes.iter().cloned().collect();
    let cas = Arc::new(MemoryCas::default());
    let rt = runtime(config, &cas);
    let reported = rt.capabilities();
    for (seq, (build, developer_dir)) in xcodes.iter().enumerate() {
        assert!(
            reported.contains(&("xcode".to_owned(), build.clone())),
            "{build} not in {reported:?}"
        );
        let spec = Spec::sh("echo \"$DEVELOPER_DIR\"; xcodebuild -version")
            .env("PATH", PATH)
            .env("DEVELOPER_DIR", "/the/commands/own")
            .property("xcode", build);
        let result = run_long(&rt, &cas, u64::try_from(seq).expect("small") + 1, &spec).await;
        let out = stdout(&cas, &result);
        assert_eq!(result.exit_code, 0, "{out}{}", stderr(&cas, &result));
        let mut lines = out.lines();
        assert_eq!(lines.next(), Some(developer_dir.to_string_lossy().as_ref()));
        assert_eq!(xcode::build_of(&out), Some(build.as_str()), "{out}");
    }
}
