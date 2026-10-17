//! Telling systemd the service is ready (`sd_notify(3)`, `READY=1` on `$NOTIFY_SOCKET`).

use std::ffi::OsStr;
use std::io;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::net::UnixDatagram;

/// The message.
const READY: &[u8] = b"READY=1";

/// Sends `READY=1` to `target`, a filesystem path or, with a leading `@`, a name in the
/// abstract socket namespace (Linux).
///
/// # Errors
/// The socket cannot be made or the message cannot be sent.
pub fn ready(target: &OsStr) -> io::Result<()> {
    let socket = UnixDatagram::unbound()?;
    match target.as_bytes().strip_prefix(b"@") {
        Some(name) => send_to_abstract(&socket, name),
        None => socket.send_to(READY, target).map(drop),
    }
}

#[cfg(target_os = "linux")]
fn send_to_abstract(socket: &UnixDatagram, name: &[u8]) -> io::Result<()> {
    use std::os::linux::net::SocketAddrExt as _;
    let address = std::os::unix::net::SocketAddr::from_abstract_name(name)?;
    socket.send_to_addr(READY, &address).map(drop)
}

#[cfg(not(target_os = "linux"))]
fn send_to_abstract(_socket: &UnixDatagram, _name: &[u8]) -> io::Result<()> {
    Err(io::Error::from(io::ErrorKind::Unsupported))
}

#[cfg(test)]
mod tests;
