//! The per-path-class auth decision (design section 3): a pure function of the path class,
//! the `buck2_auth` mode, the verified identity if any, and the bearer token if one came.
//!
//! A certificate that fails verification never gets here: it is a handshake failure. So
//! "present" always means "verified". Rules:
//!
//! - Worker class: no certificate is `UNAUTHENTICATED`; a certificate whose role is not
//!   worker or storage is `PERMISSION_DENIED` (the peer is known, the role is wrong).
//! - Reapi class: `none` allows everyone, a certificate or not; `mtls` needs a verified
//!   certificate of any role; `bearer` needs the right token and nothing else stands in for
//!   it (a certificate is neither needed nor enough, and a wrong token is a refusal even
//!   next to a good certificate: one mechanism per mode, no fallback).
//! - Operator and update classes: the operator token, whatever the certificate says.
//! - Enroll class: open. A node enrolling has no certificate yet; the child checks the
//!   enrollment token (or, for a renewal, the key the registry holds).
//! - Probe class: open.
//!
//! [`crate::security`] holds what the serving code decides with: the mode, the tokens, the
//! secret the children are told, and the per-address limit on wrong tokens ([`FailureLimit`]).

use std::collections::HashMap;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use http::{HeaderMap, header};
use kbf_peer::PeerRole;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::files::{self, FileError};
use crate::identity::Identity;
use crate::route::AuthClass;

/// How REAPI and Buck2 callers authenticate (`buck2_auth` of `installer.toml`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Buck2Auth {
    /// Network-level trust (for example a private overlay network).
    #[default]
    None,
    /// An API-key header, compared in constant time.
    Bearer,
    /// A verified client certificate.
    Mtls,
}

impl Buck2Auth {
    /// The spelling of the `buck2_auth` key.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Bearer => "bearer",
            Self::Mtls => "mtls",
        }
    }
}

/// Why a call is refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// No credential, or the wrong one: gRPC `UNAUTHENTICATED` (16), HTTP 401.
    Unauthenticated,
    /// A known peer that may not call this: gRPC `PERMISSION_DENIED` (7), HTTP 403.
    PermissionDenied,
    /// The address has presented too many wrong tokens lately: gRPC `RESOURCE_EXHAUSTED` (8),
    /// HTTP 429. The token was not looked at.
    RateLimited,
}

impl Refusal {
    /// The gRPC status code.
    #[must_use]
    pub const fn grpc_status(self) -> u8 {
        match self {
            Self::Unauthenticated => 16,
            Self::PermissionDenied => 7,
            Self::RateLimited => 8,
        }
    }

    /// The HTTP status of a non-gRPC path.
    #[must_use]
    pub const fn http_status(self) -> u16 {
        match self {
            Self::Unauthenticated => 401,
            Self::PermissionDenied => 403,
            Self::RateLimited => 429,
        }
    }
}

/// A token as the proxy keeps it: its SHA-256, so the comparison is over equal-length
/// digests. The bytes themselves are not kept.
#[derive(Clone, PartialEq, Eq)]
pub struct Token {
    digest: [u8; 32],
}

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Token(..)")
    }
}

impl Token {
    /// The token whose value is `secret`.
    #[must_use]
    pub fn new(secret: &[u8]) -> Self {
        Self {
            digest: Sha256::digest(secret).into(),
        }
    }

    /// Whether `presented` is this token, in constant time. The caller rate limits before
    /// calling this, so a flood cannot make the proxy hash without bound.
    #[must_use]
    pub fn admits(&self, presented: &[u8]) -> bool {
        let digest: [u8; 32] = Sha256::digest(presented).into();
        self.digest.ct_eq(&digest).into()
    }
}

/// The fewest bytes a token may have (the child's own rule, `kbf-server`'s `token.rs`).
pub const MIN_TOKEN_BYTES: usize = 32;
/// The most bytes a token file may hold, surrounding whitespace included.
pub const MAX_TOKEN_FILE_BYTES: usize = 4096;

/// Why a token file is refused.
#[derive(Debug, thiserror::Error)]
pub enum TokenError {
    /// It cannot be read, or is not a regular file owned by this user with mode 0600 or 0400.
    #[error(transparent)]
    File(#[from] FileError),
    /// It does not hold one token of at least [`MIN_TOKEN_BYTES`] visible ASCII characters.
    #[error(
        "{path} must hold one token of at least {MIN_TOKEN_BYTES} visible ASCII characters, \
         in at most {MAX_TOKEN_FILE_BYTES} bytes"
    )]
    Content {
        /// The file.
        path: PathBuf,
    },
}

