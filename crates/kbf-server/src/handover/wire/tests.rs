//! The handover's lines and the sockets that travel with them.

use std::net::TcpListener;
use std::os::fd::{AsFd as _, OwnedFd};
use std::os::unix::ffi::OsStringExt as _;

use super::*;

fn addr(text: &str) -> SocketAddr {
    text.parse().expect("an address")
}

fn hello(api: Option<&str>, state: PathBuf) -> Message {
    Message::Hello {
        version: PROTOCOL,
        reapi: addr("127.0.0.1:1000"),
        worker: addr("[::1]:2000"),
        api: api.map(addr),
        state,
    }
}

/// Catches: a message whose line does not parse back to it (a successor and a holder
/// that disagree on what was said), a state path whose spaces, line breaks or non-UTF-8
/// bytes break the framing or are lost (the path is hex on the wire), and a reason that
/// carries a line break into a second, bogus message.
#[test]
fn every_message_is_one_line_that_parses_back() {
    let odd = PathBuf::from(OsString::from_vec(b"/var/lib/a b\n\xff/state.db".to_vec()));
    let messages = [
        hello(None, PathBuf::from("/var/lib/kbf/state.db")),
        hello(Some("127.0.0.1:3000"), odd),
        Message::Offer {
            generation: 7,
            api: true,
        },
        Message::Offer {
            generation: 0,
            api: false,
        },
        Message::Refused("no".to_owned()),
        Message::Failed("the store said no".to_owned()),
        Message::Ready,
        Message::Released,
    ];
    for message in messages {
        let line = message.line();
        assert!(!line.contains(['\n', '\r']), "{line:?}");
        assert_eq!(
            Message::parse(&format!("{line}\n")),
            Some(message),
            "{line}"
        );
    }
    let broken = Message::Refused("two\nlines\r".to_owned());
    assert_eq!(broken.line(), "refused two lines ");
    assert_eq!(
        Message::parse("failed\n"),
        Some(Message::Failed(String::new())),
        "a reason may be empty"
    );
}

/// Catches: a line that is not a message taken for one (a garbled hello would name the
/// wrong address, or none), for each way a hello or an offer can be wrong: a missing
/// field, a field that does not parse, a state path that is not hex.
#[test]
fn a_line_that_is_not_a_message_is_none() {
    for line in [
        "",
        "launch",
        "hello",
        "hello x 127.0.0.1:1 127.0.0.1:2 - 00",
        "hello 1 127.0.0.1 127.0.0.1:2 - 00",
        "hello 1 127.0.0.1:1 nowhere - 00",
        "hello 1 127.0.0.1:1 127.0.0.1:2 nowhere 00",
        "hello 1 127.0.0.1:1 127.0.0.1:2 -",
        "hello 1 127.0.0.1:1 127.0.0.1:2 - zz",
        "offer",
        "offer 7",
        "offer seven 1",
    ] {
        assert_eq!(Message::parse(line), None, "{line:?}");
    }
}

async fn pair() -> (Link, Link) {
    let (a, b) = UnixStream::pair().expect("a socket pair");
    (Link::new(a), Link::new(b))
}

/// Catches: a link that loses, reorders or merges lines, and the three ways a read ends
/// without a message: the other end closed (not a hang), a line that is not a message
/// (named in the error), and an I/O error.
#[tokio::test]
async fn a_link_carries_lines_and_says_how_a_read_ended() {
    let (mut a, mut b) = pair().await;
    a.say(&Message::Ready).await.expect("say");
    a.say(&Message::Released).await.expect("say");
    assert_eq!(b.hear().await.expect("hear"), Message::Ready);
    assert_eq!(b.hear().await.expect("hear"), Message::Released);

    a.stream
        .get_mut()
        .write_all(b"not a message\n")
        .await
        .expect("write");
    let malformed = b.hear().await.expect_err("malformed");
    assert!(
        matches!(&malformed, WireError::Malformed(line) if line == "not a message"),
        "{malformed}"
    );

    drop(a);
    let closed = b.hear().await.expect_err("closed");
    assert!(matches!(closed, WireError::Closed), "{closed}");
    assert_eq!(
        closed.to_string(),
        "the other process closed the control connection"
    );

    let io = WireError::from(io::Error::other("broken"));
    assert_eq!(io.to_string(), "control socket: broken");
}

