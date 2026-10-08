use super::*;
use crate::testkit::{SkKey, software_signature};

// OpenSSH's own security-key signatures, made by real `ssh-keygen -Y sign`: the test
// vectors in openssh-portable's regress/unittests/sshsig/testdata (the files
// ed25519_sk.pub/.sig, ecdsa_sk.pub/.sig, namespace and signed-data). They prove the
// gate reads what OpenSSH writes, for both security-key types.
const OPENSSH_NAMESPACE: &str = "unittest";
const OPENSSH_DATA: &[u8] = b"This is a test, this is only a test";
const OPENSSH_ED25519_SK_PUB: &str = "sk-ssh-ed25519@openssh.com AAAAGnNrLXNzaC1lZDI1NTE5QG9wZW5zc2guY29tAAAAIJsaDYXQYruc6bilCYDIK4YSOeG+zmrRO2M9t03//7LHAAAABHNzaDo= ED25519-SK test key";
const OPENSSH_ED25519_SK_SIG: &str = "-----BEGIN SSH SIGNATURE-----
U1NIU0lHAAAAAQAAAEoAAAAac2stc3NoLWVkMjU1MTlAb3BlbnNzaC5jb20AAAAgmxoNhd
Biu5zpuKUJgMgrhhI54b7OatE7Yz23Tf//sscAAAAEc3NoOgAAAAh1bml0dGVzdAAAAAAA
AAAGc2hhNTEyAAAAZwAAABpzay1zc2gtZWQyNTUxOUBvcGVuc3NoLmNvbQAAAEAi+7eTjW
/+LQ2M+sCD+KFtH1n7VFFJon/SZFsxODyV8cWTlFKj617Ys1Ur5TV6uaEXQhck8rBA2oQI
HTPANLIPARI0Vng=
-----END SSH SIGNATURE-----
";
const OPENSSH_ECDSA_SK_PUB: &str = "sk-ecdsa-sha2-nistp256@openssh.com AAAAInNrLWVjZHNhLXNoYTItbmlzdHAyNTZAb3BlbnNzaC5jb20AAAAIbmlzdHAyNTYAAABBBKDVa5jRcT5V7E6ysmwVi7HJWh7p5D+hebLPakQcpnD2ajJZ6G/4WzuhlYWnclWY63JspDp299Rlhq5AT86/g8AAAAAEc3NoOg== ECDSA-SK test key";
const OPENSSH_ECDSA_SK_SIG: &str = "-----BEGIN SSH SIGNATURE-----
U1NIU0lHAAAAAQAAAH8AAAAic2stZWNkc2Etc2hhMi1uaXN0cDI1NkBvcGVuc3NoLmNvbQ
AAAAhuaXN0cDI1NgAAAEEEoNVrmNFxPlXsTrKybBWLsclaHunkP6F5ss9qRBymcPZqMlno
b/hbO6GVhadyVZjrcmykOnb31GWGrkBPzr+DwAAAAARzc2g6AAAACHVuaXR0ZXN0AAAAAA
AAAAZzaGE1MTIAAAB3AAAAInNrLWVjZHNhLXNoYTItbmlzdHAyNTZAb3BlbnNzaC5jb20A
AABIAAAAIHohGwyy8iKT3zwd1TYA9V/Ioo7h/3zCJUtyq/Qigt/HAAAAIGzidTwq7D/kFa
7Xjcp/KkdbIs4MfQpfAW/0OciajlpzARI0Vng=
-----END SSH SIGNATURE-----
";

const NOW: i64 = 1_800_000_000;

#[test]
fn openssh_security_key_signatures_verify() {
    // Catches: a verifier that cannot read OpenSSH's sk- encodings (the flags byte and
    // counter after the signature blob), or reads the flags from the wrong byte.
    for (public, sig, algorithm) in [
        (
            OPENSSH_ED25519_SK_PUB,
            OPENSSH_ED25519_SK_SIG,
            "sk-ssh-ed25519@openssh.com",
        ),
        (
            OPENSSH_ECDSA_SK_PUB,
            OPENSSH_ECDSA_SK_SIG,
            "sk-ecdsa-sha2-nistp256@openssh.com",
        ),
    ] {
        let signers =
            AllowedSigners::parse(&format!("ops namespaces=\"{OPENSSH_NAMESPACE}\" {public}"))
                .unwrap();
        let signer = signers
            .verify_in(OPENSSH_NAMESPACE, OPENSSH_DATA, sig, NOW, false)
            .unwrap();
        assert_eq!(
            signer,
            Signer {
                principals: "ops".into(),
                algorithm: algorithm.into(),
                flags: FLAG_USER_PRESENT,
                counter: 0x1234_5678,
            }
        );
        // OpenSSH's vectors carry no user-verified flag.
        assert_eq!(
            signers.verify_in(OPENSSH_NAMESPACE, OPENSSH_DATA, sig, NOW, true),
            Err(VerifyError::NotUserVerified)
        );
        assert_eq!(
            signers.verify_in(OPENSSH_NAMESPACE, b"another message", sig, NOW, false),
            Err(VerifyError::BadSignature)
        );
    }
}