impl Token {
    /// The token in the file at `path`: its content without surrounding whitespace (a
    /// trailing newline is usual), under the same rules as the child's operator token.
    ///
    /// # Errors
    /// [`TokenError`].
    pub fn from_file(path: &Path) -> Result<Self, TokenError> {
        let content = files::read_owner_only(path, MAX_TOKEN_FILE_BYTES)?;
        let token = content.trim_ascii();
        if content.len() > MAX_TOKEN_FILE_BYTES
            || token.len() < MIN_TOKEN_BYTES
            || !token.iter().all(u8::is_ascii_graphic)
        {
            return Err(TokenError::Content {
                path: path.to_owned(),
            });
        }
        Ok(Self::new(token))
    }
}

/// The tokens the proxy checks; a missing one refuses every call that needs it.
#[derive(Clone, Debug, Default)]
pub struct Tokens {
    /// The Buck2 API key (`buck2_auth = "bearer"`).
    pub buck2: Option<Token>,
    /// The operator token, which is also the update credential.
    pub operator: Option<Token>,
}

/// What a request carried.
#[derive(Clone, Copy, Debug, Default)]
pub struct Presented<'a> {
    /// The verified certificate's identity.
    pub identity: Option<&'a Identity>,
    /// The bearer token from `Authorization: Bearer`, never from a query string.
    pub bearer: Option<&'a [u8]>,
}

impl Tokens {
    /// The tokens of their files: the operator token always, the Buck2 key when
    /// `buck2_file` is given.
    ///
    /// # Errors
    /// [`TokenError`] for the first file that is refused.
    pub fn from_files(operator_file: &Path, buck2_file: Option<&Path>) -> Result<Self, TokenError> {
        Ok(Self {
            operator: Some(Token::from_file(operator_file)?),
            buck2: buck2_file.map(Token::from_file).transpose()?,
        })
    }
}

/// The token an `Authorization: Bearer <token>` header presents: the scheme in any case, the
/// spaces after it skipped, nothing after the token trimmed (a trailing byte is part of what
/// is compared). `None` for no header, another scheme or a value that is not text.
#[must_use]
pub fn bearer_of(headers: &HeaderMap) -> Option<&[u8]> {
    let value = headers.get(header::AUTHORIZATION)?.as_bytes();
    let space = value.iter().position(|&b| b == b' ')?;
    let (scheme, token) = value.split_at(space);
    scheme
        .eq_ignore_ascii_case(b"bearer")
        .then(|| token.trim_ascii_start())
}

fn token_ok(expected: Option<&Token>, presented: Presented<'_>) -> Result<(), Refusal> {
    match (expected, presented.bearer) {
        (Some(t), Some(b)) if t.admits(b) => Ok(()),
        _ => Err(Refusal::Unauthenticated),
    }
}

/// Decides one call.
///
/// # Errors
/// The [`Refusal`] to answer with.
pub fn decide(
    class: AuthClass,
    mode: Buck2Auth,
    tokens: &Tokens,
    presented: Presented<'_>,
) -> Result<(), Refusal> {
    match class {
        AuthClass::Probe | AuthClass::Enroll | AuthClass::Ui => Ok(()),
        AuthClass::Operator | AuthClass::Update => token_ok(tokens.operator.as_ref(), presented),
        AuthClass::Worker => match presented.identity {
            None => Err(Refusal::Unauthenticated),
            Some(id) if matches!(id.role, PeerRole::Daemon | PeerRole::Storage) => Ok(()),
            Some(_) => Err(Refusal::PermissionDenied),
        },
        AuthClass::Reapi => match mode {
            Buck2Auth::None => Ok(()),
            // A certificate of any farm role but a host updater's, which is for the
            // coordinator and its frontend's worker listener, and no build-cache credential.
            Buck2Auth::Mtls
                if presented
                    .identity
                    .is_some_and(|id| id.role != PeerRole::Updater) =>
            {
                Ok(())
            }
            Buck2Auth::Mtls if presented.identity.is_some() => Err(Refusal::PermissionDenied),
            Buck2Auth::Mtls => Err(Refusal::Unauthenticated),
            Buck2Auth::Bearer => token_ok(tokens.buck2.as_ref(), presented),
        },
    }
}

