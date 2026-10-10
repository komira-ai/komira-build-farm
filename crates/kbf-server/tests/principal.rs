//! The REAPI client token file: its file rules, its reload, fail closed, and
//! `kbf-server hash-token`.

#![cfg(unix)]

use std::io::Write as _;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use kbf_server::principal::{
    ClientRole, MAX_TOKEN_STORE_BYTES, Principals, TokenStore, TokenStoreError, token_line,
};
use kbf_server::token::TokenFileError;
use kbf_types::Qos;

const BIN: &str = env!("CARGO_BIN_EXE_kbf-server");
const TOKEN_A: &str = "kbf-test-token-a-0123456789abcdef0123456789";
const TOKEN_B: &str = "kbf-test-token-b-0123456789abcdef0123456789";

/// A directory unique to this call.
fn dir() -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("kbf-server-principal")
        .join(format!("{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("a directory");
    dir
}

fn line(principal: &str, token: &str) -> String {
    token_line(principal, ClientRole::Client, &Qos::Ci, token.as_bytes()).expect("a line") + "\n"
}

/// Replaces `path` with `content` and `mode` the way the module docs say to: a new
/// file in the same directory, renamed over the old one.
fn replace(path: &Path, content: &str, mode: u32) {
    let next = path.with_extension("next");
    std::fs::write(&next, content).expect("write");
    std::fs::set_permissions(&next, std::fs::Permissions::from_mode(mode)).expect("chmod");
    std::fs::rename(&next, path).expect("rename");
}

fn tokens_file(content: &str) -> PathBuf {
    let path = dir().join("tokens");
    replace(&path, content, 0o600);
    path
}

fn admits(store: &TokenStore, token: &str) -> Option<String> {
    let principals = store.current().expect("the file gives principals");
    principals
        .admit(format!("Bearer {token}").as_bytes())
        .map(|p| p.name().to_owned())
}

/// Catches: a token file that group or others can read or write accepted at start
/// (0644 is the usual mode a hand-made file gets).
#[test]
fn a_file_others_can_reach_is_refused_at_open() {
    for mode in [0o644, 0o640, 0o604, 0o660, 0o622, 0o700, 0o4600] {
        let path = dir().join("tokens");
        replace(&path, &line("ci", TOKEN_A), mode);
        let refused = TokenStore::open(&path);
        let Err(TokenStoreError::File(TokenFileError::Mode { mode: got, .. })) = refused else {
            panic!("mode {mode:04o}: {refused:?}");
        };
        assert_eq!(got, mode);
    }
    for mode in [0o600, 0o400] {
        let path = dir().join("tokens");
        replace(&path, &line("ci", TOKEN_A), mode);
        assert!(TokenStore::open(&path).is_ok(), "mode {mode:04o}");
    }
}

/// Catches: the owner check not applied to the token file. As root, a 0600 file given
/// to uid 65534 (nobody); otherwise a system file root owns.
#[test]
fn a_file_another_user_owns_is_refused() {
    let me = rustix::process::geteuid().as_raw();
    let path = if me == 0 {
        let path = tokens_file(&line("ci", TOKEN_A));
        let nobody = rustix::process::Uid::from_raw(65_534);
        rustix::fs::chown(&path, Some(nobody), None).expect("chown as root");
        path
    } else {
        PathBuf::from("/etc/hosts")
    };
    let refused = TokenStore::open(&path);
    assert!(
        matches!(
            refused,
            Err(TokenStoreError::File(TokenFileError::Owner { .. }))
        ),
        "{refused:?}"
    );
}

/// Catches: a directory, a missing file, a file over the size limit or one that is
/// not UTF-8 accepted at start, and a bad line not stopping the start.
#[test]
fn a_file_that_is_not_a_token_file_is_refused_at_open() {
    let d = dir();
    assert!(matches!(
        TokenStore::open(&d),
        Err(TokenStoreError::File(TokenFileError::NotAFile { .. }))
    ));
    assert!(matches!(
        TokenStore::open(&d.join("missing")),
        Err(TokenStoreError::File(TokenFileError::Read { .. }))
    ));
    let padded = format!(
        "{}{}",
        line("ci", TOKEN_A),
        "#".repeat(MAX_TOKEN_STORE_BYTES)
    );
    let refused = TokenStore::open(&tokens_file(&padded[..=MAX_TOKEN_STORE_BYTES]));
    assert!(
        matches!(refused, Err(TokenStoreError::TooLarge { .. })),
        "{refused:?}"
    );
    assert!(TokenStore::open(&tokens_file(&padded[..MAX_TOKEN_STORE_BYTES])).is_ok());
    let path = dir().join("tokens");
    std::fs::write(&path, b"# \xff\n").expect("write");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    let refused = TokenStore::open(&path);
    assert!(
        matches!(refused, Err(TokenStoreError::NotUtf8 { .. })),
        "{refused:?}"
    );
    let refused = TokenStore::open(&tokens_file(&format!("{}ci client\n", line("a", TOKEN_A))));
    let Err(TokenStoreError::Parse { line, .. }) = refused else {
        panic!("{refused:?}");
    };
    assert_eq!(line, 2);
}

/// Catches: the reload removed (a removed line still admitted, an added one refused
/// until a restart).
#[test]
fn an_edit_takes_effect_without_a_restart() {
    let path = tokens_file(&line("ci", TOKEN_A));
    let store = TokenStore::open_with_interval(&path, Duration::ZERO).expect("open");
    assert_eq!(admits(&store, TOKEN_A).as_deref(), Some("ci"));
    assert_eq!(admits(&store, TOKEN_B), None);
    replace(&path, &(line("ci", TOKEN_A) + &line("dev", TOKEN_B)), 0o600);
    assert_eq!(admits(&store, TOKEN_B).as_deref(), Some("dev"));
    replace(&path, &line("dev", TOKEN_B), 0o600);
    assert_eq!(admits(&store, TOKEN_A), None);
    assert_eq!(admits(&store, TOKEN_B).as_deref(), Some("dev"));
}

/// Catches: the file looked at on every call regardless of the interval, and its
/// metadata not checked at all once the interval has passed.
#[test]
fn the_file_is_looked_at_at_most_once_per_interval() {
    let path = tokens_file(&line("ci", TOKEN_A));
    let store = TokenStore::open_with_interval(&path, Duration::from_secs(3600)).expect("open");
    replace(&path, &line("dev", TOKEN_B), 0o600);
    assert_eq!(admits(&store, TOKEN_A).as_deref(), Some("ci"));
    assert_eq!(admits(&store, TOKEN_B), None);

    let store = TokenStore::open_with_interval(&path, Duration::from_millis(50)).expect("open");
    replace(&path, &line("ci", TOKEN_A), 0o600);
    std::thread::sleep(Duration::from_millis(60));
    assert_eq!(admits(&store, TOKEN_A).as_deref(), Some("ci"));
}

/// Catches: a parse error that keeps the old set (fail open), and a store that stays
/// refused after the file is fixed.
#[test]
fn a_bad_edit_refuses_every_call_until_fixed() {
    let path = tokens_file(&line("ci", TOKEN_A));
    let store = TokenStore::open_with_interval(&path, Duration::ZERO).expect("open");
    assert_eq!(admits(&store, TOKEN_A).as_deref(), Some("ci"));
    replace(
        &path,
        &(line("ci", TOKEN_A) + "dev client ci sha256:00\n"),
        0o600,
    );
    let refused = store.current();
    let Err(e) = &refused else {
        panic!("a malformed file still gives principals: {refused:?}");
    };
    assert!(
        matches!(**e, TokenStoreError::Parse { line: 2, .. }),
        "{e:?}"
    );
    // Still refused at the next look, though the file has not changed since.
    assert!(store.current().is_err());
    replace(&path, &line("ci", TOKEN_A), 0o600);
    assert_eq!(admits(&store, TOKEN_A).as_deref(), Some("ci"));
}

/// Catches: the file rules checked only at start (a file made readable by others, or
/// removed, after the start still admitting its tokens).
#[test]
fn a_file_loosened_or_removed_after_start_refuses_every_call() {
    let path = tokens_file(&line("ci", TOKEN_A));
    let store = TokenStore::open_with_interval(&path, Duration::ZERO).expect("open");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("chmod");
    let refused = store.current();
    assert!(
        matches!(
            refused.as_ref().err().map(|e| &**e),
            Some(TokenStoreError::File(TokenFileError::Mode {
                mode: 0o644,
                ..
            }))
        ),
        "{refused:?}"
    );
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    assert_eq!(admits(&store, TOKEN_A).as_deref(), Some("ci"));
    std::fs::remove_file(&path).expect("remove");
    let refused = store.current();
    assert!(
        matches!(
            refused.as_ref().err().map(|e| &**e),
            Some(TokenStoreError::File(TokenFileError::Read { .. }))
        ),
        "{refused:?}"
    );
    replace(&path, &line("ci", TOKEN_A), 0o600);
    assert_eq!(admits(&store, TOKEN_A).as_deref(), Some("ci"));
}

/// Catches: a digest shown by the store's `Debug`, or by the error a refused file
/// gives.
#[test]
fn the_store_and_its_errors_show_no_digest() {
    let a = line("ci", TOKEN_A);
    let hex = a
        .trim_end()
        .rsplit(':')
        .next()
        .expect("a digest")
        .to_owned();
    let path = tokens_file(&a);
    let store = TokenStore::open_with_interval(&path, Duration::ZERO).expect("open");
    store.current().expect("principals");
    let shown = format!("{store:?}");
    assert!(shown.contains("\"ci\""), "{shown}");
    assert!(
        !shown.contains(&hex[..16]) && !shown.contains(TOKEN_A),
        "{shown}"
    );
    replace(&path, &format!("{a}{}", &a[..a.len() - 2]), 0o600);
    let refused = store.current().expect_err("a truncated line");
    let shown = format!("{refused} {refused:?} {store:?}");
    assert!(
        !shown.contains(&hex[..16]) && !shown.contains(TOKEN_A),
        "{shown}"
    );
}

fn hash_token(args: &[&str], stdin: &[u8]) -> (Option<i32>, String, String) {
    let mut all = vec!["hash-token"];
    all.extend_from_slice(args);
    run(&all, stdin)
}

/// Runs the binary with `args` and `stdin`; a write to a child that exited before
/// reading stdin (a refusal by clap) is not an error of the test.
fn run(args: &[&str], stdin: &[u8]) -> (Option<i32>, String, String) {
    let mut child = Command::new(BIN)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn kbf-server");
    let _ = child.stdin.take().expect("stdin").write_all(stdin);
    let out = child.wait_with_output().expect("wait");
    (
        out.status.code(),
        String::from_utf8(out.stdout).expect("UTF-8"),
        String::from_utf8(out.stderr).expect("UTF-8"),
    )
}

/// Catches: hash-token printing a line the file does not parse, hashing the newline
/// with the token, ignoring `--qos`, or not printing a line at all.
#[test]
fn hash_token_prints_a_line_the_file_admits() {
    let (code, out, err) = hash_token(&["ci-main"], format!("{TOKEN_A}\n").as_bytes());
    assert_eq!(code, Some(0), "{err}");
    let principals = Principals::parse(&out).expect("the line parses");
    let got = principals
        .admit(format!("Bearer {TOKEN_A}").as_bytes())
        .expect("admits the token");
    assert_eq!((got.name(), got.qos()), ("ci-main", &Qos::Ci));
    assert!(!out.contains(TOKEN_A), "{out}");

    let (code, out, err) = hash_token(&["dev", "--qos", "interactive"], TOKEN_B.as_bytes());
    assert_eq!(code, Some(0), "{err}");
    let principals = Principals::parse(&out).expect("the line parses");
    let got = principals.admit(format!("Bearer {TOKEN_B}").as_bytes());
    assert_eq!(got.map(|p| p.qos().clone()), Some(Qos::Interactive));
}

/// Catches: hash-token accepting a token too short to be a secret, an unknown QoS or
/// a bad principal name, and exiting 0 with no line on a refusal.
#[test]
fn hash_token_refuses_what_the_file_would_not_take() {
    let (code, out, err) = hash_token(&["dev"], b"short\n");
    assert_eq!(code, Some(2), "{err}");
    assert!(out.is_empty() && err.contains("at least 32"), "{out} {err}");
    let (code, out, _) = hash_token(&["a/b"], TOKEN_A.as_bytes());
    assert_eq!((code, out.as_str()), (Some(2), ""));
    let (code, out, _) = hash_token(&["dev", "--qos", "urgent"], TOKEN_A.as_bytes());
    assert_eq!((code, out.as_str()), (Some(2), ""));
    let (code, out, _) = hash_token(&[], TOKEN_A.as_bytes());
    assert_eq!((code, out.as_str()), (Some(2), ""));
}

/// Catches: server flags accepted with the subcommand (they would be silently
/// ignored, since hash-token serves nothing). The token on stdin is valid, so the
/// only reason left to refuse is the flag; the same run without the flag succeeds.
#[test]
fn hash_token_takes_no_server_flags() {
    let stdin = format!("{TOKEN_A}\n");
    let (code, out, err) = run(&["hash-token", "dev"], stdin.as_bytes());
    assert_eq!(code, Some(0), "{err}");
    let (code, out2, err) = run(
        &["--listen", "127.0.0.1:0", "hash-token", "dev"],
        stdin.as_bytes(),
    );
    assert_eq!(code, Some(2), "{out2} {err}");
    assert!(out2.is_empty(), "{out2}");
    assert!(
        err.contains("cannot be used with") && err.contains("--listen"),
        "{err}"
    );
    assert!(!out.is_empty());
}
