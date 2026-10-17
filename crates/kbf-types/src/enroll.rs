//! Enrollment vocabulary: the types a token, a node and a refusal are made of
//! (`docs/design/install-and-enrollment.md`, sections 5.1 and 5.3).
//!
//! The wire messages are `kbf.enroll.v1` in `kbf-proto`; this module holds the values
//! they carry, in the one spelling every crate shares:
//!
//! - the token string `kbft1.<id>.<secret>.<pin>`, its three parts and the refusal of a
//!   malformed one that names the part and never repeats what was typed;
//! - the node id and the role set a token can grant;
//! - the closed set of reasons a frontend refuses an `Enroll` or `Renew`, each with its
//!   metric name, gRPC status code and message.
//!
//! Nothing here draws randomness or hashes: the parts are built from bytes a caller
//! drew (`TokenSecret::from_bytes`) or hashed (`CaPin::from_bytes`), so the crate stays
//! pure.

use std::fmt;
use std::str::FromStr;

/// The token format version, the first part of a token string.
const TOKEN_VERSION: &str = "kbft1";
/// Characters of a [`TokenId`]: 60 bits as lower-case base32.
const TOKEN_ID_LEN: usize = 12;
/// Characters of 32 bytes as base64url without padding.
const B64_32_LEN: usize = 43;

const BASE32: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
const BASE64URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

fn base64url_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let word =
            chunk.iter().fold(0u32, |w, b| (w << 8) | u32::from(*b)) << (8 * (3 - chunk.len()));
        for i in 0..=chunk.len() {
            out.push(char::from(
                BASE64URL[((word >> (18 - 6 * i)) & 63) as usize],
            ));
        }
    }
    out
}

/// Decodes exactly 32 bytes of unpadded base64url, refusing every other spelling of
/// them: a wrong length, a character outside the alphabet (padding included) and a last
/// character whose unused bits are not zero, so one value has one spelling.
fn base64url_decode_32(text: &str) -> Result<[u8; 32], PartFault> {
    if text.len() != B64_32_LEN {
        return Err(PartFault::Length);
    }
    let mut out = [0u8; 32];
    let (mut filled, mut acc, mut bits) = (0usize, 0u32, 0u32);
    for c in text.bytes() {
        let value = BASE64URL
            .iter()
            .position(|a| *a == c)
            .ok_or(PartFault::Alphabet)?;
        acc = (acc << 6) | value as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out[filled] = (acc >> bits) as u8;
            filled += 1;
            acc &= (1 << bits) - 1;
        }
    }
    // 43 characters carry 258 bits: 32 bytes and two bits that must be zero.
    if acc != 0 {
        return Err(PartFault::NonCanonical);
    }
    Ok(out)
}

/// What is wrong with one part of a token string. Never carries the text itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PartFault {
    /// The part has the wrong number of characters.
    #[error("has the wrong length")]
    Length,
    /// The part has a character outside its alphabet.
    #[error("has a character outside its alphabet")]
    Alphabet,
    /// The part decodes but is not the canonical spelling of its value.
    #[error("is not the canonical spelling of its value")]
    NonCanonical,
}

/// Why a string is not a token. No variant holds any of the input, so a message built
/// from one cannot echo a secret.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TokenError {
    /// The string does not have the four dot-separated parts.
    #[error("a token has four dot-separated parts, this one has {0}")]
    Parts(usize),
    /// The first part is not the format version this build reads.
    #[error("the token format version is not {TOKEN_VERSION}")]
    Version,
    /// The id part is malformed.
    #[error("the token id {0}")]
    Id(PartFault),
    /// The secret part is malformed.
    #[error("the token secret {0}")]
    Secret(PartFault),
    /// The CA pin part is malformed.
    #[error("the token CA pin {0}")]
    Pin(PartFault),
}

/// The lookup key of a token: 12 characters of lower-case base32 (60 bits). Not secret.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TokenId(String);

impl TokenId {
    /// The id for the low 60 bits of `bits` (the caller draws them from a CSPRNG).
    #[must_use]
    pub fn from_bits(bits: u64) -> Self {
        let text = (0..TOKEN_ID_LEN)
            .map(|i| char::from(BASE32[((bits >> (5 * (TOKEN_ID_LEN - 1 - i))) & 31) as usize]))
            .collect();
        Self(text)
    }

    /// The id spelt `text`, if it is 12 characters of `a-z2-7`.
    pub fn parse(text: &str) -> Result<Self, PartFault> {
        if text.len() != TOKEN_ID_LEN {
            return Err(PartFault::Length);
        }
        if !text.bytes().all(|c| BASE32.contains(&c)) {
            return Err(PartFault::Alphabet);
        }
        Ok(Self(text.to_owned()))
    }