/// Whether [`decide`] will hash a presented bearer token for this class and mode.
#[must_use]
pub const fn checks_token(class: AuthClass, mode: Buck2Auth) -> bool {
    match class {
        AuthClass::Operator | AuthClass::Update => true,
        AuthClass::Reapi => matches!(mode, Buck2Auth::Bearer),
        AuthClass::Worker | AuthClass::Enroll | AuthClass::Probe | AuthClass::Ui => false,
    }
}

/// How many wrong tokens one address may present in a row before its tokens are not looked at.
pub const FAILURE_BURST: u32 = 10;
/// How long an address takes to earn one wrong token back.
pub const FAILURE_REFILL: Duration = Duration::from_secs(1);
/// The most addresses remembered at once.
const MAX_TRACKED: usize = 10_000;

/// Wrong tokens per address, as a bucket that drains one per [`FAILURE_REFILL`].
///
/// It counts failures and not calls: a Buck2 client sends its key on every call, thousands a
/// second, and must not be slowed down for being right. An address past [`FAILURE_BURST`]
/// failures is refused before its token is hashed, until it has earned a place back.
#[derive(Debug, Default)]
pub struct FailureLimit {
    buckets: Mutex<HashMap<IpAddr, Bucket>>,
}

#[derive(Clone, Copy, Debug)]
struct Bucket {
    failures: u32,
    /// The instant `failures` was last brought up to date.
    at: Instant,
}

impl Bucket {
    /// Drains the failures `now` has earned back.
    fn refresh(&mut self, now: Instant) {
        let earned = now.saturating_duration_since(self.at).as_nanos() / FAILURE_REFILL.as_nanos();
        let earned = u32::try_from(earned).unwrap_or(u32::MAX);
        self.failures = self.failures.saturating_sub(earned);
        // Keep the part of a period not yet earned; with nothing owed there is none.
        self.at = if self.failures == 0 {
            now
        } else {
            self.at + FAILURE_REFILL * earned
        };
    }
}

impl FailureLimit {
    /// An empty limit.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether `ip` may have its token looked at.
    #[must_use]
    pub fn allows(&self, ip: IpAddr, now: Instant) -> bool {
        let mut buckets = self.buckets.lock().unwrap_or_else(PoisonError::into_inner);
        buckets.get_mut(&ip).is_none_or(|bucket| {
            bucket.refresh(now);
            bucket.failures < FAILURE_BURST
        })
    }

    /// Notes one wrong token from `ip`.
    pub fn record(&self, ip: IpAddr, now: Instant) {
        let mut buckets = self.buckets.lock().unwrap_or_else(PoisonError::into_inner);
        if buckets.len() >= MAX_TRACKED && !buckets.contains_key(&ip) {
            // Forget the addresses that owe nothing; if every one owes something, forget an
            // arbitrary one: memory stays bounded, and each address keeps its own burst.
            buckets.retain(|_, bucket| {
                bucket.refresh(now);
                bucket.failures > 0
            });
            if buckets.len() >= MAX_TRACKED {
                let victim = buckets.keys().next().copied();
                victim.iter().for_each(|ip| {
                    buckets.remove(ip);
                });
            }
        }
        let bucket = buckets.entry(ip).or_insert(Bucket {
            failures: 0,
            at: now,
        });
        bucket.refresh(now);
        bucket.failures = (bucket.failures + 1).min(FAILURE_BURST);
    }

