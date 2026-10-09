//! REAPI client principals: who a bearer token names, kept in a token file the server
//! re-reads when it changes.
//!
//! Nothing serves from this module yet: no listener reads a token file, and REAPI
//! calls are not authenticated. It holds the file format, the file rules, the reload,
//! and the check of an `Authorization` header against the file's entries.
//!
//! **The file.** Each line is blank, a `#` comment (a `#` anywhere starts one), or
//! one entry of four fields separated by spaces or tabs:
//!
//! ```text
//! <principal> <role> <qos> sha256:<64 hex>
//! ci-main     client ci    sha256:9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08
//! ```
//!
//! - `principal` names the caller: 1 to 64 characters from `A-Z a-z 0-9 . _ @ -`. A
//!   principal may have more than one line (one per token), so a token is rotated by
//!   adding the new line, moving its clients to the new token, then removing the old
//!   line.
//! - `role` is `client` (the only role so far; any other word is refused, so a file
//!   written for a later server is not half-read by this one).
//! - `qos` is a built-in QoS level (`interactive`, `ci` or `batch`), the level this
//!   principal's work gets when it names none.
//! - the last field is the SHA-256 digest of the token, in hex (either case). The
//!   file holds no token, so reading it grants nothing; `kbf-server hash-token`
//!   prints the line for a token read from stdin. Two lines with the same digest are
//!   refused: a token names one principal.
//!
//! A parse error names its line number and what is wrong, never the digest field's
//! value.
//!
//! **File rules**, as for the operator API token ([`crate::token`]): a regular file,
//! owned by the server's effective user, mode 0600 or 0400, opened without blocking
//! and checked on the open file, at most [`MAX_TOKEN_STORE_BYTES`] bytes, UTF-8. A
//! file others can write could gain a token; one others can read tells them who may
//! call.
//!
//! **Reload.** [`TokenStore::current`] looks at the file's metadata at most once per
//! interval ([`RELOAD_INTERVAL`] by default). When its device, inode, size, mode,
//! owner, modification time or change time differ from the last look, the file is
//! read and checked again. The store then holds what that read gave: the new entries,
//! or an error. **Fail closed:** while the file is missing, refused by the rules, or
//! does not parse, `current` returns that error, and no entry of an earlier read is
//! kept. Replace the file by writing a new one in the same directory, with the same
//! owner and mode, and renaming it over the old one: a reader that runs during an
//! in-place edit can see half a line, and an in-place edit that keeps the size and
//! lands within the filesystem's timestamp granularity may go unseen until the next
//! change.
//!
//! **Debug** of a [`Principal`], [`Principals`] or [`TokenStore`] shows names, roles
//! and QoS levels, never a digest.

use std::fmt;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use kbf_types::Qos;
use sha2::{Digest as _, Sha256};
use subtle::ConstantTimeEq;

use crate::token::{TokenFileError, bearer_digest};

/// The most bytes a token file may hold.
pub const MAX_TOKEN_STORE_BYTES: usize = 1 << 20;

/// How often, at most, [`TokenStore::current`] looks at the file's metadata.
pub const RELOAD_INTERVAL: Duration = Duration::from_secs(1);

/// The most characters a principal's name may have.
pub const MAX_PRINCIPAL_BYTES: usize = 64;

/// What a principal may do. One role so far.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClientRole {
    /// A REAPI client: Execute, read and write the CAS, read the action cache.
    Client,
}

impl ClientRole {
    /// The role as the file spells it.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Client => "client",
        }
    }

    /// The role the file spells `word`, if any.
    #[must_use]
    pub fn parse(word: &str) -> Option<Self> {
        (word == "client").then_some(Self::Client)
    }
}

/// One entry of a token file: a principal, its role and default QoS, and the digest
/// of one of its tokens.
#[derive(Clone)]
pub struct Principal {
    name: String,
    role: ClientRole,
    qos: Qos,
    digest: [u8; 32],
}