fn listener() -> (TcpListener, SocketAddr) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let local = listener.local_addr().expect("local address");
    (listener, local)
}

/// What `listener`'s socket is, as a received handle on it says.
fn local(fd: OwnedFd) -> SocketAddr {
    TcpListener::from(fd)
        .local_addr()
        .expect("a listening socket")
}

/// Catches: sockets that do not arrive with the line they were sent on (the offer would
/// name sockets it does not carry), arrive as other sockets, or arrive in another order
/// (the successor would serve REAPI on the worker socket); a line sent without sockets
/// that comes with some; and a read that ends without a message.
#[tokio::test]
async fn sockets_arrive_with_their_line_in_order() {
    let (mut holder, mut successor) = pair().await;
    let ((one, one_addr), (two, two_addr), (three, three_addr)) =
        (listener(), listener(), listener());
    let offer = Message::Offer {
        generation: 3,
        api: true,
    };
    holder
        .say_with(&offer, &[one.as_fd(), two.as_fd(), three.as_fd()])
        .await
        .expect("send");
    let (message, sockets) = successor.hear_with().await.expect("receive");
    assert_eq!(message, offer);
    let got: Vec<SocketAddr> = sockets.into_iter().map(local).collect();
    assert_eq!(got, [one_addr, two_addr, three_addr]);

    holder.say(&Message::Released).await.expect("say");
    let (message, sockets) = successor.hear_with().await.expect("receive");
    assert_eq!(message, Message::Released);
    assert!(sockets.is_empty());

    holder
        .stream
        .get_mut()
        .write_all(b"sideways\n")
        .await
        .expect("write");
    let malformed = successor.hear_with().await.expect_err("malformed");
    assert!(matches!(malformed, WireError::Malformed(line) if line == "sideways"));

    drop(holder);
    let closed = successor.hear_with().await.expect_err("closed");
    assert!(matches!(closed, WireError::Closed), "{closed}");
}

/// Catches: a successor that fails on, or takes for sockets, a control message of
/// another kind that rides on the connection (a peer's credentials, which the kernel
/// attaches when a socket asks for them): only the sockets are taken.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn control_messages_that_are_not_sockets_are_not_taken_for_them() {
    use std::os::unix::net::UnixStream as StdUnixStream;

    use rustix::net::{
        SendAncillaryBuffer, SendAncillaryMessage, SendFlags, UCred, sendmsg, sockopt,
    };

    let (a, b) = StdUnixStream::pair().expect("a socket pair");
    sockopt::set_socket_passcred(&b, true).expect("ask for credentials");
    a.set_nonblocking(true).expect("nonblocking");
    b.set_nonblocking(true).expect("nonblocking");
    let (one, one_addr) = listener();
    let creds = UCred {
        pid: rustix::process::getpid(),
        uid: rustix::process::getuid(),
        gid: rustix::process::getgid(),
    };
    let line = b"ready\n";
    let mut space =
        [std::mem::MaybeUninit::uninit(); rustix::cmsg_space!(ScmCredentials(1), ScmRights(1))];
    let mut control = SendAncillaryBuffer::new(&mut space);
    let fds = [one.as_fd()];
    assert!(control.push(SendAncillaryMessage::ScmCredentials(creds)));
    assert!(control.push(SendAncillaryMessage::ScmRights(&fds)));
    sendmsg(
        &a,
        &[std::io::IoSlice::new(line)],
        &mut control,
        SendFlags::empty(),
    )
    .expect("send");

    let mut successor = Link::new(UnixStream::from_std(b).expect("tokio"));
    let (message, sockets) = successor.hear_with().await.expect("receive");
    assert_eq!(message, Message::Ready);
    let got: Vec<SocketAddr> = sockets.into_iter().map(local).collect();
    assert_eq!(got, [one_addr]);
}