    /// How many addresses are remembered.
    #[must_use]
    pub fn tracked(&self) -> usize {
        self.buckets
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::files::FileError;

    fn id(role: PeerRole) -> Identity {
        Identity {
            node_id: "n.farm".into(),
            role,
            ips: Vec::new(),
        }
    }

    fn tokens() -> Tokens {
        Tokens {
            buck2: Some(Token::new(b"buck2-key")),
            operator: Some(Token::new(b"op-key")),
        }
    }

    fn call(class: AuthClass, mode: Buck2Auth, p: Presented<'_>) -> Result<(), Refusal> {
        decide(class, mode, &tokens(), p)
    }

    #[test]
    fn worker_paths_need_a_worker_or_storage_certificate() {
        let none = Presented::default();
        assert_eq!(
            call(AuthClass::Worker, Buck2Auth::None, none),
            Err(Refusal::Unauthenticated)
        );
        for (role, ok) in [
            (PeerRole::Daemon, true),
            (PeerRole::Storage, true),
            (PeerRole::Frontend, false),
            (PeerRole::Updater, false),
        ] {
            let i = id(role);
            let p = Presented {
                identity: Some(&i),
                bearer: Some(b"op-key".as_slice()),
            };
            let want = if ok {
                Ok(())
            } else {
                Err(Refusal::PermissionDenied)
            };
            assert_eq!(
                call(AuthClass::Worker, Buck2Auth::None, p),
                want,
                "{role:?}"
            );
        }
    }

    #[test]
    fn reapi_follows_buck2_auth() {
        let i = id(PeerRole::Daemon);
        let none = Presented::default();
        let cert = Presented {
            identity: Some(&i),
            bearer: None,
        };
        let good = Presented {
            identity: None,
            bearer: Some(b"buck2-key"),
        };
        let bad = Presented {
            identity: Some(&i),
            bearer: Some(b"nope"),
        };
        assert_eq!(call(AuthClass::Reapi, Buck2Auth::None, none), Ok(()));
        assert_eq!(
            call(AuthClass::Reapi, Buck2Auth::Mtls, none),
            Err(Refusal::Unauthenticated)
        );
        assert_eq!(call(AuthClass::Reapi, Buck2Auth::Mtls, cert), Ok(()));
        assert_eq!(call(AuthClass::Reapi, Buck2Auth::Bearer, good), Ok(()));
        assert_eq!(
            call(AuthClass::Reapi, Buck2Auth::Bearer, cert),
            Err(Refusal::Unauthenticated)
        );
        // A good certificate does not rescue a wrong token.
        assert_eq!(
            call(AuthClass::Reapi, Buck2Auth::Bearer, bad),
            Err(Refusal::Unauthenticated)
        );
        // The operator token is not the Buck2 key.
        let op = Presented {
            identity: None,
            bearer: Some(b"op-key".as_slice()),
        };
        assert_eq!(
            call(AuthClass::Reapi, Buck2Auth::Bearer, op),
            Err(Refusal::Unauthenticated)
        );
    }

    #[test]
    fn operator_and_update_need_the_operator_token_whatever_the_certificate() {
        let i = id(PeerRole::Storage);
        let good = Presented {
            identity: None,
            bearer: Some(b"op-key".as_slice()),
        };
        let cert_only = Presented {
            identity: Some(&i),
            bearer: None,
        };
        for class in [AuthClass::Operator, AuthClass::Update] {
            assert_eq!(call(class, Buck2Auth::None, good), Ok(()));
            assert_eq!(
                call(class, Buck2Auth::None, cert_only),
                Err(Refusal::Unauthenticated)
            );
            assert_eq!(
                decide(class, Buck2Auth::None, &Tokens::default(), good),
                Err(Refusal::Unauthenticated)
            );
        }
        assert_eq!(
            call(AuthClass::Probe, Buck2Auth::Mtls, Presented::default()),
            Ok(())
        );
    }

    #[test]
    fn refusals_map_to_statuses() {
        assert_eq!(Refusal::Unauthenticated.grpc_status(), 16);
        assert_eq!(Refusal::PermissionDenied.grpc_status(), 7);
        assert_eq!(Refusal::RateLimited.grpc_status(), 8);
        assert_eq!(Refusal::Unauthenticated.http_status(), 401);
        assert_eq!(Refusal::PermissionDenied.http_status(), 403);
        assert_eq!(Refusal::RateLimited.http_status(), 429);
        assert_eq!(format!("{:?}", Token::new(b"x")), "Token(..)");
        assert!(!Token::new(b"x").admits(b""));
    }

    #[test]
    fn enrollment_probes_and_the_ui_take_no_credential() {
        for mode in [Buck2Auth::None, Buck2Auth::Mtls, Buck2Auth::Bearer] {
            for class in [AuthClass::Enroll, AuthClass::Probe, AuthClass::Ui] {
                let none = Tokens::default();
                assert_eq!(decide(class, mode, &none, Presented::default()), Ok(()));
            }
        }
    }

    /// A token is the whole token: not a prefix of it, not it with one byte more, not the
    /// other token, not the empty one.
    #[test]
    fn a_token_admits_only_itself() {
        let token = Token::new(b"buck2-key-0123456789");
        assert!(token.admits(b"buck2-key-0123456789"));
        for wrong in [
            &b"buck2-key-012345678"[..],
            b"buck2-key-01234567890",
            b"buck2-key-0123456780",
            b"Buck2-key-0123456789",
            b"",
        ] {
            assert!(!token.admits(wrong), "{wrong:?}");
        }
    }

    fn headers(authorization: &[u8]) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(
            header::AUTHORIZATION,
            http::HeaderValue::from_bytes(authorization).expect("a value"),
        );
        h
    }

