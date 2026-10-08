//! The caller check on macOS (S4.3): the connecting process's code signature, found
//! by its audit token, must satisfy the pinned `kbf-daemon` requirement.
//!
//! The audit token (`LOCAL_PEERTOKEN`) names a process by pid and pid version, so a
//! later process that reuses the pid is not taken for it. The server identifies the
//! caller as soon as it accepts the connection, checks that token's code then, and
//! requires the same token once the request has arrived (see `crate::server`): a
//! process that connects and only then executes the genuine daemon was checked as
//! itself, before the exec, and is refused. The CI job (`tools/ci/mac-session-tests.sh`)
//! plants that case and also measures what the token reports across an `exec`.

use std::os::fd::AsRawFd as _;
use std::os::unix::net::UnixStream;
use std::str::FromStr as _;

use core_foundation::base::TCFType as _;
use core_foundation::data::CFData;
use security_framework::os::macos::code_signing::{
    Flags, GuestAttributes, SecCode, SecRequirement,
};

use crate::server::{Caller, CallerCheck};

/// The size of an `audit_token_t`: eight 32-bit words.
const AUDIT_TOKEN_BYTES: usize = 32;

/// Checks callers against one code requirement.
#[derive(Debug)]
pub struct CodeSignatureCheck {
    requirement: String,
}

impl CodeSignatureCheck {
    /// A check against `requirement` (code requirement language, for example
    /// `cdhash H"<40 hex digits>"`).
    ///
    /// # Errors
    /// The requirement does not compile.
    pub fn new(requirement: &str) -> Result<Self, String> {
        SecRequirement::from_str(requirement)
            .map_err(|why| format!("the daemon requirement does not compile: {why}"))?;
        Ok(Self {
            requirement: requirement.to_owned(),
        })
    }
}

impl CallerCheck for CodeSignatureCheck {
    fn identify(&self, socket: &UnixStream) -> Result<Caller, String> {
        audit_token(socket).map(|token| caller(&token))
    }

    fn check(&self, caller: &Caller) -> Result<(), String> {
        // Compiled per check: the Security objects are not shared between threads.
        let requirement = SecRequirement::from_str(&self.requirement)
            .map_err(|why| format!("the daemon requirement: {why}"))?;
        let data = CFData::from_buffer(&caller.token);
        let mut attributes = GuestAttributes::new();
        attributes.set_audit_token(data.as_concrete_TypeRef());
        let code = SecCode::copy_guest_with_attribues(None, &attributes, Flags::NONE)
            .map_err(|why| format!("no running code for {caller}: {why}"))?;
        code.check_validity(Flags::NONE, &requirement)
            .map_err(|why| format!("{caller} is not the installed kbf-daemon: {why}"))
    }
}

/// The caller an audit token names: `audit_token_to_pid` is word 5 and
/// `audit_token_to_pidversion` word 7 (`bsm/libbsm.h`).
fn caller(token: &[u8; AUDIT_TOKEN_BYTES]) -> Caller {
    let word = |index: usize| {
        let mut bytes = [0u8; 4];
        bytes.copy_from_slice(&token[4 * index..4 * index + 4]);
        u32::from_ne_bytes(bytes)
    };
    Caller {
        token: token.to_vec(),
        pid: i64::from(word(5).cast_signed()),
        version: i64::from(word(7).cast_signed()),
    }
}

/// The audit token of the process at the other end of `socket`.
fn audit_token(socket: &UnixStream) -> Result<[u8; AUDIT_TOKEN_BYTES], String> {
    let mut token = [0u8; AUDIT_TOKEN_BYTES];
    let mut len = AUDIT_TOKEN_BYTES as libc::socklen_t;
    // SAFETY: `token` is a writable buffer of `len` bytes and `len` a writable
    // socklen_t; the call writes at most `len` bytes and stores how many.
    let got = unsafe {
        libc::getsockopt(
            socket.as_raw_fd(),
            libc::SOL_LOCAL,
            libc::LOCAL_PEERTOKEN,
            token.as_mut_ptr().cast(),
            &raw mut len,
        )
    };
    if got != 0 {
        return Err(format!(
            "LOCAL_PEERTOKEN: {}",
            std::io::Error::last_os_error()
        ));
    }
    if len as usize != AUDIT_TOKEN_BYTES {
        return Err(format!("LOCAL_PEERTOKEN gave {len} bytes"));
    }
    Ok(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches: the pid or pid version read from the wrong word of the token, which
    /// would make the server's "same process" comparison and its logs meaningless.
    /// A connection to this process's own socket pair names this process.
    #[test]
    fn a_token_names_the_pid_at_the_other_end() {
        let (a, _b) = UnixStream::pair().unwrap();
        let me = caller(&audit_token(&a).unwrap());
        assert_eq!(me.pid, i64::from(std::process::id()));
        let check = CodeSignatureCheck::new("cdhash H\"0000000000000000000000000000000000000000\"")
            .unwrap();
        assert_eq!(check.identify(&a).unwrap(), me);
        // Refused, whether as code that fails the requirement or as code not found.
        let error = check.check(&me).unwrap_err();
        assert!(error.contains(&me.to_string()), "{error}");
        assert!(CodeSignatureCheck::new("not a requirement").is_err());
    }
}