    /// The id as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TokenId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The 32 secret bytes of a token. Has no `Display`, `PartialEq` or `Clone`, and its
/// `Debug` prints `TokenSecret(..)`: the only way to read it is [`TokenSecret::as_bytes`].
pub struct TokenSecret([u8; 32]);

impl TokenSecret {
    /// The secret made of `bytes` (the caller draws them from a CSPRNG).
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// The secret bytes, as an `Enroll` request carries them.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Debug for TokenSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("TokenSecret(..)")
    }
}

/// The SHA-256 of the DER of the farm's root CA certificate: what a node checks the
/// frontend against at first contact (design section 5.4). Public.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct CaPin([u8; 32]);

impl CaPin {
    /// The pin for a root certificate whose SHA-256 is `sha256`.
    #[must_use]
    pub const fn from_bytes(sha256: [u8; 32]) -> Self {
        Self(sha256)
    }

    /// The hash bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Display for CaPin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&base64url_encode(&self.0))
    }
}

impl fmt::Debug for CaPin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "CaPin({self})")
    }
}

/// An enrollment token: `kbft1.<id>.<secret>.<pin>`, one line of ASCII (design section
/// 5.1). Parsing is strict: no whitespace, no padding, one spelling per value.
pub struct Token {
    id: TokenId,
    secret: TokenSecret,
    pin: CaPin,
}

impl Token {
    /// A token of its three parts.
    #[must_use]
    pub const fn new(id: TokenId, secret: TokenSecret, pin: CaPin) -> Self {
        Self { id, secret, pin }
    }

    /// The lookup id.
    #[must_use]
    pub const fn id(&self) -> &TokenId {
        &self.id
    }

    /// The secret.
    #[must_use]
    pub const fn secret(&self) -> &TokenSecret {
        &self.secret
    }

    /// The CA pin.
    #[must_use]
    pub const fn pin(&self) -> &CaPin {
        &self.pin
    }

    /// The token string, secret included. Named for what it does because the type has no
    /// `Display`: call it where the token is shown once, or written to its file.
    #[must_use]
    pub fn to_line(&self) -> String {
        format!(
            "{TOKEN_VERSION}.{}.{}.{}",
            self.id,
            base64url_encode(&self.secret.0),
            self.pin
        )
    }
}

impl FromStr for Token {
    type Err = TokenError;

    fn from_str(text: &str) -> Result<Self, TokenError> {
        let parts: Vec<&str> = text.split('.').collect();
        let [version, id, secret, pin] = parts[..] else {
            return Err(TokenError::Parts(parts.len()));
        };
        if version != TOKEN_VERSION {
            return Err(TokenError::Version);
        }
        Ok(Self {
            id: TokenId::parse(id).map_err(TokenError::Id)?,
            secret: TokenSecret(base64url_decode_32(secret).map_err(TokenError::Secret)?),
            pin: CaPin(base64url_decode_32(pin).map_err(TokenError::Pin)?),
        })
    }
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Token(..)")
    }
}

/// Why a string is not a [`NodeId`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum NodeIdError {
    /// The id is empty.
    #[error("a node id is empty")]
    Empty,
    /// The id is longer than 63 characters.
    #[error("a node id is longer than {} characters", NodeId::MAX_LEN)]
    TooLong,
    /// The id has a character other than `a-z`, `0-9` and `-`.
    #[error("a node id has a character outside a-z, 0-9 and -")]
    Character,
    /// The id starts or ends with `-`.
    #[error("a node id starts or ends with -")]
    Edge,
}

/// A node's id: `^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$`. It becomes the certificate's one
/// DNS name, the member `instance_id` and the `Hello` `node_id`, so it is a valid DNS
/// label and nothing else.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeId(String);

impl NodeId {
    /// The longest id: a DNS label.
    pub const MAX_LEN: usize = 63;

    /// The id as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for NodeId {
    type Err = NodeIdError;

    fn from_str(text: &str) -> Result<Self, NodeIdError> {
        if text.is_empty() {
            return Err(NodeIdError::Empty);
        }
        if text.len() > Self::MAX_LEN {
            return Err(NodeIdError::TooLong);
        }
        if !text
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
        {
            return Err(NodeIdError::Character);
        }
        if text.starts_with('-') || text.ends_with('-') {
            return Err(NodeIdError::Edge);
        }
        Ok(Self(text.to_owned()))
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Why a string is not a role a token can grant.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RoleError {
    /// `frontend` is a real role of the farm but no token grants it.
    #[error("the frontend role cannot be granted by a token")]
    Frontend,
    /// Not a role at all.
    #[error("not a role a token can grant")]
    Unknown,
}

/// A role a token can grant. The farm has a third, `frontend`, which a frontend gives
/// only to itself (design section 6.3), and no "client" role.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Role {
    /// Serves blobs and the action cache.
    Storage,
    /// Runs actions.
    Daemon,
}

impl Role {
    /// Every grantable role, in the order a request's certificates are listed.
    pub const ALL: [Self; 2] = [Self::Storage, Self::Daemon];