fn signers_for(key: &SkKey) -> AllowedSigners {
    AllowedSigners::parse(&key.allowed_line("alice@example.org")).unwrap()
}

#[test]
fn a_touched_signature_from_an_allowed_security_key_verifies() {
    let key = SkKey::new(1);
    let signer = signers_for(&key)
        .verify(b"message", &key.sign(b"message"), NOW, false)
        .unwrap();
    assert_eq!(signer.principals, "alice@example.org");
    assert_eq!(signer.counter, 7);
}

#[test]
fn no_signature_and_a_bad_one_are_refused() {
    // Catches: a verifier that skips the signature check (M10, first row).
    let key = SkKey::new(1);
    let signers = signers_for(&key);
    assert_eq!(
        signers.verify(b"m", "", NOW, false),
        Err(VerifyError::Malformed)
    );
    assert_eq!(
        signers.verify(b"other", &key.sign(b"m"), NOW, false),
        Err(VerifyError::BadSignature)
    );
}

#[test]
fn a_key_outside_the_allowed_signers_is_refused() {
    let signers = signers_for(&SkKey::new(1));
    let stranger = SkKey::new(2);
    assert_eq!(
        signers.verify(b"m", &stranger.sign(b"m"), NOW, false),
        Err(VerifyError::UnknownSigner)
    );
}

#[test]
fn another_namespace_is_refused_both_in_the_signature_and_in_the_file() {
    // Catches: ignoring the signed namespace (a signature made for `git` or `file`
    // reused as an erase), and ignoring the file's namespaces= limit.
    let key = SkKey::new(1);
    let signers = signers_for(&key);
    assert_eq!(
        signers.verify(
            b"m",
            &key.sign_with("file", b"m", FLAG_USER_PRESENT),
            NOW,
            false
        ),
        Err(VerifyError::Namespace("file".into()))
    );
    for line in [
        format!("alice namespaces=\"git,file\" {}", key.openssh()),
        format!("alice {}", key.openssh()),
    ] {
        let signers = AllowedSigners::parse(&line).unwrap();
        assert_eq!(
            signers.verify(b"m", &key.sign(b"m"), NOW, false),
            Err(VerifyError::UnknownSigner)
        );
    }
    // A list that includes the gate's namespace is enough.
    let line = format!("alice namespaces=\"git,{NAMESPACE}\" {}", key.openssh());
    let signers = AllowedSigners::parse(&line).unwrap();
    assert!(signers.verify(b"m", &key.sign(b"m"), NOW, false).is_ok());
}

#[test]
fn a_software_key_is_refused_even_when_listed() {
    // Catches: accepting any key type (M10 mutant "accept any key type"): a software
    // ed25519 key added to the file by mistake must not erase a Mac.
    let (line, sig) = software_signature("bob", b"m");
    let signers = AllowedSigners::parse(&line).unwrap();
    assert_eq!(
        signers.verify(b"m", &sig, NOW, false),
        Err(VerifyError::SoftwareKey("ssh-ed25519".into()))
    );
}

#[test]
fn a_signature_without_a_touch_is_refused() {
    // Catches: ignoring the flags byte (M10 mutant "ignore the flags byte").
    let key = SkKey::new(1);
    let signers = signers_for(&key);
    let untouched = key.sign_with(NAMESPACE, b"m", 0);
    assert_eq!(
        signers.verify(b"m", &untouched, NOW, false),
        Err(VerifyError::NoTouch)
    );
    let verified_only = key.sign_with(NAMESPACE, b"m", FLAG_USER_VERIFIED);
    assert_eq!(
        signers.verify(b"m", &verified_only, NOW, false),
        Err(VerifyError::NoTouch)
    );
}

