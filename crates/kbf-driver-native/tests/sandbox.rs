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
/// directory outside every lease holding a file `victim`; `{LEASE}` is the same in
/// another lease's directory under the scratch root (an output that lease is about to
/// upload); `{TMP}` is a path in `/tmp`; `{DOMAIN}` is a preferences domain no one
/// uses. The hard links write through a path inside the lease to a file outside it
/// (`file-link` is not one of `file-write*`). `defaults write` asks `cfprefsd`, which
/// writes the user's `~/Library/Preferences` on the action's behalf
/// (`user-preference-write`, also not one of `file-write*`), so one action could leave
/// settings, such as a tool's defaults, for the next.
const OUTSIDE: [(&str, &str); 11] = [
    ("create", "echo x > {OUT}/new"),
    ("append", "echo x >> {OUT}/victim"),
    ("unlink", "rm -f {OUT}/victim"),
    ("rename", "mv {OUT}/victim {OUT}/moved"),
    ("chmod", "chmod 600 {OUT}/victim"),
    ("mkdir", "mkdir {OUT}/dir"),
    ("xattr", "xattr -w kbf.test x {OUT}/victim"),
    ("tmp", "echo x > {TMP}"),
    ("link", "ln {OUT}/victim ./h && echo x >> ./h"),
    ("link-lease", "ln {LEASE}/victim ./l && echo x >> ./l"),
    ("defaults", "defaults write {DOMAIN} k v"),
];

/// Where the rows of [`OUTSIDE`] aim.
struct Targets {
    out: PathBuf,
    lease: PathBuf,
    tmp: PathBuf,
    domain: String,
}

impl Targets {
    /// Fresh targets in `dir`, whose scratch root is `dir/leases` ([`config`]): both
    /// victims hold `kept`, and neither `tmp` nor `domain` exists.
    fn fresh(dir: &Path, tmp: &Path, domain: &str) -> Self {
        let _ = std::fs::remove_file(tmp);
        forget(domain);
        Self {
            out: victim_in(&dir.join("outside")),
            lease: victim_in(&dir.join("leases").join("lease-9-999").join("out")),
            tmp: tmp.to_owned(),
            domain: domain.to_owned(),
        }
    }

    /// Puts a fresh `victim` back in both directories.
    fn rearm(&self) {
        for dir in [&self.out, &self.lease] {
            std::fs::write(dir.join("victim"), "kept\n").expect("victim");
        }
    }
}