    /// Its name: the `RoleCsr.role` and `IssuedCert.role` spelling, and the last part of
    /// the role URI `urn:kbf:role:<name>` in the certificate.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Storage => "storage",
            Self::Daemon => "daemon",
        }
    }

    const fn bit(self) -> u8 {
        match self {
            Self::Storage => 1,
            Self::Daemon => 2,
        }
    }
}

impl FromStr for Role {
    type Err = RoleError;

    fn from_str(name: &str) -> Result<Self, RoleError> {
        if name == "frontend" {
            return Err(RoleError::Frontend);
        }
        Self::ALL
            .into_iter()
            .find(|role| role.name() == name)
            .ok_or(RoleError::Unknown)
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Why a list of roles is not a [`RoleSet`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RoleSetError {
    /// No roles.
    #[error("no roles")]
    Empty,
    /// A role named twice.
    #[error("role {0} is named twice")]
    Duplicate(Role),
    /// A name that is not a grantable role.
    #[error(transparent)]
    Role(#[from] RoleError),
}

/// A non-empty set of roles, listed in [`Role::ALL`] order whatever order it was built in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RoleSet(u8);

impl RoleSet {
    /// The set of `roles`; refuses none and a repeat.
    pub fn new(roles: impl IntoIterator<Item = Role>) -> Result<Self, RoleSetError> {
        let mut bits = 0u8;
        for role in roles {
            if bits & role.bit() != 0 {
                return Err(RoleSetError::Duplicate(role));
            }
            bits |= role.bit();
        }
        if bits == 0 {
            return Err(RoleSetError::Empty);
        }
        Ok(Self(bits))
    }

    /// The set of roles spelt `names`; as [`RoleSet::new`], and refuses a name that is
    /// not a grantable role.
    pub fn from_names<'a>(names: impl IntoIterator<Item = &'a str>) -> Result<Self, RoleSetError> {
        let roles = names
            .into_iter()
            .map(str::parse)
            .collect::<Result<Vec<Role>, _>>()?;
        Self::new(roles)
    }

    /// Whether `role` is in the set.
    #[must_use]
    pub const fn contains(self, role: Role) -> bool {
        self.0 & role.bit() != 0
    }

    /// The roles, in [`Role::ALL`] order.
    pub fn iter(self) -> impl Iterator<Item = Role> {
        Role::ALL
            .into_iter()
            .filter(move |role| self.contains(*role))
    }
}

/// The gRPC status code a refusal answers with. The numbers are `google.rpc.Code`'s.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefusalCode {
    /// The request is malformed or names something the frontend will not take.
    InvalidArgument,
    /// The caller did not prove itself.
    Unauthenticated,
    /// The caller proved itself and may not do this.
    PermissionDenied,
    /// The thing to create exists.
    AlreadyExists,
    /// A rate limit; try later.
    ResourceExhausted,
    /// The frontend cannot answer now; try again.
    Unavailable,
}

impl RefusalCode {
    /// The `google.rpc.Code` number.
    #[must_use]
    pub const fn number(self) -> i32 {
        match self {
            Self::InvalidArgument => 3,
            Self::Unauthenticated => 16,
            Self::PermissionDenied => 7,
            Self::AlreadyExists => 6,
            Self::ResourceExhausted => 8,
            Self::Unavailable => 14,
        }
    }

    /// Whether the same request can succeed later with no change: a node retries these
    /// with backoff and gives up on the others.
    #[must_use]
    pub const fn retryable(self) -> bool {
        matches!(self, Self::ResourceExhausted | Self::Unavailable)
    }
}

/// Why a frontend refused an `Enroll` or a `Renew`: the closed set. A reason names its
/// metric label (`kbf_enroll_refused_total{reason}`), its status code and the message the
/// frontend sends; a frontend refuses with nothing else, so a caller can match on it.
///
/// The token reasons after the first are told only to a caller who has proved the secret
/// (design section 5.1); to anyone else the answer is [`RefusalReason::TokenRefused`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RefusalReason {
    /// The token id is unknown or the secret is wrong; the two are not told apart.
    TokenRefused,
    /// The token's expiry has passed.
    TokenExpired,
    /// The token has no uses left.
    TokenUsedUp,
    /// The token was revoked.
    TokenRevoked,
    /// The request is malformed; the status names the field.
    MalformedRequest,
    /// A CSR is too large, does not parse, does not prove its key, or has a key that is
    /// not ECDSA P-256.
    BadCsr,
    /// The requested roles are empty, repeated or not a role a token can grant.
    BadRoles,
    /// A role the token does not grant.
    RoleNotGranted,
    /// The node id is not the exact id or prefix the token allows.
    NodeIdNotAllowed,
    /// The node id is already enrolled and the token cannot replace it.
    NodeIdInUse,
    /// The advertised host is outside the names the token allows.
    AdvertiseNotAllowed,
    /// The advertised host is the frontend's own name, loopback, unspecified, link-local
    /// or multicast, or malformed.
    AdvertiseInvalid,
    /// The advertised host belongs to another node.
    AdvertiseInUse,
    /// A renewal for a node the registry does not hold.
    UnknownNode,
    /// The node is revoked.
    NodeRevoked,
    /// A renewal signed by a key other than the one the registry holds.
    WrongKey,
    /// A renewal whose last certificate expired too long ago; it must enroll again.
    RenewalExpired,
    /// A renewal whose `issued_at` is too far from the frontend's clock.
    StaleIssuedAt,
    /// A renewal repeated, or sooner than the frontend allows.
    RenewTooSoon,
    /// A rate limit or the concurrency cap.
    RateLimited,
    /// The state store cannot be reached.
    StoreUnavailable,
    /// This process is not the active signer.
    NotSigner,
}

impl RefusalReason {
    /// Every reason.
    pub const ALL: [Self; 22] = [
        Self::TokenRefused,
        Self::TokenExpired,
        Self::TokenUsedUp,
        Self::TokenRevoked,
        Self::MalformedRequest,
        Self::BadCsr,
        Self::BadRoles,
        Self::RoleNotGranted,
        Self::NodeIdNotAllowed,
        Self::NodeIdInUse,
        Self::AdvertiseNotAllowed,
        Self::AdvertiseInvalid,
        Self::AdvertiseInUse,
        Self::UnknownNode,
        Self::NodeRevoked,
        Self::WrongKey,
        Self::RenewalExpired,
        Self::StaleIssuedAt,
        Self::RenewTooSoon,
        Self::RateLimited,
        Self::StoreUnavailable,
        Self::NotSigner,
    ];

