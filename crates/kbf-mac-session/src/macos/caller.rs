//! The caller check on macOS (S4.3): the connecting process's code signature, found
//! by its audit token, must satisfy the pinned `kbf-daemon` requirement.
//!
//! The audit token (`LOCAL_PEERTOKEN`) is the kernel's record of the process that
//! connected, including its pid version, which changes on `exec`. So a process that
//! connects and then executes the genuine daemon is refused: its token no longer names
//! a running process. A check by pid would accept it.

use std::os::fd::AsRawFd as _;
use std::os::unix::net::UnixStream;
use std::str::FromStr as _;

use core_foundation::base::TCFType as _;
use core_foundation::data::CFData;
use security_framework::os::macos::code_signing::{
    Flags, GuestAttributes, SecCode, SecRequirement,
};

use crate::server::CallerCheck;

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
    fn check(&self, socket: &UnixStream) -> Result<(), String> {
        let token = audit_token(socket)?;
        // Compiled per check: the Security objects are not shared between threads.
        let requirement = SecRequirement::from_str(&self.requirement)
            .map_err(|why| format!("the daemon requirement: {why}"))?;
        let data = CFData::from_buffer(&token);
        let mut attributes = GuestAttributes::new();
        attributes.set_audit_token(data.as_concrete_TypeRef());
        let code = SecCode::copy_guest_with_attribues(None, &attributes, Flags::NONE)
            .map_err(|why| format!("no running code for the caller's audit token: {why}"))?;
        code.check_validity(Flags::NONE, &requirement)
            .map_err(|why| format!("the caller is not the installed kbf-daemon: {why}"))
    }
}

/// The audit token of the process that connected to `socket`.
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
