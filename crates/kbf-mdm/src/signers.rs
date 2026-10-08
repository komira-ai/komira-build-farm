//! Operator signatures (M4.2): the allowed-signers file, and verification of an
//! `ssh-keygen -Y sign` signature (the SSHSIG format) under it.
//!
//! The SSHSIG envelope and the signature itself are checked by the `ssh-key` crate.
//! The gate adds what `ssh-keygen -Y verify` does not promise: only security-key
//! (`sk-`) key types are accepted, and the security key's flags byte must say "user
//! present" (a touch), and "user verified" too when the gate is configured so.
//!
//! The allowed-signers file is the format of ssh-keygen(1)'s ALLOWED SIGNERS section:
//! `principals [options] keytype base64-key [comment]`, with the options
//! `namespaces="..."`, `valid-after="..."` and `valid-before="..."`. Two limits are the
//! gate's own: a key verifies a request only if its line names the gate's namespace
//! (a line without `namespaces=` allows none), and times must be UTC, written with a
//! final `Z`. `cert-authority` lines are refused: the gate trusts keys, not CAs.

use ssh_key::{Algorithm, PublicKey, SshSig};

/// The namespace operators sign erase requests in (`ssh-keygen -Y sign -n ...`).
pub const NAMESPACE: &str = "kbf-mdm-erase";

/// The security-key flags: user present (a touch) and user verified (PIN or biometric),
/// from OpenSSH's PROTOCOL.u2f.
pub const FLAG_USER_PRESENT: u8 = 0x01;
pub const FLAG_USER_VERIFIED: u8 = 0x04;

/// One line of the allowed-signers file.
#[derive(Clone, Debug)]
struct Entry {
    principals: String,
    namespaces: Vec<String>,
    valid_after: Option<i64>,
    valid_before: Option<i64>,
    key: PublicKey,
}

/// The parsed allowed-signers file.
#[derive(Clone, Debug, Default)]
pub struct AllowedSigners {
    entries: Vec<Entry>,
}

/// Why the allowed-signers file could not be read.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("allowed signers line {line}: {reason}")]
pub struct SignersError {
    pub line: usize,
    pub reason: String,
}

/// Who signed a verified request, and the security key's flags and counter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Signer {
    /// The principals field of the matching allowed-signers line.
    pub principals: String,
    /// The key's algorithm name, such as `sk-ssh-ed25519@openssh.com`.
    pub algorithm: String,
    pub flags: u8,
    pub counter: u32,
}

/// Why a signature was refused.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum VerifyError {
    #[error("not an SSH signature")]
    Malformed,
    #[error("signed in namespace {0:?}, not the gate's")]
    Namespace(String),
    #[error("signed with a {0} key; only security keys (sk-) are accepted")]
    SoftwareKey(String),
    #[error("the key is not in the allowed signers for the gate's namespace")]
    UnknownSigner,
    #[error("the key is outside its valid-after/valid-before window")]
    OutsideValidity,
    #[error("the signature does not verify")]
    BadSignature,
    #[error("the security key did not record a touch (user present flag unset)")]
    NoTouch,
    #[error("the security key did not verify the user (user verified flag unset)")]
    NotUserVerified,
}

/// Reads a `valid-after`/`valid-before` time: `YYYYMMDD[HHMM[SS]]Z`.
fn parse_time(text: &str) -> Option<i64> {
    let digits = text.strip_suffix('Z')?;
    if !matches!(digits.len(), 8 | 12 | 14) || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let number = |range: std::ops::Range<usize>| -> u32 {
        digits.get(range).map_or(0, |s| s.parse().unwrap_or(0))
    };
    let month = time::Month::try_from(u8::try_from(number(4..6)).ok()?).ok()?;
    let year = i32::try_from(number(0..4)).ok()?;
    let date =
        time::Date::from_calendar_date(year, month, u8::try_from(number(6..8)).ok()?).ok()?;
    let at = date
        .with_hms(
            u8::try_from(number(8..10)).ok()?,
            u8::try_from(number(10..12)).ok()?,
            u8::try_from(number(12..14)).ok()?,
        )
        .ok()?;
    Some(at.assume_utc().unix_timestamp())
}

/// Splits `text` at its first whitespace outside double quotes.
fn split_token(text: &str) -> (&str, &str) {
    let mut quoted = false;
    for (i, c) in text.char_indices() {
        match c {
            '"' => quoted = !quoted,
            c if c.is_whitespace() && !quoted => return (&text[..i], text[i..].trim_start()),
            _ => {}
        }
    }
    (text, "")
}