impl fmt::Debug for Principal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Principal")
            .field("name", &self.name)
            .field("role", &self.role)
            .field("qos", &self.qos.name())
            .finish_non_exhaustive()
    }
}

impl Principal {
    /// Who the token names.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// What the principal may do.
    #[must_use]
    pub const fn role(&self) -> ClientRole {
        self.role
    }

    /// The QoS level its work gets when it names none.
    #[must_use]
    pub const fn qos(&self) -> &Qos {
        &self.qos
    }
}

/// The entries of a token file.
#[derive(Clone, Default)]
pub struct Principals {
    entries: Vec<Principal>,
}

impl fmt::Debug for Principals {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(&self.entries).finish()
    }
}

impl Principals {
    /// Parses a token file's text (format in the module docs).
    ///
    /// # Errors
    /// The 1-based number of the first bad line, and what is wrong with it (never the
    /// digest field's value).
    pub fn parse(text: &str) -> Result<Self, (usize, String)> {
        let mut entries: Vec<Principal> = Vec::new();
        for (at, line) in text.lines().enumerate() {
            let line = line.split('#').next().unwrap_or_default();
            let words: Vec<&str> = line.split_whitespace().collect();
            let bad = |why: String| (at + 1, why);
            let [name, role, qos, digest] = words.as_slice() else {
                if words.is_empty() {
                    continue;
                }
                return Err(bad(format!(
                    "an entry is four fields, `<principal> <role> <qos> sha256:<64 hex>`; \
                     this line has {}",
                    words.len()
                )));
            };
            if !valid_name(name) {
                return Err(bad(format!(
                    "principal {name:?} must be 1 to {MAX_PRINCIPAL_BYTES} characters from \
                     A-Z a-z 0-9 . _ @ -"
                )));
            }
            let role = ClientRole::parse(role)
                .ok_or_else(|| bad(format!("unknown role {role:?}; the role is `client`")))?;
            let qos: Qos = qos.parse().map_err(|e| bad(format!("{e}")))?;
            let digest = parse_digest(digest)
                .ok_or_else(|| bad("the digest must be `sha256:` and 64 hex digits".to_owned()))?;
            if let Some(first) = entries.iter().position(|e| e.digest == digest) {
                return Err(bad(format!(
                    "the same digest as entry {} (principal {:?}): a token names one principal",
                    first + 1,
                    entries[first].name
                )));
            }
            entries.push(Principal {
                name: (*name).to_owned(),
                role,
                qos,
                digest,
            });
        }
        Ok(Self { entries })
    }

    /// The number of entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether there are no entries (every token is refused).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The principal whose token `authorization`, an `Authorization` header's value,
    /// presents as `Bearer <token>` (the scheme in any case), if any. The presented
    /// token is hashed and its digest compared in constant time with every entry's,
    /// all of them, whether or not one has matched already.
    #[must_use]
    pub fn admit(&self, authorization: &[u8]) -> Option<&Principal> {
        let presented = bearer_digest(authorization)?;
        let mut found = None;
        // Every entry is compared, whether or not one has matched already.
        for entry in &self.entries {
            if bool::from(presented.ct_eq(&entry.digest)) {
                found = Some(entry);
            }
        }
        found
    }
}

/// The token file line for `token`: `<principal> <role> <qos> sha256:<hex>`, with
/// `token` stripped of surrounding whitespace before it is hashed.
///
/// # Errors
/// The principal's name is not valid, or the token has fewer than
/// [`crate::token::MIN_TOKEN_BYTES`] characters or a character that is not visible
/// ASCII.
pub fn token_line(
    principal: &str,
    role: ClientRole,
    qos: &Qos,
    token: &[u8],
) -> Result<String, String> {
    if !valid_name(principal) {
        return Err(format!(
            "principal {principal:?} must be 1 to {MAX_PRINCIPAL_BYTES} characters from \
             A-Z a-z 0-9 . _ @ -"
        ));
    }
    let token = token.trim_ascii();
    if !crate::token::usable(token) {
        return Err(format!(
            "a token is at least {} visible ASCII characters, with no space inside",
            crate::token::MIN_TOKEN_BYTES
        ));
    }
    let digest: String = Sha256::digest(token)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    Ok(format!(
        "{principal} {} {} sha256:{digest}",
        role.name(),
        qos.name()
    ))
}