    #[test]
    fn the_bearer_is_read_as_the_child_reads_it() {
        assert_eq!(bearer_of(&headers(b"Bearer abc")), Some(&b"abc"[..]));
        assert_eq!(bearer_of(&headers(b"bearer   abc")), Some(&b"abc"[..]));
        assert_eq!(bearer_of(&headers(b"BEARER abc ")), Some(&b"abc "[..]));
        assert_eq!(bearer_of(&headers(b"Bearer \xff")), Some(&b"\xff"[..]));
        assert_eq!(bearer_of(&headers(b"Basic abc")), None);
        assert_eq!(bearer_of(&headers(b"Bearer")), None);
        assert_eq!(bearer_of(&HeaderMap::new()), None);
    }

    #[test]
    fn only_the_classes_that_hash_a_token_are_limited() {
        for class in [AuthClass::Operator, AuthClass::Update] {
            assert!(checks_token(class, Buck2Auth::None));
        }
        assert!(checks_token(AuthClass::Reapi, Buck2Auth::Bearer));
        for mode in [Buck2Auth::None, Buck2Auth::Mtls] {
            assert!(!checks_token(AuthClass::Reapi, mode));
        }
        for class in [
            AuthClass::Worker,
            AuthClass::Enroll,
            AuthClass::Probe,
            AuthClass::Ui,
        ] {
            assert!(!checks_token(class, Buck2Auth::Bearer));
        }
    }

    fn ip(n: u32) -> IpAddr {
        IpAddr::V4(std::net::Ipv4Addr::from(0xc000_0200 + n))
    }

    #[test]
    fn an_address_earns_its_failures_back_one_period_at_a_time() {
        let limit = FailureLimit::new();
        let t0 = Instant::now();
        assert!(limit.allows(ip(1), t0), "an address never seen");
        for _ in 0..FAILURE_BURST {
            limit.record(ip(1), t0);
        }
        limit.record(ip(1), t0);
        assert!(!limit.allows(ip(1), t0));
        assert!(limit.allows(ip(2), t0), "another address");
        // A period and a half earns one place; the half is not lost.
        let t1 = t0 + FAILURE_REFILL + FAILURE_REFILL / 2;
        assert!(limit.allows(ip(1), t1));
        limit.record(ip(1), t1);
        assert!(!limit.allows(ip(1), t1));
        assert!(limit.allows(ip(1), t0 + FAILURE_REFILL * 2));
        // Ten periods clear it, and the entry is forgotten by the next sweep.
        let t2 = t0 + FAILURE_REFILL * (FAILURE_BURST + 1);
        assert!(limit.allows(ip(1), t2));
        assert!(limit.allows(ip(1), t2 + FAILURE_REFILL * 100));
    }

    #[test]
    fn the_tracked_addresses_are_bounded() {
        let limit = FailureLimit::new();
        let t0 = Instant::now();
        let most = u32::try_from(MAX_TRACKED).expect("small");
        for n in 0..most {
            limit.record(ip(n), t0);
        }
        assert_eq!(limit.tracked(), MAX_TRACKED);
        // An address already remembered needs no room.
        limit.record(ip(0), t0);
        assert_eq!(limit.tracked(), MAX_TRACKED);
        // Everyone still owes a failure: one is forgotten to make room.
        limit.record(ip(most), t0);
        assert_eq!(limit.tracked(), MAX_TRACKED);
        assert!(limit.allows(ip(most), t0));
        // Later, everyone has earned theirs back: the sweep forgets them all.
        limit.record(ip(most + 1), t0 + FAILURE_REFILL * 5);
        assert_eq!(limit.tracked(), 1);
    }

    /// The numbers the design note and the child promise, as literals: ten wrong tokens in a
    /// row, one earned back a second, ten thousand addresses, thirty-two bytes of token.
    #[test]
    fn the_promised_limits_are_the_constants() {
        assert_eq!(FAILURE_BURST, 10);
        assert_eq!(FAILURE_REFILL, Duration::from_secs(1));
        assert_eq!(MAX_TRACKED, 10_000);
        assert_eq!(MIN_TOKEN_BYTES, 32);
        assert_eq!(MAX_TOKEN_FILE_BYTES, 4096);
    }

