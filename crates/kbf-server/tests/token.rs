//! The operator API token file: what `--api-token-file` accepts and refuses.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use std::time::Duration;

use kbf_server::token::{ApiToken, MAX_TOKEN_FILE_BYTES, MIN_TOKEN_BYTES, TokenFileError};

const TOKEN: &str = "kbf-test-token-0123456789abcdef0123456789";

fn dir() -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("kbf-server-token");
    std::fs::create_dir_all(&dir).expect("a token directory");
    dir
}

/// A file holding `content` with `mode`, unique to this call.
fn file(content: &[u8], mode: u32) -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let path = dir().join(format!("token-{}-{n}", std::process::id()));
    std::fs::write(&path, content).expect("write the file");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).expect("chmod");
    path
}

fn admitted(path: &Path) -> bool {
    let token = ApiToken::from_file(path).expect("a usable token");
    token.admits(format!("Bearer {TOKEN}").as_bytes())
}

/// Catches: a token file with mode 0600 or 0400 refused, the trailing newline (or
/// surrounding whitespace) kept as part of the token, and a token that admits
/// nothing.
#[test]
fn an_owner_only_file_gives_its_token() {
    for mode in [0o600, 0o400] {
        assert!(admitted(&file(format!("{TOKEN}\n").as_bytes(), mode)));
    }
    assert!(admitted(&file(format!("  {TOKEN} \r\n").as_bytes(), 0o600)));
    let exact = "x".repeat(MIN_TOKEN_BYTES);
    let token = ApiToken::from_file(&file(exact.as_bytes(), 0o600)).expect("long enough");
    assert!(token.admits(format!("Bearer {exact}").as_bytes()));
}

/// Catches: the mode check removed or loosened (a token that group or others can
/// read, or that is executable or setuid, authenticates nobody).
#[test]
fn a_file_others_can_reach_is_refused() {
    for mode in [0o644, 0o640, 0o604, 0o660, 0o606, 0o700, 0o4600] {
        let refused = ApiToken::from_file(&file(TOKEN.as_bytes(), mode));
        let Err(TokenFileError::Mode { mode: got, .. }) = refused else {
            panic!("mode {mode:04o}: {refused:?}");
        };
        assert_eq!(got, mode);
        let message = refused.unwrap_err().to_string();
        assert!(message.contains(&format!("{mode:04o}")), "{message}");
    }
}

/// Catches: the owner check removed (a token planted by another user would be
/// trusted). As root, a 0600 file given to uid 65534 (nobody); otherwise a system
/// file root owns.
#[test]
fn a_file_another_user_owns_is_refused() {
    let me = rustix::process::geteuid().as_raw();
    let (path, other) = if me == 0 {
        let path = file(TOKEN.as_bytes(), 0o600);
        let nobody = rustix::process::Uid::from_raw(65_534);
        rustix::fs::chown(&path, Some(nobody), None).expect("chown as root");
        (path, 65_534)
    } else {
        (PathBuf::from("/etc/hosts"), 0)
    };
    let refused = ApiToken::from_file(&path);
    let Err(TokenFileError::Owner { owner, server, .. }) = refused else {
        panic!("{refused:?}");
    };
    assert_eq!((owner, server), (other, me));
}

/// Catches: a FIFO (or another file that blocks on open) waited on at start instead
/// of refused: the open would hang until something writes to it.
#[test]
fn a_fifo_is_refused_at_once() {
    let path = dir().join(format!("fifo-{}", std::process::id()));
    let _ = std::fs::remove_file(&path);
    rustix::fs::mknodat(
        rustix::fs::CWD,
        &path,
        rustix::fs::FileType::Fifo,
        rustix::fs::Mode::from_raw_mode(0o600),
        0,
    )
    .expect("mkfifo");
    let (tx, rx) = std::sync::mpsc::channel();
    let probe = path.clone();
    std::thread::spawn(move || {
        let _ = tx.send(ApiToken::from_file(&probe));
    });
    let refused = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("the open of a FIFO blocked");
    assert!(
        matches!(refused, Err(TokenFileError::NotAFile { .. })),
        "{refused:?}"
    );
    let _ = std::fs::remove_file(&path);
}

/// Catches: a token file of any size read whole at start (a mistaken path to a large
/// file would be read into memory), and the limit off by one.
#[test]
fn a_file_larger_than_the_limit_is_refused() {
    let padded = format!("{TOKEN}{}", " ".repeat(MAX_TOKEN_FILE_BYTES - TOKEN.len()));
    assert!(ApiToken::from_file(&file(padded.as_bytes(), 0o600)).is_ok());
    let over = format!("{padded} ");
    let refused = ApiToken::from_file(&file(over.as_bytes(), 0o600));
    assert!(
        matches!(refused, Err(TokenFileError::Content { .. })),
        "{refused:?}"
    );
    let message = refused.unwrap_err().to_string();
    assert!(message.contains("at most 4096 bytes"), "{message}");
}

/// Catches: a write-only or no-access token file accepted where the owner can still
/// open it (root opens any file): the documented modes are 0600 and 0400 only.
#[test]
fn a_write_only_or_no_access_file_is_refused() {
    for mode in [0o200, 0o000] {
        let refused = ApiToken::from_file(&file(TOKEN.as_bytes(), mode));
        assert!(
            matches!(
                refused,
                Err(TokenFileError::Mode { .. } | TokenFileError::Read { .. })
            ),
            "{mode:04o}: {refused:?}"
        );
    }
}

/// Catches: a token too short, with a space or control byte inside, or empty, being
/// accepted; a missing file or a directory not refused.
#[test]
fn a_file_without_a_usable_token_is_refused() {
    let short = "x".repeat(MIN_TOKEN_BYTES - 1);
    let spaced = format!("{} {}", &TOKEN[..20], &TOKEN[20..]);
    let control = format!("{TOKEN}\u{7}");
    let non_ascii = format!("{TOKEN}é");
    for content in ["", "\n", short.as_str(), &spaced, &control, &non_ascii] {
        let refused = ApiToken::from_file(&file(content.as_bytes(), 0o600));
        assert!(
            matches!(refused, Err(TokenFileError::Content { .. })),
            "{content:?}: {refused:?}"
        );
    }
    let missing = dir().join("no-such-token");
    assert!(matches!(
        ApiToken::from_file(&missing),
        Err(TokenFileError::Read { .. })
    ));
    let refused = ApiToken::from_file(&dir());
    assert!(
        matches!(refused, Err(TokenFileError::NotAFile { .. })),
        "{refused:?}"
    );
}
