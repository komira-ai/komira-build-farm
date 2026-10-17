use std::io;

use super::*;
use crate::fixtures::{FakeRunner, ran};

const T: Duration = Duration::from_secs(5);
const INFO: &str = "Executable=/x/kbf-daemon\nIdentifier=kbf-daemon\n\
    CodeDirectory v=20400 size=1 flags=0x10002(adhoc,runtime) hashes=1+2 location=embedded\n\
    CDHash=abcdef\n";

fn quarantine(answer: Ran) -> Result<bool, Refusal> {
    let runner = FakeRunner::new(move |_| Ok(answer.clone()));
    quarantined(&runner, Path::new("/inbox/kbf.tar.gz"), T)
}

fn codesign(verify: i32, display_stderr: &'static str) -> Result<(), Refusal> {
    let runner = FakeRunner::new(move |s| {
        let verify_run = s.args[0] == "--verify";
        Ok(ran(
            if verify_run { verify } else { 0 },
            "",
            if verify_run { "" } else { display_stderr },
        ))
    });
    check_signature(&runner, "kbf-daemon", Path::new("/out/kbf-daemon"), T)
}

/// Catches the quarantine attribute going unseen: `xattr -p` exiting 0 means the file
/// carries it, exit 1 with `No such xattr` means it does not, and anything else (another
/// exit, a status from a signal, a message that is not that one, a tool that is not there
/// or runs out of time) is an error, never read as "not quarantined".
///
/// Mutant seen red: the exit 0 arm returning false (a quarantined file passes).
#[test]
fn the_quarantine_attribute_is_read_from_xattrs_exit_and_message() {
    assert!(quarantine(ran(0, "0081;5f;Safari;\n", "")).expect("answers"));
    assert!(
        !quarantine(ran(
            1,
            "",
            "xattr: [Errno 93] No such xattr: com.apple.quarantine\n"
        ))
        .expect("answers")
    );
    for other in [
        ran(1, "", "xattr: [Errno 13] Permission denied\n"),
        ran(2, "", ""),
        Ran {
            code: None,
            ..ran(0, "", "")
        },
    ] {
        let err = quarantine(other).expect_err("an error, not a pass");
        assert_eq!(err.code(), "tool-failed");
    }
    let gone = FakeRunner::new(|_| Err(io::Error::from(io::ErrorKind::NotFound)));
    let err = quarantined(&gone, Path::new("/x"), T).expect_err("no xattr tool");
    assert_eq!(err.code(), "tool-failed");
    let slow = FakeRunner::new(|_| {
        Ok(Ran {
            code: None,
            timed_out: true,
            ..ran(0, "", "")
        })
    });
    let err = quarantined(&slow, Path::new("/x"), T).expect_err("too slow");
    assert_eq!(err.code(), "timed-out");
}

/// Catches the tool being run with other arguments than the ones whose answer is read:
/// the attribute asked for is the quarantine one, on the file, by the system's `xattr`,
/// with an empty environment, from `/`.
#[test]
fn xattr_is_asked_for_the_quarantine_attribute_of_the_file() {
    let runner = FakeRunner::new(|_| Ok(ran(1, "", "No such xattr")));
    quarantined(&runner, Path::new("/inbox/kbf.tar.gz"), T).expect("answers");
    let call = &runner.calls()[0];
    assert_eq!(call.program, PathBuf::from("/usr/bin/xattr"));
    let args: Vec<&str> = call
        .args
        .iter()
        .map(|a| a.to_str().expect("utf-8"))
        .collect();
    assert_eq!(
        args,
        ["-s", "-p", "com.apple.quarantine", "/inbox/kbf.tar.gz"]
    );
    assert!(call.env.is_empty());
    assert_eq!(
        (call.cwd.as_path(), call.run_as, call.timeout),
        (Path::new("/"), None, T)
    );
}

/// Catches a signature accepted that CI would not have made: it must verify strictly and
/// its code directory flags must be exactly `adhoc,runtime`. Not valid, no hardened
/// runtime, not ad hoc (a Developer ID signature has other flags), extra flags and no
/// code directory line at all are each refused; the refusal says what the flags were.
///
/// Mutants seen red: `--verify` result ignored; flags compared with `contains`.
#[test]
fn a_signature_must_verify_and_be_ad_hoc_with_the_hardened_runtime() {
    codesign(0, INFO).expect("the signature CI makes");

    let err = codesign(1, INFO).expect_err("invalid");
    assert_eq!(err.code(), "signature-invalid");
    for (info, flags) in [
        ("CodeDirectory v=20400 flags=0x2(adhoc) hashes=1\n", "adhoc"),
        (
            "CodeDirectory v=20400 flags=0x10000(runtime) hashes=1\n",
            "runtime",
        ),
        (
            "CodeDirectory v=20400 flags=0x10002(adhoc,runtime,library-validation) hashes=1\n",
            "adhoc,runtime,library-validation",
        ),
        ("CodeDirectory v=20400 flags=0x0(none) hashes=1\n", "none"),
        ("Executable=/x\n", ""),
    ] {
        let err = codesign(0, info).expect_err(info);
        assert_eq!(err.code(), "signature-flags");
        assert!(
            err.to_string().contains(&format!("flags are {flags:?}")),
            "{err}"
        );
    }
}

/// Catches the signature tools failing to run being read as a verdict: a `codesign` that
/// cannot be started or runs out of time on either call is its own refusal.
#[test]
fn a_codesign_that_cannot_run_is_not_a_verdict() {
    let gone = FakeRunner::new(|_| Err(io::Error::from(io::ErrorKind::NotFound)));
    let err = check_signature(&gone, "kbf", Path::new("/x"), T).expect_err("no codesign");
    assert_eq!(err.code(), "tool-failed");
    let display_slow = FakeRunner::new(|s| {
        Ok(Ran {
            code: Some(0),
            timed_out: s.args[0] == "--display",
            ..ran(0, "", "")
        })
    });
    let err = check_signature(&display_slow, "kbf", Path::new("/x"), T).expect_err("slow");
    assert_eq!(err.code(), "timed-out");
}

/// Catches the flags parser reading what it should not: it reads the names after
/// `flags=0x<hex>` on the `CodeDirectory` line and nothing from other lines or from a
/// line that is cut.
#[test]
fn the_code_directory_flags_are_read_from_their_line() {
    assert_eq!(code_directory_flags(INFO), Some("adhoc,runtime"));
    assert_eq!(code_directory_flags("flags=0x2(adhoc)\n"), None);
    assert_eq!(code_directory_flags("CodeDirectory v=1 size=2\n"), None);
    assert_eq!(
        code_directory_flags("CodeDirectory flags=0x2 hashes\n"),
        None
    );
    assert_eq!(
        code_directory_flags("CodeDirectory flags=0x2(adhoc\n"),
        None
    );
    assert_eq!(code_directory_flags(""), None);
}