    #[test]
    fn a_token_file_is_read_under_the_childs_rules() {
        let dir = crate::scratch::scratch("token-files");
        let write = |name: &str, content: &[u8], mode: u32| {
            let path = dir.join(name);
            crate::scratch::write_file(&path, content, mode);
            path
        };
        let token = "0123456789abcdef0123456789abcdef";
        let good = write("good", format!("{token}\n").as_bytes(), 0o600);
        assert!(
            Token::from_file(&good)
                .expect("a token")
                .admits(token.as_bytes())
        );
        let read_only = write("read-only", token.as_bytes(), 0o400);
        assert!(Token::from_file(&read_only).is_ok());

        let bad = |path: &std::path::Path| Token::from_file(path).expect_err("refused");
        assert!(matches!(
            bad(&write("short", b"too-short", 0o600)),
            TokenError::Content { .. }
        ));
        // The child's floor is 32 bytes: one fewer is refused, exactly 32 is a token.
        let below = write("below", &[b'a'; 31], 0o600);
        assert!(matches!(bad(&below), TokenError::Content { .. }));
        assert!(Token::from_file(&write("floor", &[b'a'; 32], 0o600)).is_ok());
        let spaced = write("spaced", b"0123456789abcdef 123456789abcdef0", 0o600);
        assert!(matches!(bad(&spaced), TokenError::Content { .. }));
        let big = write("big", &vec![b'a'; MAX_TOKEN_FILE_BYTES + 1], 0o600);
        assert!(matches!(bad(&big), TokenError::Content { .. }));
        let open = write("open", token.as_bytes(), 0o644);
        assert!(matches!(
            bad(&open),
            TokenError::File(FileError::Mode { .. })
        ));
        assert!(matches!(
            bad(&dir.join("absent")),
            TokenError::File(FileError::Read { .. })
        ));
        assert!(matches!(
            bad(&dir),
            TokenError::File(FileError::NotAFile { .. })
        ));

        let both = Tokens::from_files(&good, Some(&read_only)).expect("both");
        assert!(both.operator.is_some());
        assert!(both.buck2.is_some());
        let one = Tokens::from_files(&good, None).expect("one");
        assert!(one.buck2.is_none());
        assert!(Tokens::from_files(&good, Some(&open)).is_err());
    }

    /// With `buck2_auth = "mtls"` a certificate of the farm's own roles opens the build cache and
    /// a host updater's does not: it is for the update calls.
    #[test]
    fn an_updater_certificate_is_no_build_cache_credential() {
        let updater = id(PeerRole::Updater);
        let presented = Presented {
            identity: Some(&updater),
            bearer: None,
        };
        assert_eq!(
            call(AuthClass::Reapi, Buck2Auth::Mtls, presented),
            Err(Refusal::PermissionDenied)
        );
        for role in [PeerRole::Daemon, PeerRole::Storage, PeerRole::Frontend] {
            let i = id(role);
            let presented = Presented {
                identity: Some(&i),
                bearer: None,
            };
            assert_eq!(call(AuthClass::Reapi, Buck2Auth::Mtls, presented), Ok(()));
        }
    }

    /// A host updater's certificate opens none of the operator or update paths at the proxy: those
    /// take the operator token whatever the certificate says, and the frontend serves an updater
    /// its own-node calls itself.
    #[test]
    fn an_updater_certificate_is_no_operator_credential() {
        let updater = id(PeerRole::Updater);
        let presented = Presented {
            identity: Some(&updater),
            bearer: None,
        };
        for class in [AuthClass::Operator, AuthClass::Update] {
            assert_eq!(
                call(class, Buck2Auth::Mtls, presented),
                Err(Refusal::Unauthenticated),
                "{class:?}"
            );
        }
        let wrong = Presented {
            identity: Some(&updater),
            bearer: Some(b"not-the-token".as_slice()),
        };
        assert_eq!(
            call(AuthClass::Operator, Buck2Auth::Mtls, wrong),
            Err(Refusal::Unauthenticated)
        );
        let right = Presented {
            identity: Some(&updater),
            bearer: Some(b"op-key".as_slice()),
        };
        assert_eq!(call(AuthClass::Operator, Buck2Auth::Mtls, right), Ok(()));
    }
}
