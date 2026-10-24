//! A keychain item one lease stores is not readable by the next lease on the same slot.
//!
//! Lease 1 runs a small Swift probe (`keychain_probe.swift`, compiled with `/usr/bin/swiftc`
//! before the leases) that stores a generic-password item in the user's data-protection
//! keychain with `SecItemAdd`, with the accessibility after first unlock and with the
//! default one, and prints `ADDED <variant> <OSStatus>`. The slot's clean runs. Lease 2, on
//! the same uid, runs the probe in read mode (`SecItemCopyMatching`) and prints `FOUND` or
//! `NOT_FOUND` with the status. The test asserts there is no `FOUND`.
//!
//! It prints both lines and which entries of `Library/Keychains` exist right after the
//! clean (the per-user keychain `secd` makes holds a UUID folder there). If `SecItemAdd`
//! itself fails in a pool user's context, the test fails with that status: that is a
//! finding too (except `errSecMissingEntitlement`, which says the probe, signed ad hoc, has no
//! access to the data-protection keychain: printed as `FINDING`, and the read still runs).

use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use kbf_driver_native::NativeRuntime;

use super::assert_root;
use crate::support::pool::{Scratch, Users, config, home_of};
use crate::support::{MemoryCas, Spec, stderr, stdout};

const PROBE_SOURCE: &str = include_str!("keychain_probe.swift");

/// Compiles the probe into `scratch` (readable and executable by the pool user).
fn compile(scratch: &Scratch) -> String {
    let dir: &Path = &scratch.0;
    let source = dir.join("keychain_probe.swift");
    std::fs::write(&source, PROBE_SOURCE).expect("source");
    let binary = dir.join("keychain-probe");
    let built = Command::new("/usr/bin/swiftc")
        .args(["-O", "-o"])
        .arg(&binary)
        .arg(&source)
        .env("TMPDIR", dir)
        .output()
        .expect("swiftc");
    assert!(
        built.status.success(),
        "swiftc failed: {}",
        String::from_utf8_lossy(&built.stderr)
    );
    binary.to_string_lossy().into_owned()
}

/// The lines of `text` that start with `word`.
fn lines<'a>(text: &'a str, word: &str) -> Vec<&'a str> {
    text.lines().filter(|l| l.starts_with(word)).collect()
}

#[ignore = "needs root and real users: cargo xtask sudo-test -p kbf-driver-native --test slot_clean"]
#[tokio::test]
async fn a_keychain_item_one_lease_stores_is_not_readable_by_the_next() {
    assert_root();
    let name = "keychain";
    let scratch = Scratch::new(name);
    let users = Users::new(name, 1);
    let user = users.0[0].clone();
    let probe = compile(&scratch);
    let cas = Arc::new(MemoryCas::default());
    let rt = NativeRuntime::new(config(&scratch, users.pool()), Arc::clone(&cas)).expect("runtime");
    let service = format!("kbf-keychain-{}", std::process::id());

    let first = crate::support::run(&rt, &cas, 1, &Spec::argv(&[&probe, "add", &service]))
        .await
        .expect("lease 1 ran");
    let (a_out, a_err) = (stdout(&cas, &first), stderr(&cas, &first));
    eprintln!("keychain lease 1: exit {}\n{a_out}{a_err}", first.exit_code);
    // Right after the clean that follows lease 1.
    let keychains = home_of(&user).join("Library/Keychains");
    let after_clean: Vec<String> = std::fs::read_dir(&keychains)
        .map(|names| {
            names
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    eprintln!("keychain after the clean: Library/Keychains = {after_clean:?}");

    let second = crate::support::run(&rt, &cas, 2, &Spec::argv(&[&probe, "read", &service]))
        .await
        .expect("lease 2 ran");
    let (b_out, b_err) = (stdout(&cas, &second), stderr(&cas, &second));
    eprintln!(
        "keychain lease 2: exit {}\n{b_out}{b_err}",
        second.exit_code
    );

    let added: Vec<&str> = lines(&a_out, "ADDED");
    assert_eq!(first.exit_code, 0, "lease 1: {a_out}{a_err}");
    // `errSecMissingEntitlement` (-34018): a process with no keychain entitlements (this
    // probe is ad-hoc signed) cannot use the data-protection keychain at all, so no item of
    // it can cross to the next lease. Said loudly, not hidden; any other failure of every
    // `SecItemAdd` is a finding that fails the test.
    let stored = added.iter().any(|l| l.ends_with(" 0"));
    // -34018 and the statuses of a user with no file keychain to add to (-25307, -25308,
    // -25291, -25294, and -60008 seen on the hosted runner).
    let unreachable = ["-34018", "-25307", "-25308", "-25291", "-25294", "-60008"];
    let no_entitlement = !added.is_empty()
        && added
            .iter()
            .all(|l| unreachable.iter().any(|s| l.ends_with(&format!(" {s}"))));
    if no_entitlement {
        eprintln!(
            "FINDING: SecItemAdd stores nothing for this pool user's process (no keychain \
             entitlements, or no file keychain): {added:?}"
        );
    }
    assert!(
        stored || no_entitlement,
        "SecItemAdd failed in the pool user's context (a finding): {added:?}\n{a_out}{a_err}"
    );
    assert_eq!(second.exit_code, 0, "lease 2: {b_out}{b_err}");
    assert_eq!(
        lines(&b_out, "NOT_FOUND").len(),
        added.len(),
        "lease 2 read every variant: {b_out}{b_err}"
    );
    assert!(
        lines(&b_out, "FOUND").is_empty(),
        "an item lease 1 stored is readable by lease 2: {b_out}\nadded {added:?}\nafter the clean {after_clean:?}"
    );
}
