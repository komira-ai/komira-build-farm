//! `READY=1` to a socket the test listens on.

use std::os::unix::net::UnixDatagram;

use super::*;
use crate::handover::tests::Scratch;

/// Catches: a ready message that is not `READY=1` (the manager would wait for the start
/// timeout), one sent to the wrong kind of address (a path taken for an abstract name or
/// the reverse), and an error swallowed where the manager is not there.
#[test]
fn ready_reaches_a_path_and_an_abstract_name() {
    let dir = Scratch::new();
    let path = dir.0.join("notify");
    let listening = UnixDatagram::bind(&path).expect("a datagram socket");
    ready(path.as_os_str()).expect("sent to a path");
    let mut buffer = [0_u8; 32];
    let got = listening.recv(&mut buffer).expect("a message");
    assert_eq!(&buffer[..got], b"READY=1");

    #[cfg(target_os = "linux")]
    {
        use std::os::linux::net::SocketAddrExt as _;
        let name = format!("kbf-notify-test-{}", std::process::id());
        let address =
            std::os::unix::net::SocketAddr::from_abstract_name(name.as_bytes()).expect("a name");
        let listening = UnixDatagram::bind_addr(&address).expect("an abstract socket");
        ready(OsStr::new(&format!("@{name}"))).expect("sent to an abstract name");
        let got = listening.recv(&mut buffer).expect("a message");
        assert_eq!(&buffer[..got], b"READY=1");
    }

    let nowhere = dir.0.join("nobody-listens");
    assert!(ready(nowhere.as_os_str()).is_err());
}