#[test]
fn user_verification_is_required_when_configured() {
    let key = SkKey::new(1);
    let signers = signers_for(&key);
    let touched = key.sign(b"m");
    assert_eq!(
        signers.verify(b"m", &touched, NOW, true),
        Err(VerifyError::NotUserVerified)
    );
    let both = key.sign_with(NAMESPACE, b"m", FLAG_USER_PRESENT | FLAG_USER_VERIFIED);
    assert_eq!(signers.verify(b"m", &both, NOW, true).unwrap().flags, 0x05);
}

#[test]
fn valid_after_and_valid_before_bound_the_key() {
    // Catches: ignoring the validity window, so a key retired with valid-before keeps
    // erasing Macs.
    let key = SkKey::new(1);
    let line = format!(
        "alice namespaces=\"{NAMESPACE}\",valid-after=\"20270115Z\",valid-before=\"202701160000Z\" {}",
        key.openssh()
    );
    let signers = AllowedSigners::parse(&line).unwrap();
    let sig = key.sign(b"m");
    let start = parse_time("20270115Z").unwrap();
    assert_eq!(parse_time("20270115000000Z"), Some(start));
    assert!(signers.verify(b"m", &sig, start, false).is_ok());
    assert_eq!(
        signers.verify(b"m", &sig, start - 1, false),
        Err(VerifyError::OutsideValidity)
    );
    assert_eq!(
        signers.verify(b"m", &sig, start + 86_400, false),
        Err(VerifyError::OutsideValidity)
    );
}

#[test]
fn a_second_line_for_the_same_key_is_consulted() {
    // A retired line followed by a current one for the same key: the current one wins.
    let key = SkKey::new(1);
    let text = format!(
        "# keys\n\nold namespaces=\"{NAMESPACE}\",valid-before=\"20270101Z\" {k}\n\"new one\" namespaces=\"{NAMESPACE}\" {k}\n",
        k = key.openssh()
    );
    let signers = AllowedSigners::parse(&text).unwrap();
    let signer = signers.verify(b"m", &key.sign(b"m"), NOW, false).unwrap();
    assert_eq!(signer.principals, "new one");
}

#[test]
fn malformed_and_unsupported_lines_are_refused_with_their_number() {
    let key = SkKey::new(1).openssh();
    for (line, reason) in [
        (
            format!("a cert-authority {key}"),
            "cert-authority is not supported",
        ),
        (
            format!("a namespaces=x {key}"),
            "option namespaces: expected namespaces=\"...\"",
        ),
        (
            format!("a valid-after=\"2027\" {key}"),
            "valid-after: expected YYYYMMDD[HHMM[SS]]Z",
        ),
        (
            format!("a valid-before=\"20270115\" {key}"),
            "valid-before: expected YYYYMMDD[HHMM[SS]]Z",
        ),
        (
            format!("a frobnicate=\"1\" {key}"),
            "unknown option \"frobnicate\"",
        ),
    ] {
        let err = AllowedSigners::parse(&format!("# c\n{line}")).unwrap_err();
        assert_eq!(
            err,
            SignersError {
                line: 2,
                reason: reason.into()
            }
        );
    }
    let err = AllowedSigners::parse("a ssh-ed25519 notbase64").unwrap_err();
    assert!(err.reason.starts_with("key: "), "{err}");
    assert!(err.to_string().starts_with("allowed signers line 1: key: "));
}

#[test]
fn times_are_utc_and_checked() {
    let day = NOW - 8 * 3600; // 2027-01-15T00:00:00Z
    assert_eq!(parse_time("20270115Z"), Some(day));
    assert_eq!(parse_time("202701150001Z"), Some(day + 60));
    assert_eq!(parse_time("20270115000001Z"), Some(day + 1));
    for bad in [
        "20270115",
        "2027011Z",
        "2027O115Z",
        "20271315Z",
        "20270132Z",
        "202701152500Z",
    ] {
        assert_eq!(parse_time(bad), None, "{bad}");
    }
}

#[test]
fn error_messages_name_the_problem() {
    assert_eq!(
        VerifyError::SoftwareKey("ssh-ed25519".into()).to_string(),
        "signed with a ssh-ed25519 key; only security keys (sk-) are accepted"
    );
    assert!(
        VerifyError::Namespace("git".into())
            .to_string()
            .contains("\"git\"")
    );
}
