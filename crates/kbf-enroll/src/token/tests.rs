//! The token file is the credential: each way it could be readable or writable by
//! someone else, or not be a token, is refused, and the secret is never in the message.

use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::PathBuf;

use super::*;
use crate::testutil::{scratch, token};

fn euid() -> u32 {
    rustix::process::geteuid().as_raw()
}

fn write(dir: &Path, name: &str, bytes: &[u8], mode: u32) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, bytes).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
    path
}

fn line() -> String {
    token().to_line()
}

#[test]
fn a_token_line_is_read_with_or_without_the_newline_at_mode_0600_or_0400() {
    let dir = scratch("token-ok");
    for (name, body, mode) in [
        ("a", line(), 0o600),
        ("b", format!("{}\n", line()), 0o600),
        ("c", line(), 0o400),
    ] {
        let got = read(&write(&dir, name, body.as_bytes(), mode), euid()).unwrap();
        assert_eq!(got.to_line(), line(), "{name}");
    }
}

#[test]
fn a_file_others_can_read_or_write_is_not_read() {
    let dir = scratch("token-mode");
    for mode in [0o644, 0o640, 0o604, 0o660, 0o700, 0o664] {
        let path = write(&dir, "t", line().as_bytes(), mode);
        match read(&path, euid()) {
            Err(TokenFileError::Mode { mode: got, .. }) => assert_eq!(got, mode),
            other => panic!("mode {mode:o}: {other:?}"),
        }
    }
}

#[test]
fn a_file_of_another_owner_is_not_read() {
    let dir = scratch("token-owner");
    let path = write(&dir, "t", line().as_bytes(), 0o600);
    let err = read(&path, euid() + 1).unwrap_err();
    assert!(
        matches!(err, TokenFileError::Owner { found, expected, .. } if found == euid() && expected == euid() + 1)
    );
    assert!(err.to_string().contains("not "), "{err}");
}

#[test]
fn a_symlink_is_not_followed_even_to_a_good_file() {
    let dir = scratch("token-link");
    let real = write(&dir, "real", line().as_bytes(), 0o600);
    symlink(&real, dir.join("link")).unwrap();
    let err = read(&dir.join("link"), euid()).unwrap_err();
    assert!(matches!(err, TokenFileError::Symlink(_)), "{err:?}");
}

#[test]
fn a_directory_and_a_missing_file_are_refused() {
    let dir = scratch("token-kind");
    assert!(matches!(
        read(&dir, euid()).unwrap_err(),
        TokenFileError::NotRegular(_)
    ));
    let err = read(&dir.join("absent"), euid()).unwrap_err();
    assert!(
        matches!(&err, TokenFileError::Io { source, .. } if source.kind() == std::io::ErrorKind::NotFound),
        "{err:?}"
    );
    assert!(err.to_string().contains("absent"));
}

#[test]
fn size_and_line_count_are_bounded() {
    let dir = scratch("token-size");
    let big = vec![b'a'; MAX_TOKEN_FILE_BYTES as usize + 1];
    let err = read(&write(&dir, "big", &big, 0o600), euid()).unwrap_err();
    assert!(matches!(err, TokenFileError::TooLarge(_)), "{err:?}");
    // Exactly at the limit is read (and then refused as not a token), not as too large.
    let at = vec![b'a'; MAX_TOKEN_FILE_BYTES as usize];
    let err = read(&write(&dir, "at", &at, 0o600), euid()).unwrap_err();
    assert!(
        matches!(
            err,
            TokenFileError::Lines(_) | TokenFileError::Malformed { .. }
        ),
        "{err:?}"
    );
    let two = format!("{}\n{}\n", line(), line());
    let err = read(&write(&dir, "two", two.as_bytes(), 0o600), euid()).unwrap_err();
    assert!(matches!(err, TokenFileError::Lines(_)), "{err:?}");
    for empty in ["", "\n"] {
        let err = read(&write(&dir, "empty", empty.as_bytes(), 0o600), euid()).unwrap_err();
        assert!(matches!(err, TokenFileError::Lines(_)), "{err:?}");
    }
}

#[test]
fn a_malformed_token_names_the_part_and_never_the_text() {
    let dir = scratch("token-bad");
    let good = line();
    let parts: Vec<&str> = good.split('.').collect();
    let secret = parts[2];
    let cases = [
        (
            format!("{}.{}.{}", parts[0], parts[1], parts[2]),
            "four dot-separated parts",
        ),
        (
            format!("kbft2.{}.{}.{}", parts[1], parts[2], parts[3]),
            "format version",
        ),
        (
            format!("{}.{}x.{}.{}", parts[0], parts[1], parts[2], parts[3]),
            "token id",
        ),
        (
            format!("{}.{}.{}x.{}", parts[0], parts[1], parts[2], parts[3]),
            "token secret",
        ),
        (
            format!(
                "{}.{}.{}.{}!",
                parts[0],
                parts[1],
                parts[2],
                &parts[3][..42]
            ),
            "token CA pin",
        ),
    ];
    for (body, want) in cases {
        let err = read(&write(&dir, "t", body.as_bytes(), 0o600), euid()).unwrap_err();
        let shown = err.to_string();
        assert!(matches!(err, TokenFileError::Malformed { .. }), "{shown}");
        assert!(shown.contains(want), "{shown} should name {want}");
        assert!(
            !shown.contains(secret),
            "the secret is in the error: {shown}"
        );
    }
    // Bytes that are not UTF-8 are a malformed token too.
    let err = read(&write(&dir, "t", &[0xff, 0xfe, b'.'], 0o600), euid()).unwrap_err();
    assert!(matches!(err, TokenFileError::Malformed { .. }), "{err:?}");
}

#[test]
fn remove_deletes_the_file_and_forgives_one_already_gone() {
    let dir = scratch("token-remove");
    let path = write(&dir, "t", line().as_bytes(), 0o600);
    remove(&path).unwrap();
    assert!(!path.exists());
    remove(&path).unwrap();
    // Any other failure is reported: a directory is not removable as a file.
    assert!(remove(&dir).is_err());
}