/// [`token_line`] for the token `input` holds, read to its end; at most
/// [`crate::token::MAX_TOKEN_FILE_BYTES`] bytes are taken.
///
/// # Errors
/// `input` cannot be read or holds more, or [`token_line`] refuses.
pub fn token_line_from(
    principal: &str,
    role: ClientRole,
    qos: &Qos,
    input: &mut dyn std::io::Read,
) -> Result<String, String> {
    let max = crate::token::MAX_TOKEN_FILE_BYTES;
    let mut token = Vec::new();
    input
        .take(u64::try_from(max + 1).unwrap_or(u64::MAX))
        .read_to_end(&mut token)
        .map_err(|e| format!("read the token: {e}"))?;
    if token.len() > max {
        return Err(format!("the token input has more than {max} bytes"));
    }
    token_line(principal, role, qos, &token)
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_PRINCIPAL_BYTES
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'@' | b'-'))
}

fn parse_digest(field: &str) -> Option<[u8; 32]> {
    let hex = field.strip_prefix("sha256:")?.as_bytes();
    if hex.len() != 64 || !hex.iter().all(u8::is_ascii_hexdigit) {
        return None;
    }
    // Only hex digits are left.
    let nibble = |b: u8| match b {
        b'0'..=b'9' => b - b'0',
        b'a'..=b'f' => b - b'a' + 10,
        _ => b - b'A' + 10,
    };
    let mut digest = [0u8; 32];
    for (byte, pair) in digest.iter_mut().zip(hex.chunks_exact(2)) {
        *byte = (nibble(pair[0]) << 4) | nibble(pair[1]);
    }
    Some(digest)
}

/// Why a token file gives no principals.
#[derive(Debug, thiserror::Error)]
pub enum TokenStoreError {
    /// It cannot be read, or breaks a file rule (type, owner, mode).
    #[error(transparent)]
    File(#[from] TokenFileError),
    /// It has more than [`MAX_TOKEN_STORE_BYTES`] bytes.
    #[error("{path} has more than {MAX_TOKEN_STORE_BYTES} bytes")]
    TooLarge { path: PathBuf },
    /// It is not UTF-8.
    #[error("{path} is not UTF-8")]
    NotUtf8 { path: PathBuf },
    /// A line does not parse.
    #[error("{path} line {line}: {why}")]
    Parse {
        path: PathBuf,
        line: usize,
        why: String,
    },
}

/// The metadata a change of the file shows in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileKey {
    dev: u64,
    ino: u64,
    size: u64,
    mode: u32,
    uid: u32,
    mtime: (i64, i64),
    ctime: (i64, i64),
}

impl FileKey {
    #[cfg(unix)]
    fn of(meta: &std::fs::Metadata) -> Self {
        use std::os::unix::fs::MetadataExt;
        Self {
            dev: meta.dev(),
            ino: meta.ino(),
            size: meta.size(),
            mode: meta.mode(),
            uid: meta.uid(),
            mtime: (meta.mtime(), meta.mtime_nsec()),
            ctime: (meta.ctime(), meta.ctime_nsec()),
        }
    }
}

/// What the last look at the file found.
struct State {
    looked: Instant,
    key: Option<FileKey>,
    loaded: Result<Arc<Principals>, Arc<TokenStoreError>>,
}

/// A token file, re-read when it changes (see the module docs).
pub struct TokenStore {
    path: PathBuf,
    interval: Duration,
    state: Mutex<State>,
}

impl fmt::Debug for TokenStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        f.debug_struct("TokenStore")
            .field("path", &self.path)
            .field("loaded", &state.loaded)
            .finish_non_exhaustive()
    }
}