/// Splits an options token at commas outside double quotes.
fn split_options(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let (mut start, mut quoted) = (0, false);
    for (i, c) in text.char_indices() {
        match c {
            '"' => quoted = !quoted,
            ',' if !quoted => {
                out.push(&text[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&text[start..]);
    out
}

fn unquote(value: &str) -> Option<&str> {
    value.strip_prefix('"')?.strip_suffix('"')
}

fn looks_like_key_type(token: &str) -> bool {
    ["ssh-", "sk-", "ecdsa-"]
        .iter()
        .any(|prefix| token.starts_with(prefix))
}

fn parse_line(line: &str) -> Result<Entry, String> {
    let (principals, mut rest) = split_token(line);
    let principals = unquote(principals).unwrap_or(principals).to_owned();
    let (mut namespaces, mut valid_after, mut valid_before) = (Vec::new(), None, None);
    if !looks_like_key_type(rest) {
        let (options, key_text) = split_token(rest);
        rest = key_text;
        for option in split_options(options) {
            let (name, value) = option.split_once('=').unwrap_or((option, ""));
            let name = name.to_ascii_lowercase();
            if name == "cert-authority" {
                return Err("cert-authority is not supported".into());
            }
            let value =
                unquote(value).ok_or_else(|| format!("option {name}: expected {name}=\"...\""))?;
            let when = || parse_time(value).ok_or(format!("{name}: expected YYYYMMDD[HHMM[SS]]Z"));
            match name.as_str() {
                "namespaces" => namespaces = value.split(',').map(str::to_owned).collect(),
                "valid-after" => valid_after = Some(when()?),
                "valid-before" => valid_before = Some(when()?),
                other => return Err(format!("unknown option {other:?}")),
            }
        }
    }
    let key = PublicKey::from_openssh(rest).map_err(|e| format!("key: {e}"))?;
    Ok(Entry {
        principals,
        namespaces,
        valid_after,
        valid_before,
        key,
    })
}

impl AllowedSigners {
    /// Parses an allowed-signers file. Blank lines and `#` comments are skipped.
    ///
    /// # Errors
    /// A line is not a valid allowed-signers line, or uses what the gate refuses.
    pub fn parse(text: &str) -> Result<Self, SignersError> {
        let mut entries = Vec::new();
        for (index, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            entries.push(parse_line(line).map_err(|reason| SignersError {
                line: index + 1,
                reason,
            })?);
        }
        Ok(Self { entries })
    }

    /// Verifies `signature` (the armored output of `ssh-keygen -Y sign`) over `message`
    /// at time `now`. With `require_user_verified`, the security key must also have
    /// verified the user.
    ///
    /// # Errors
    /// Any check fails; the error says which.
    pub fn verify(
        &self,
        message: &[u8],
        signature: &str,
        now: i64,
        require_user_verified: bool,
    ) -> Result<Signer, VerifyError> {
        self.verify_in(NAMESPACE, message, signature, now, require_user_verified)
    }

    /// [`Self::verify`] in another namespace: OpenSSH's own test vectors are signed in
    /// theirs.
    fn verify_in(
        &self,
        namespace: &str,
        message: &[u8],
        signature: &str,
        now: i64,
        require_user_verified: bool,
    ) -> Result<Signer, VerifyError> {
        let sig = SshSig::from_pem(signature).map_err(|_| VerifyError::Malformed)?;
        if sig.namespace() != namespace {
            return Err(VerifyError::Namespace(sig.namespace().to_owned()));
        }
        let algorithm = sig.public_key().algorithm();
        if !matches!(
            algorithm,
            Algorithm::SkEd25519 | Algorithm::SkEcdsaSha2NistP256
        ) {
            return Err(VerifyError::SoftwareKey(algorithm.as_str().to_owned()));
        }
        let mut known = self
            .entries
            .iter()
            .filter(|e| e.key.key_data() == sig.public_key())
            .filter(|e| e.namespaces.iter().any(|n| n == namespace))
            .peekable();
        known.peek().ok_or(VerifyError::UnknownSigner)?;
        let entry = known
            .find(|e| {
                e.valid_after.is_none_or(|t| now >= t) && e.valid_before.is_none_or(|t| now < t)
            })
            .ok_or(VerifyError::OutsideValidity)?;
        entry
            .key
            .verify(namespace, message, &sig)
            .map_err(|_| VerifyError::BadSignature)?;
        // An sk- signature ends with the flags byte and a big-endian 32-bit counter;
        // `ssh-key` checked that they were signed.
        let bytes = sig.signature_bytes();
        let trailer = &bytes[bytes.len() - 5..];
        let flags = trailer[0];
        let counter = u32::from_be_bytes([trailer[1], trailer[2], trailer[3], trailer[4]]);
        if flags & FLAG_USER_PRESENT == 0 {
            return Err(VerifyError::NoTouch);
        }
        if require_user_verified && flags & FLAG_USER_VERIFIED == 0 {
            return Err(VerifyError::NotUserVerified);
        }
        Ok(Signer {
            principals: entry.principals.clone(),
            algorithm: algorithm.as_str().to_owned(),
            flags,
            counter,
        })
    }
}

#[cfg(test)]
#[path = "signers_tests.rs"]
mod tests;
