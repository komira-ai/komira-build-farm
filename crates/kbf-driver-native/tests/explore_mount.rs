//! TEMPORARY exploration of komira-build-farm#178 and #183 (not to be merged).
#![cfg(target_os = "macos")]

mod support;

macro_rules! say {
    ($($t:tt)*) => {{
        use std::io::Write as _;
        let _ = writeln!(std::io::stderr(), $($t)*);
    }};
}

use std::process::Command;

use kbf_driver_native::mounts;
use support::scratch;

fn sh(label: &str, script: &str) {
    let out = Command::new("/bin/sh")
        .arg("-c")
        .arg(script)
        .output()
        .expect("sh");
    say!(
        "=== {label}: {}\n{}{}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn explore() {
    let dir = scratch("explore");
    let d = dir.display();

    // Which unmount routes work for a volume the same user mounted.
    sh(
        "make images",
        &format!(
            "cd '{d}' && for n in a b; do hdiutil create -size 8m -fs HFS+ -volname kbf$n img$n.dmg >/dev/null && mkdir -p m$n && hdiutil attach img$n.dmg -mountpoint '{d}'/m$n -nobrowse -noverify -noautoopen | tail -1; done; mount | grep '{d}'"
        ),
    );
    say!("by_syscall a: {:?}", mounts::by_syscall(&dir.join("ma")));
    sh("after syscall", &format!("mount | grep '{d}'"));
    say!("by_diskutil b: {:?}", mounts::by_diskutil(&dir.join("mb")));
    sh("after diskutil", &format!("mount | grep '{d}'"));

    // The Metal toolchain mount, made by xcodebuild below a home outside any driver.
    let home = dir.join("xhome");
    std::fs::create_dir_all(&home).expect("home");
    let h = home.display();
    sh(
        "xcodebuild with CFFIXED_USER_HOME",
        &format!(
            "cd '{d}' && mkdir -p pkg/Sources/hello && cd pkg && printf 'print(1)\\n' > Sources/hello/main.swift && printf '// swift-tools-version:5.9\\nimport PackageDescription\\nlet package = Package(name: \"hello\", targets: [.executableTarget(name: \"hello\", path: \"Sources/hello\")])\\n' > Package.swift && export CFFIXED_USER_HOME='{h}' && xcodebuild -showComponent MetalToolchain 2>&1 | tail -3; xcodebuild -scheme hello -destination 'platform=macOS' -derivedDataPath dd build 2>&1 | tail -3; mount | grep -i MetalToolchain/mounts"
        ),
    );
    sh(
        "owners along the chain",
        &format!(
            "ls -leOd '{h}/Library' '{h}/Library/Developer' '{h}/Library/Developer/DVTDownloads' '{h}/Library/Developer/DVTDownloads/MetalToolchain' '{h}/Library/Developer/DVTDownloads/MetalToolchain/mounts' 2>&1; id"
        ),
    );
    say!("unmount_below: {:?}", mounts::unmount_below(&home));
    sh("mounts left", &format!("mount | grep '{d}'"));
    say!(
        "remove_tree: {:?}",
        kbf_outputs::remove_tree(&home).map_err(|e| e.to_string())
    );
    sh("what is left", &format!("ls -leOR '{h}' 2>&1 | head -40"));

    // Does the Metal compiler take its module cache from anything a lease sets?
    sh(
        "metal cache path",
        &format!(
            "cd '{d}' && printf '#include <metal_stdlib>\\nkernel void k() {{}}\\n' > s.metal; for e in none CLANG_MODULE_CACHE_PATH=/zz/one TMPDIR=/zz/two HOME=/zz/three CFFIXED_USER_HOME=/zz/four; do echo \"-- $e\"; if [ $e = none ]; then xcrun metal -### -c s.metal -o s.air 2>&1; else env $e xcrun metal -### -c s.metal -o s.air 2>&1; fi | tr ' ' '\\n' | grep -i -E 'module-cache|ModuleCache|zz' | head -4; done; strings -a \"$(xcrun -f metal)\" | grep -E '^[A-Z][A-Z0-9_]{{5,}}$' | grep -i -E 'CACHE|MODULE|TMP|HOME|DIR' | sort -u | head -30"
        ),
    );
}
