//! The network: off unless the action asks for it, where the node can enforce that.

mod support;

use std::sync::Arc;

use kbf_driver_native::network::Isolation;
use support::{MemoryCas, Spec, config, run, runtime, scratch, stdout};

/// Perl that connects to a TEST-NET address (never routed) and prints the error, or
/// `connected`. A sandbox refuses the connect at once with EPERM; without one, the
/// connect times out (the alarm) or fails another way.
const CONNECT: &str = r#"
use Socket;
$SIG{ALRM} = sub { print "timed out\n"; exit 0 };
alarm 3;
socket(my $s, PF_INET, SOCK_STREAM, 0) or do { print "socket: $!\n"; exit 0 };
if (connect($s, sockaddr_in(80, inet_aton("192.0.2.1")))) { print "connected\n" } else { print "$!\n" }
"#;

/// Catches: an action without the network that can still open a connection, one
/// that asked for the network and was refused it, and loopback refused (a build's
/// own test servers live there). Runs where the node has an isolation mechanism
/// (`sandbox-exec` on macOS); elsewhere it checks the node says it has none.
#[tokio::test]
async fn the_network_is_off_unless_asked_for() {
    let dir = scratch("network");
    let config = config(&dir);
    let cas = Arc::new(MemoryCas::default());
    let rt = runtime(config.clone(), &cas);
    if config.isolation == Isolation::None {
        // Every supported Mac has sandbox-exec; a macOS run without it is a failure.
        assert_eq!(std::env::consts::OS, "linux", "no sandbox-exec on this Mac");
        return;
    }
    let denied = "Operation not permitted";
    let off = Spec::argv(&["/usr/bin/perl", "-e", CONNECT]);
    let result = run(&rt, &cas, 1, &off).await.expect("ran");
    assert_eq!(stdout(&cas, &result).trim(), denied);
    let on = off.clone().property("network", "on");
    let result = run(&rt, &cas, 2, &on).await.expect("ran");
    assert_ne!(stdout(&cas, &result).trim(), denied);

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let loopback = Spec::argv(&[
        "/usr/bin/perl",
        "-e",
        &CONNECT
            .replace("192.0.2.1", "127.0.0.1")
            .replace("80,", &format!("{port},")),
    ]);
    let result = run(&rt, &cas, 3, &loopback).await.expect("ran");
    assert_eq!(stdout(&cas, &result).trim(), "connected");
}