/// The value of `k` in `domain` as the test's own user reads it, if any: what an
/// action that got a preference write through left behind.
fn preference(domain: &str) -> Option<String> {
    let out = std::process::Command::new("/usr/bin/defaults")
        .args(["read", domain, "k"])
        .output()
        .expect("defaults read");
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// Removes `domain` from the test user's preferences.
fn forget(domain: &str) {
    let _ = std::process::Command::new("/usr/bin/defaults")
        .args(["delete", domain])
        .output();
}

/// A script that runs each of `ways` and prints `name=<exit status>` for each.
fn attempts(ways: &[(&str, &str)], to: &Targets) -> String {
    ways.iter()
        .map(|(name, command)| {
            let command = command
                .replace("{OUT}", &to.out.to_string_lossy())
                .replace("{LEASE}", &to.lease.to_string_lossy())
                .replace("{TMP}", &to.tmp.to_string_lossy())
                .replace("{DOMAIN}", &to.domain);
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

/// `dir`, made afresh, with a file `victim` holding `kept`.
fn victim_in(dir: &Path) -> PathBuf {
    if dir.exists() {
        kbf_outputs::remove_tree(dir).expect("clear the victim's directory");
    }
    std::fs::create_dir_all(dir).expect("the victim's directory");
    std::fs::write(dir.join("victim"), "kept\n").expect("victim");
    dir.to_owned()
}

/// The names in `dir`.
fn names(dir: &Path) -> Vec<std::ffi::OsString> {
    std::fs::read_dir(dir)
        .expect("a directory")
        .map(|e| e.expect("entry").file_name())
        .collect()
}

/// Catches an action, with or without the network, that can create, append to,
/// remove, rename, chmod or tag a file outside its lease directory, write in `/tmp`,
/// change a file outside it (another lease's output included) through a hard link
/// made inside it, or write the user's preferences: a tool that turned its own sandbox
/// off (`swift build --disable-sandbox`) could then change other leases, the daemon's
/// files, the node, or the settings the next action runs with. Control: the same
/// action without a sandbox does every one of those writes.
#[tokio::test]
async fn no_action_writes_outside_its_lease() {
    let dir = scratch("writes");
    let cas = Arc::new(MemoryCas::default());
    let sandboxed = config(&dir);
    assert!(matches!(sandboxed.isolation, Isolation::Sandbox(_)));
    let mut open = config(&dir.join("unsandboxed"));
    open.isolation = Isolation::None;
    // The runtimes first: a start sweeps the scratch root, the other lease included.
    let (sandboxed, open) = (runtime(sandboxed, &cas), runtime(open, &cas));
    let tmp = PathBuf::from(format!("/tmp/kbf-sandbox-test-{}", std::process::id()));
    let domain = format!("kbf.sandbox-test.{}", std::process::id());

    let to = Targets::fresh(&dir, &tmp, &domain);
    let script = attempts(&OUTSIDE, &to);
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
        for victim in [&to.out, &to.lease] {
            assert_eq!(names(victim), ["victim"], "network={network}");
            assert_eq!(
                std::fs::read_to_string(victim.join("victim")).expect("victim"),
                "kept\n",
                "network={network} changed {}",
                victim.display()
            );
        }
        assert!(!tmp.exists(), "network={network} wrote {}", tmp.display());
        assert_eq!(
            preference(&domain),
            None,
            "network={network} wrote {domain}"
        );
    }

    // The control: unsandboxed, every write goes through, so the refusals above are
    // the sandbox's. Each row runs on fresh victims (`unlink` takes the first); the
    // links must have reached their victims, and the preference must be there for the
    // test's own user to read back.
    for (seq, (name, command)) in OUTSIDE.iter().enumerate() {
        to.rearm();
        let script = attempts(&[(name, command)], &to);
        let spec = Spec::sh(&script).env("PATH", PATH);
        let seq = 10 + u64::try_from(seq).expect("small");
        let result = run(&open, &cas, seq, &spec).await.expect("ran");
        assert_eq!(
            statuses(&stdout(&cas, &result)),
            [((*name).to_owned(), 0)],
            "control: {name} fails even unsandboxed: {}",
            stderr(&cas, &result)
        );
        let reached = match *name {
            "link" => Some(&to.out),
            "link-lease" => Some(&to.lease),
            _ => None,
        };
        if let Some(victim) = reached {
            assert_eq!(
                std::fs::read_to_string(victim.join("victim")).expect("victim"),
                "kept\nx\n",
                "control: {name} did not write through its link"
            );
        }
    }
    assert_eq!(
        preference(&domain).as_deref(),
        Some("v"),
        "control: {domain}"
    );
    forget(&domain);
    let _ = std::fs::remove_file(&tmp);
}

/// Catches a profile that also refuses the writes an action must make: in its working
/// directory, an output in a subdirectory, `HOME`, `TMPDIR`, its caches (the clang
/// module cache), `/dev/null`, through `mktemp`, and through a hard link between two of
/// its own files.
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
        (
            "link",
            "echo x > linked && ln linked link && echo y >> link",
        ),
    ];
    let unused = Targets {
        out: PathBuf::from("/unused"),
        lease: PathBuf::from("/unused"),
        tmp: PathBuf::from("/unused"),
        domain: String::new(),
    };
    let script = attempts(&ways, &unused);
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
    let all: Vec<(String, PathBuf)> = xcode::discover(
        Path::new(xcode::APPLICATIONS),
        Path::new(xcode::XCODEBUILD),
        xcode::ANSWER_WITHIN,
    )
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