    /// The metric label: lower-case words joined by `_`.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::TokenRefused => "token_refused",
            Self::TokenExpired => "token_expired",
            Self::TokenUsedUp => "token_used_up",
            Self::TokenRevoked => "token_revoked",
            Self::MalformedRequest => "malformed_request",
            Self::BadCsr => "bad_csr",
            Self::BadRoles => "bad_roles",
            Self::RoleNotGranted => "role_not_granted",
            Self::NodeIdNotAllowed => "node_id_not_allowed",
            Self::NodeIdInUse => "node_id_in_use",
            Self::AdvertiseNotAllowed => "advertise_not_allowed",
            Self::AdvertiseInvalid => "advertise_invalid",
            Self::AdvertiseInUse => "advertise_in_use",
            Self::UnknownNode => "unknown_node",
            Self::NodeRevoked => "node_revoked",
            Self::WrongKey => "wrong_key",
            Self::RenewalExpired => "renewal_expired",
            Self::StaleIssuedAt => "stale_issued_at",
            Self::RenewTooSoon => "renew_too_soon",
            Self::RateLimited => "rate_limited",
            Self::StoreUnavailable => "store_unavailable",
            Self::NotSigner => "not_signer",
        }
    }

    /// The reason named `name`, if any.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|reason| reason.name() == name)
    }

    /// The status code the refusal answers with (design section 5.10).
    #[must_use]
    pub const fn code(self) -> RefusalCode {
        match self {
            Self::TokenRefused | Self::TokenExpired | Self::TokenUsedUp | Self::TokenRevoked => {
                RefusalCode::Unauthenticated
            }
            Self::MalformedRequest
            | Self::BadCsr
            | Self::BadRoles
            | Self::AdvertiseInvalid
            | Self::StaleIssuedAt => RefusalCode::InvalidArgument,
            Self::RoleNotGranted
            | Self::NodeIdNotAllowed
            | Self::AdvertiseNotAllowed
            | Self::UnknownNode
            | Self::NodeRevoked
            | Self::WrongKey
            | Self::RenewalExpired => RefusalCode::PermissionDenied,
            Self::NodeIdInUse | Self::AdvertiseInUse => RefusalCode::AlreadyExists,
            Self::RenewTooSoon | Self::RateLimited => RefusalCode::ResourceExhausted,
            Self::StoreUnavailable | Self::NotSigner => RefusalCode::Unavailable,
        }
    }

    /// The status message the frontend sends. It names the reason and never a token, a
    /// secret or the caller's input.
    #[must_use]
    pub const fn message(self) -> &'static str {
        match self {
            Self::TokenRefused => "enrollment refused",
            Self::TokenExpired => "token expired",
            Self::TokenUsedUp => "token used up",
            Self::TokenRevoked => "token revoked",
            Self::MalformedRequest => "malformed request",
            Self::BadCsr => "bad certificate request",
            Self::BadRoles => "bad roles",
            Self::RoleNotGranted => "role not granted by the token",
            Self::NodeIdNotAllowed => "node id not allowed by the token",
            Self::NodeIdInUse => "node id already enrolled",
            Self::AdvertiseNotAllowed => "advertise host not allowed by the token",
            Self::AdvertiseInvalid => "advertise host not acceptable",
            Self::AdvertiseInUse => "advertise host belongs to another node",
            Self::UnknownNode => "node not enrolled",
            Self::NodeRevoked => "node revoked",
            Self::WrongKey => "request not signed by the node's key",
            Self::RenewalExpired => "certificate expired too long ago; enroll again",
            Self::StaleIssuedAt => "issued_at too far from the frontend's clock",
            Self::RenewTooSoon => "renewal too soon",
            Self::RateLimited => "rate limited",
            Self::StoreUnavailable => "state store unavailable",
            Self::NotSigner => "not the active signer",
        }
    }
}

