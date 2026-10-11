//! Throwaway probe (never merged): what the hosted macOS runner shows of CoreDevice,
//! remoted and usbmuxd, and which sandbox rules refuse them.

#![cfg(target_os = "macos")]

#[test]
fn probe() {
    let dir = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("probe-i1");
    std::fs::create_dir_all(&dir).expect("dir");
    let base = dir.join("base.sb");
    std::fs::write(&base, kbf_driver_native::network::BASE_PROFILE).expect("base");
    let script = dir.join("probe.sh");
    std::fs::write(&script, include_str!("aa_probe.sh")).expect("script");
    let out = std::process::Command::new("/bin/bash")
        .arg(&script)
        .arg(&base)
        .arg(dir.join("lease"))
        .output()
        .expect("bash");
    panic!(
        "PROBE OUTPUT\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}