impl TokenStore {
    /// The token file at `path`, read now so that a bad file stops the server at
    /// start, then looked at again at most once per [`RELOAD_INTERVAL`].
    ///
    /// # Errors
    /// The file breaks a rule or does not parse.
    pub fn open(path: &Path) -> Result<Self, TokenStoreError> {
        Self::open_with_interval(path, RELOAD_INTERVAL)
    }

    /// [`Self::open`], looking at the file at most once per `interval`.
    ///
    /// # Errors
    /// The file breaks a rule or does not parse.
    pub fn open_with_interval(path: &Path, interval: Duration) -> Result<Self, TokenStoreError> {
        let (principals, key) = load(path)?;
        Ok(Self {
            path: path.to_owned(),
            interval,
            state: Mutex::new(State {
                looked: Instant::now(),
                key: Some(key),
                loaded: Ok(Arc::new(principals)),
            }),
        })
    }

    /// The principals the file gives now: looks at its metadata if the interval has
    /// passed since the last look, and reads it again if that changed. Blocks on file
    /// I/O only then.
    ///
    /// # Errors
    /// The file, as last read, breaks a rule or does not parse, or it is gone (fail
    /// closed: no earlier entry is kept).
    pub fn current(&self) -> Result<Arc<Principals>, Arc<TokenStoreError>> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let now = Instant::now();
        if now.duration_since(state.looked) < self.interval {
            return state.loaded.clone();
        }
        state.looked = now;
        let key = match std::fs::metadata(&self.path) {
            Ok(meta) => file_key(&meta),
            Err(source) => {
                state.key = None;
                state.loaded = Err(Arc::new(TokenStoreError::File(TokenFileError::Read {
                    path: self.path.clone(),
                    source,
                })));
                return state.loaded.clone();
            }
        };
        if state.key == Some(key) {
            return state.loaded.clone();
        }
        match load(&self.path) {
            Ok((principals, read)) => {
                state.key = Some(read);
                state.loaded = Ok(Arc::new(principals));
            }
            Err(e) => {
                state.key = Some(key);
                state.loaded = Err(Arc::new(e));
            }
        }
        state.loaded.clone()
    }
}

#[cfg(unix)]
fn file_key(meta: &std::fs::Metadata) -> FileKey {
    FileKey::of(meta)
}

/// Elsewhere than Unix no token file is read (see [`load`]); every look is a change.
#[cfg(not(unix))]
fn file_key(_: &std::fs::Metadata) -> FileKey {
    FileKey {
        dev: 0,
        ino: 0,
        size: 0,
        mode: 0,
        uid: 0,
        mtime: (0, 0),
        ctime: (0, 0),
    }
}

/// Reads and parses the file under the rules, with the open file's metadata.
#[cfg(unix)]
fn load(path: &Path) -> Result<(Principals, FileKey), TokenStoreError> {
    let (content, meta) = crate::token::read_owner_only(path, MAX_TOKEN_STORE_BYTES)?;
    if content.len() > MAX_TOKEN_STORE_BYTES {
        return Err(TokenStoreError::TooLarge {
            path: path.to_owned(),
        });
    }
    let text = String::from_utf8(content).map_err(|_| TokenStoreError::NotUtf8 {
        path: path.to_owned(),
    })?;
    let principals = Principals::parse(&text).map_err(|(line, why)| TokenStoreError::Parse {
        path: path.to_owned(),
        line,
        why,
    })?;
    Ok((principals, FileKey::of(&meta)))
}

/// Elsewhere than Unix the file's owner and mode cannot be checked, so no token file
/// is accepted.
#[cfg(not(unix))]
fn load(path: &Path) -> Result<(Principals, FileKey), TokenStoreError> {
    Err(TokenStoreError::File(TokenFileError::Read {
        path: path.to_owned(),
        source: std::io::ErrorKind::Unsupported.into(),
    }))
}

#[cfg(test)]
mod tests;