impl fmt::Display for RefusalReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Built independently of the code: Python's `base64.urlsafe_b64encode` of
    // bytes 0..32 and of bytes 255 down to 224, and `base64.b32encode` of the id bits.
    const SECRET_TEXT: &str = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8";
    const PIN_TEXT: &str = "__79_Pv6-fj39vX08_Lx8O_u7ezr6uno5-bl5OPi4eA";
    const ID_TEXT: &str = "aerukz4jvpg6";
    const ID_BITS: u64 = 0x0012_3456_789a_bcde;
    const LINE: &str = "kbft1.aerukz4jvpg6.AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8.__79_Pv6-fj39vX08_Lx8O_u7ezr6uno5-bl5OPi4eA";

    fn secret_bytes() -> [u8; 32] {
        std::array::from_fn(|i| i as u8)
    }

    fn pin_bytes() -> [u8; 32] {
        std::array::from_fn(|i| 255 - i as u8)
    }

    fn line_with(id: &str, secret: &str, pin: &str) -> String {
        format!("kbft1.{id}.{secret}.{pin}")
    }

    fn parse(text: &str) -> Result<Token, TokenError> {
        text.parse()
    }

    /// Catches: a changed alphabet, bit order or padding rule in the encoder (the token
    /// string is a stored, pasted contract), and parts that do not round trip.
    #[test]
    fn the_token_line_is_golden_and_round_trips() {
        let token = Token::new(
            TokenId::from_bits(ID_BITS),
            TokenSecret::from_bytes(secret_bytes()),
            CaPin::from_bytes(pin_bytes()),
        );
        assert_eq!(token.to_line(), LINE);
        let back = parse(LINE).unwrap();
        assert_eq!(back.id().as_str(), ID_TEXT);
        assert_eq!(back.secret().as_bytes(), &secret_bytes());
        assert_eq!(back.pin().as_bytes(), &pin_bytes());
        assert_eq!(back.to_line(), LINE);
        assert_eq!(back.pin().to_string(), PIN_TEXT);
    }

    /// Catches: an id built from more than 60 bits (a 13th character, or a carry), from
    /// the wrong end of the integer, or with a character outside the base32 alphabet.
    #[test]
    fn ids_take_the_low_sixty_bits() {
        assert_eq!(TokenId::from_bits(ID_BITS).as_str(), ID_TEXT);
        assert_eq!(TokenId::from_bits(0).as_str(), "aaaaaaaaaaaa");
        assert_eq!(TokenId::from_bits(1).as_str(), "aaaaaaaaaaab");
        assert_eq!(TokenId::from_bits(1 << 60).as_str(), "aaaaaaaaaaaa");
        assert_eq!(TokenId::from_bits(u64::MAX).as_str(), "777777777777");
        assert_eq!(TokenId::from_bits(ID_BITS).to_string(), ID_TEXT);
    }

    /// Catches: the all-zero and all-one secrets mis-encoding at the last character (the
    /// two bits that carry no data).
    #[test]
    fn the_extremes_encode_to_their_canonical_spelling() {
        let zero = Token::new(
            TokenId::from_bits(0),
            TokenSecret::from_bytes([0; 32]),
            CaPin::from_bytes([0xff; 32]),
        )
        .to_line();
        let want = format!("kbft1.aaaaaaaaaaaa.{}.{}8", "A".repeat(43), "_".repeat(42));
        assert_eq!(zero, want);
        let back = parse(&zero).unwrap();
        assert_eq!(back.secret().as_bytes(), &[0; 32]);
        assert_eq!(back.pin().as_bytes(), &[0xff; 32]);
    }

    /// Catches: a malformed token accepted, or refused naming the wrong part.
    #[test]
    fn a_malformed_token_is_refused_naming_the_part() {
        use PartFault::{Alphabet, Length, NonCanonical};
        let ok_secret = SECRET_TEXT;
        let ok_pin = PIN_TEXT;
        let cases: Vec<(String, TokenError)> = vec![
            (String::new(), TokenError::Parts(1)),
            ("kbft1.a.b".to_owned(), TokenError::Parts(3)),
            (format!("{LINE}.x"), TokenError::Parts(5)),
            (LINE.replacen("kbft1", "kbft2", 1), TokenError::Version),
            (LINE.replacen("kbft1", "KBFT1", 1), TokenError::Version),
            (
                line_with("aerukz4jvpg", ok_secret, ok_pin),
                TokenError::Id(Length),
            ),
            (
                line_with("aerukz4jvpg66", ok_secret, ok_pin),
                TokenError::Id(Length),
            ),
            (
                line_with("AERUKZ4JVPG6", ok_secret, ok_pin),
                TokenError::Id(Alphabet),
            ),
            (
                line_with("aerukz4jvpg1", ok_secret, ok_pin),
                TokenError::Id(Alphabet),
            ),
            (
                line_with("aerukz4jvpg=", ok_secret, ok_pin),
                TokenError::Id(Alphabet),
            ),
            (
                line_with(ID_TEXT, &ok_secret[1..], ok_pin),
                TokenError::Secret(Length),
            ),
            (
                line_with(ID_TEXT, &format!("{ok_secret}A"), ok_pin),
                TokenError::Secret(Length),
            ),
            (
                line_with(ID_TEXT, &format!("{ok_secret}="), ok_pin),
                TokenError::Secret(Length),
            ),
            (
                line_with(ID_TEXT, &ok_secret.replacen('A', "+", 1), ok_pin),
                TokenError::Secret(Alphabet),
            ),
            (
                line_with(ID_TEXT, &format!("{}=", &ok_secret[1..]), ok_pin),
                TokenError::Secret(Alphabet),
            ),
            // The last character carries 4 data bits; 'B' sets a bit of the 2 unused ones.
            (
                line_with(ID_TEXT, &format!("{}B", &"A".repeat(42)), ok_pin),
                TokenError::Secret(NonCanonical),
            ),
            (
                line_with(ID_TEXT, ok_secret, &ok_pin[1..]),
                TokenError::Pin(Length),
            ),
            (
                line_with(ID_TEXT, ok_secret, &ok_pin.replacen('_', "/", 1)),
                TokenError::Pin(Alphabet),
            ),
            (
                line_with(ID_TEXT, ok_secret, &format!("{}9", &ok_pin[..42])),
                TokenError::Pin(NonCanonical),
            ),
            (format!("{LINE}\n"), TokenError::Pin(Length)),
            (format!(" {LINE}"), TokenError::Version),
        ];
        for (text, want) in cases {
            assert_eq!(parse(&text).err(), Some(want), "{text:?}");
        }
    }

    /// Catches: an error message that echoes the input, which would put a secret in a log
    /// or a terminal. Every refusal of a token that holds a recognisable secret is
    /// searched for pieces of it.
    #[test]
    fn no_error_message_holds_any_of_the_input() {
        let secrets = ["SECRETSECRETSECRETSECRETSECRETSECRETSECRE", "SECRET+SECRET"];
        let bad = vec![
            format!("kbft2.{ID_TEXT}.{}.{PIN_TEXT}", secrets[0]),
            format!("kbft1.{ID_TEXT}.{}.{PIN_TEXT}", secrets[1]),
            format!("kbft1.{ID_TEXT}.{}", secrets[0]),
            format!("kbft1.{ID_TEXT}.{}.{PIN_TEXT}.{}", secrets[0], secrets[0]),
            format!("kbft1.SECRET.{}.{PIN_TEXT}", secrets[0]),
            format!("kbft1.{ID_TEXT}.{PIN_TEXT}.{}", secrets[0]),
        ];
        for text in bad {
            let message = parse(&text).unwrap_err().to_string();
            for piece in ["SECRET", "kbft2", "+"] {
                assert!(!message.contains(piece), "{message:?} echoes {piece:?}");
            }
        }
        // The messages that name a part.
        assert_eq!(
            parse("a.b.c").unwrap_err().to_string(),
            "a token has four dot-separated parts, this one has 3"
        );
        assert_eq!(
            parse(&LINE.replacen("kbft1", "x", 1))
                .unwrap_err()
                .to_string(),
            "the token format version is not kbft1"
        );
        assert_eq!(
            parse(&line_with("A", SECRET_TEXT, PIN_TEXT))
                .unwrap_err()
                .to_string(),
            "the token id has the wrong length"
        );
        assert_eq!(
            parse(&line_with(ID_TEXT, "A", PIN_TEXT))
                .unwrap_err()
                .to_string(),
            "the token secret has the wrong length"
        );
        assert_eq!(
            parse(&line_with(
                ID_TEXT,
                SECRET_TEXT,
                &PIN_TEXT.replacen('_', "/", 1)
            ))
            .unwrap_err()
            .to_string(),
            "the token CA pin has a character outside its alphabet"
        );
        assert_eq!(
            PartFault::NonCanonical.to_string(),
            "is not the canonical spelling of its value"
        );
    }

    /// Catches: an error that reads as nothing (an empty or copied message), for the
    /// messages the tests above do not already spell out.
    #[test]
    fn every_error_says_what_is_wrong() {
        let messages = [
            (PartFault::Length.to_string(), "has the wrong length"),
            (
                PartFault::Alphabet.to_string(),
                "has a character outside its alphabet",
            ),
            (NodeIdError::Empty.to_string(), "a node id is empty"),
            (
                NodeIdError::Character.to_string(),
                "a node id has a character outside a-z, 0-9 and -",
            ),
            (
                NodeIdError::Edge.to_string(),
                "a node id starts or ends with -",
            ),
            (
                RoleError::Frontend.to_string(),
                "the frontend role cannot be granted by a token",
            ),
            (
                RoleError::Unknown.to_string(),
                "not a role a token can grant",
            ),
            (RoleSetError::Empty.to_string(), "no roles"),
            (
                RoleSetError::Role(RoleError::Unknown).to_string(),
                "not a role a token can grant",
            ),
            (
                TokenError::Pin(PartFault::Length).to_string(),
                "the token CA pin has the wrong length",
            ),
        ];
        for (got, want) in messages {
            assert_eq!(got, want);
        }
    }

    /// Catches: a `Debug` (a `{:?}` in a log or a test failure) that prints the secret.
    #[test]
    fn debug_never_prints_the_secret() {
        let token = parse(LINE).unwrap();
        assert_eq!(format!("{token:?}"), "Token(..)");
        assert_eq!(format!("{:?}", token.secret()), "TokenSecret(..)");
        assert_eq!(format!("{:?}", token.pin()), format!("CaPin({PIN_TEXT})"));
        assert_eq!(format!("{:?}", token.id()), format!("TokenId({ID_TEXT:?})"));
    }

    /// Catches: a node id the certificate's DNS name, the member id or the `Hello` would
    /// disagree about; each refusal is the first rule it breaks.
    #[test]
    fn node_ids_are_dns_labels() {
        let longest = "a".repeat(63);
        let hyphens = format!("a{}b", "-".repeat(61));
        for ok in ["a", "node-02", "0", "a-b-c", "m1", &longest, &hyphens] {
            let id: NodeId = ok.parse().unwrap();
            assert_eq!(id.as_str(), ok);
            assert_eq!(id.to_string(), ok);
        }
        let too_long = "a".repeat(64);
        let cases = [
            ("", NodeIdError::Empty),
            (too_long.as_str(), NodeIdError::TooLong),
            ("Node", NodeIdError::Character),
            ("node_02", NodeIdError::Character),
            ("node.02", NodeIdError::Character),
            ("nöde", NodeIdError::Character),
            ("node 02", NodeIdError::Character),
            ("-node", NodeIdError::Edge),
            ("node-", NodeIdError::Edge),
            ("-", NodeIdError::Edge),
        ];
        for (text, want) in cases {
            assert_eq!(text.parse::<NodeId>().err(), Some(want), "{text:?}");
        }
        assert_eq!(
            NodeIdError::TooLong.to_string(),
            "a node id is longer than 63 characters"
        );
    }

    /// Catches: `frontend` grantable, a role spelt loosely, and a changed name (it is the
    /// `RoleCsr.role` spelling and the tail of the role URI).
    #[test]
    fn roles_have_their_wire_names() {
        assert_eq!(Role::ALL.map(Role::name), ["storage", "daemon"]);
        assert_eq!("storage".parse(), Ok(Role::Storage));
        assert_eq!("daemon".parse(), Ok(Role::Daemon));
        assert_eq!("frontend".parse::<Role>(), Err(RoleError::Frontend));
        for bad in ["", "Storage", "client", "storage ", "daemon\n"] {
            assert_eq!(bad.parse::<Role>(), Err(RoleError::Unknown), "{bad:?}");
        }
        assert_eq!(Role::Daemon.to_string(), "daemon");
    }

    /// Catches: a role set that is empty, repeats a role, depends on the order it was
    /// built in, or lets `frontend` in.
    #[test]
    fn role_sets_are_non_empty_and_ordered() {
        let both = RoleSet::new([Role::Daemon, Role::Storage]).unwrap();
        assert_eq!(both, RoleSet::new([Role::Storage, Role::Daemon]).unwrap());
        assert_eq!(
            both.iter().collect::<Vec<_>>(),
            [Role::Storage, Role::Daemon]
        );
        assert!(both.contains(Role::Storage));
        assert!(both.contains(Role::Daemon));

        let daemon = RoleSet::from_names(["daemon"]).unwrap();
        assert_eq!(daemon.iter().collect::<Vec<_>>(), [Role::Daemon]);
        assert!(daemon.contains(Role::Daemon));
        assert!(!daemon.contains(Role::Storage));
        assert!(
            !RoleSet::new([Role::Storage])
                .unwrap()
                .contains(Role::Daemon)
        );
        assert_ne!(daemon, both);

        assert_eq!(RoleSet::new([]), Err(RoleSetError::Empty));
        assert_eq!(RoleSet::from_names([]), Err(RoleSetError::Empty));
        assert_eq!(
            RoleSet::new([Role::Storage, Role::Storage]),
            Err(RoleSetError::Duplicate(Role::Storage))
        );
        assert_eq!(
            RoleSet::from_names(["daemon", "storage", "daemon"]),
            Err(RoleSetError::Duplicate(Role::Daemon))
        );
        assert_eq!(
            RoleSet::from_names(["storage", "frontend"]),
            Err(RoleSetError::Role(RoleError::Frontend))
        );
        assert_eq!(
            RoleSet::from_names(["client"]),
            Err(RoleSetError::Role(RoleError::Unknown))
        );
        assert_eq!(
            RoleSetError::Duplicate(Role::Daemon).to_string(),
            "role daemon is named twice"
        );
    }

    /// Catches: a reason added, renamed or moved to another status code without this
    /// table (written from design sections 5.1 and 5.10, not from the code) changing: the
    /// metric labels and the codes a node retries on are a contract.
    #[test]
    fn the_refusal_table_is_golden() {
        use RefusalCode::{
            AlreadyExists as Exists, InvalidArgument as Invalid, PermissionDenied as Denied,
            ResourceExhausted as Exhausted, Unauthenticated as Unauth, Unavailable as Down,
        };
        let table: [(&str, RefusalCode, &str); 22] = [
            ("token_refused", Unauth, "enrollment refused"),
            ("token_expired", Unauth, "token expired"),
            ("token_used_up", Unauth, "token used up"),
            ("token_revoked", Unauth, "token revoked"),
            ("malformed_request", Invalid, "malformed request"),
            ("bad_csr", Invalid, "bad certificate request"),
            ("bad_roles", Invalid, "bad roles"),
            ("role_not_granted", Denied, "role not granted by the token"),
            (
                "node_id_not_allowed",
                Denied,
                "node id not allowed by the token",
            ),
            ("node_id_in_use", Exists, "node id already enrolled"),
            (
                "advertise_not_allowed",
                Denied,
                "advertise host not allowed by the token",
            ),
            (
                "advertise_invalid",
                Invalid,
                "advertise host not acceptable",
            ),
            (
                "advertise_in_use",
                Exists,
                "advertise host belongs to another node",
            ),
            ("unknown_node", Denied, "node not enrolled"),
            ("node_revoked", Denied, "node revoked"),
            ("wrong_key", Denied, "request not signed by the node's key"),
            (
                "renewal_expired",
                Denied,
                "certificate expired too long ago; enroll again",
            ),
            (
                "stale_issued_at",
                Invalid,
                "issued_at too far from the frontend's clock",
            ),
            ("renew_too_soon", Exhausted, "renewal too soon"),
            ("rate_limited", Exhausted, "rate limited"),
            ("store_unavailable", Down, "state store unavailable"),
            ("not_signer", Down, "not the active signer"),
        ];
        assert_eq!(RefusalReason::ALL.len(), table.len());
        for (reason, (name, code, message)) in RefusalReason::ALL.into_iter().zip(table) {
            assert_eq!(reason.name(), name);
            assert_eq!(reason.code(), code, "{name}");
            assert_eq!(reason.message(), message, "{name}");
            assert_eq!(reason.to_string(), name);
            assert_eq!(RefusalReason::from_name(name), Some(reason));
        }
        assert_eq!(RefusalReason::from_name("Token_Refused"), None);
        assert_eq!(RefusalReason::from_name(""), None);
    }

    /// Catches: a reason listed twice or not at all in `ALL` (a metric label that cannot
    /// be read back, or a reason a caller cannot enumerate), and a label that is not a
    /// plain metric word.
    #[test]
    fn reasons_are_distinct_and_plain() {
        let mut names: Vec<&str> = RefusalReason::ALL.iter().map(|r| r.name()).collect();
        let mut messages: Vec<&str> = RefusalReason::ALL.iter().map(|r| r.message()).collect();
        for name in &names {
            assert!(
                name.bytes().all(|c| c.is_ascii_lowercase() || c == b'_'),
                "{name}"
            );
        }
        let mut sorted = RefusalReason::ALL;
        sorted.sort();
        assert_eq!(sorted, RefusalReason::ALL, "ALL is in declaration order");
        names.sort_unstable();
        names.dedup();
        messages.sort_unstable();
        messages.dedup();
        assert_eq!(names.len(), RefusalReason::ALL.len());
        assert_eq!(messages.len(), RefusalReason::ALL.len());
    }

    /// Catches: a code number that is not `google.rpc.Code`'s (kbf-proto cross-checks it
    /// against the generated enum), and a retry rule that retries a refusal that cannot
    /// change (a revoked node) or gives up on one that can (a rate limit).
    #[test]
    fn codes_have_their_numbers_and_retry_rule() {
        use RefusalCode::*;
        let codes = [
            (InvalidArgument, 3, false),
            (Unauthenticated, 16, false),
            (PermissionDenied, 7, false),
            (AlreadyExists, 6, false),
            (ResourceExhausted, 8, true),
            (Unavailable, 14, true),
        ];
        for (code, number, retry) in codes {
            assert_eq!(code.number(), number, "{code:?}");
            assert_eq!(code.retryable(), retry, "{code:?}");
        }
    }
}
