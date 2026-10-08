//! The operator API token file: what `--api-token-file` accepts and refuses.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use kbf_server::token::{ApiToken, MIN_TOKEN_BYTES, TokenFileError};

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
/// trusted). Skipped when the tests run as root, who owns the system file used.
#[test]
fn a_file_another_user_owns_is_refused() {
    let me = rustix::process::geteuid().as_raw();
    if me == 0 {
        return;
    }
    let refused = ApiToken::from_file(Path::new("/etc/hosts"));
    let Err(TokenFileError::Owner { owner, server, .. }) = refused else {
        panic!("{refused:?}");
    };
    assert_eq!((owner, server), (0, me));
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
