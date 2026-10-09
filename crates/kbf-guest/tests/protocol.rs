//! The handshake and the one-command rule, over a Unix socket pair.

mod common;

use std::io::Write as _;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use common::{Served, Shares, TOKEN, sh};
use kbf_guest::client::{Client, ClientError};
use kbf_guest::wire::{
    End, GuestMsg, HostMsg, MAX_FRAME, Refusal, RunRequest, TOKEN_LEN, VERSION, read_frame,
    write_frame,
};

fn send(stream: &mut UnixStream, msg: &HostMsg) {
    write_frame(stream, &msg.encode()).expect("sent");
}

fn recv(stream: &mut UnixStream) -> GuestMsg {
    GuestMsg::decode(&read_frame(stream).expect("frame")).expect("decodes")
}

fn refusal(result: Result<impl std::fmt::Debug, ClientError>) -> Refusal {
    match result {
        Err(ClientError::Refused { reason, .. }) => reason,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

/// Catches a skipped or partial token check (a prefix compare, a length-only check):
/// a token that differs in its last byte is refused, the connection is closed, and the
/// agent still serves the right token next.
#[test]
fn a_wrong_token_is_refused_and_the_right_one_still_works() {
    let shares = Shares::new("token");
    let served = Served::start(shares.config());
    let mut wrong = TOKEN;
    wrong[TOKEN_LEN - 1] ^= 1;
    let mut host = served.connect_raw();
    let err = Client::connect(host.try_clone().expect("clone"), wrong).expect_err("refused");
    assert_eq!(refusal(Err::<(), _>(err)), Refusal::Token);
    // Closed after the refusal: a Run on this connection gets nothing back.
    let _ = write_frame(&mut host, &HostMsg::Run(sh("true", &[])).encode());
    assert!(read_frame(&mut host).is_err(), "the connection stayed open");
    assert!(
        !shares.outputs.join("stdout").exists(),
        "a refused host ran a command"
    );

    let mut client = served.connect();
    assert_eq!(client.session(), "TestSession");
    client.run(&sh("exit 0", &[])).expect("started");
    assert_eq!(client.wait().expect("exited").end, End::Exited(0));
}

/// Catches a version that is not checked, and a host message other than Hello taken
/// as a handshake.
#[test]
fn another_version_or_no_hello_is_refused() {
    let shares = Shares::new("version");
    let served = Served::start(shares.config());
    let mut host = served.connect_raw();
    send(
        &mut host,
        &HostMsg::Hello {
            version: VERSION + 1,
            token: TOKEN,
        },
    );
    assert!(matches!(
        recv(&mut host),
        GuestMsg::Refused {
            reason: Refusal::Version,
            ..
        }
    ));

    let mut host = served.connect_raw();
    send(&mut host, &HostMsg::Run(sh("true", &[])));
    assert!(matches!(
        recv(&mut host),
        GuestMsg::Refused {
            reason: Refusal::NoHello,
            ..
        }
    ));
    assert!(!shares.outputs.join("stdout").exists());
}

/// Catches a connection that holds the agent forever without sending Hello (any
/// guest process that can connect could otherwise lock the host out).
#[test]
fn a_silent_connection_is_dropped_after_the_hello_timeout() {
    let shares = Shares::new("hello-timeout");
    let mut config = shares.config();
    config.hello_timeout = Duration::from_millis(200);
    let served = Served::start(config);
    let mut silent = served.connect_raw();
    assert!(matches!(
        recv(&mut silent),
        GuestMsg::Refused {
            reason: Refusal::NoHello,
            ..
        }
    ));
    let mut client = served.connect();
    client.run(&sh("exit 0", &[])).expect("started");
    assert_eq!(client.wait().expect("exited").end, End::Exited(0));
}

/// Catches a hello timeout that bounds each read rather than the whole Hello: a
/// connection that announces a large frame and then sends a byte every half timeout
/// would otherwise hold the agent for as long as it keeps dripping.
#[test]
fn a_dripping_connection_is_dropped_after_the_hello_timeout() {
    let shares = Shares::new("hello-drip");
    let mut config = shares.config();
    config.hello_timeout = Duration::from_millis(200);
    let served = Served::start(config);
    let mut host = served.connect_raw();
    // A reply that never comes fails the read below instead of hanging the test.
    host.set_read_timeout(Some(Duration::from_secs(3)))
        .expect("read timeout");
    let mut drip = host.try_clone().expect("clone");
    let dripping = std::thread::spawn(move || {
        drip.write_all(&MAX_FRAME.to_be_bytes()).expect("length");
        let give_up = Instant::now() + Duration::from_secs(5);
        // Ends once the agent shuts the connection.
        while Instant::now() < give_up && drip.write_all(&[0]).is_ok() {
            std::thread::sleep(Duration::from_millis(100));
        }
    });
    let start = Instant::now();
    let reply = read_frame(&mut host).expect("the agent replied before the test gave up");
    assert!(matches!(
        GuestMsg::decode(&reply).expect("decodes"),
        GuestMsg::Refused {
            reason: Refusal::NoHello,
            ..
        }
    ));
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "refused only after {:?}",
        start.elapsed()
    );
    dripping.join().expect("drip thread");
    let mut client = served.connect();
    client.run(&sh("exit 0", &[])).expect("started");
    assert_eq!(client.wait().expect("exited").end, End::Exited(0));
}

/// Catches the Hello deadline left on the socket after `Ready`: the reader would then
/// time out once a run stays silent for longer than the hello timeout, take that as a
/// host that is gone, and kill the run (every build over 10 s in production).
#[test]
fn a_run_longer_than_the_hello_timeout_still_completes() {
    let shares = Shares::new("hello-outlast");
    let mut config = shares.config();
    config.hello_timeout = Duration::from_millis(200);
    let served = Served::start(config);
    let mut client = served.connect();
    client.run(&sh("sleep 1; exit 0", &[])).expect("started");
    let exit = client.wait().expect("the run was reported");
    assert_eq!(exit.end, End::Exited(0));
}

/// Catches a second command on one boot: during the run, after it on the same
/// connection, and on a new connection.
#[test]
fn a_second_run_is_refused() {
    let shares = Shares::new("second-run");
    let served = Served::start(shares.config());
    let mut client = served.connect();
    client.run(&sh("sleep 0.3", &[])).expect("started");
    client.send(&HostMsg::Run(sh("true", &[]))).expect("sent");
    assert!(matches!(
        client.recv().expect("reply"),
        GuestMsg::Refused {
            reason: Refusal::AlreadyRan,
            ..
        }
    ));
    // A second Hello while it runs is refused too, and does not end the run.
    client
        .send(&HostMsg::Hello {
            version: VERSION,
            token: TOKEN,
        })
        .expect("sent");
    assert!(matches!(
        client.recv().expect("reply"),
        GuestMsg::Refused {
            reason: Refusal::BadRequest,
            ..
        }
    ));
    assert_eq!(client.wait().expect("exited").end, End::Exited(0));
    assert_eq!(refusal(client.run(&sh("true", &[]))), Refusal::AlreadyRan);
    drop(client);

    let mut again = served.connect();
    assert_eq!(refusal(again.run(&sh("true", &[]))), Refusal::AlreadyRan);
    // Kill with nothing running is ignored, and a second Hello is refused.
    again.send(&HostMsg::Kill).expect("sent");
    again
        .send(&HostMsg::Hello {
            version: VERSION,
            token: TOKEN,
        })
        .expect("sent");
    assert!(matches!(
        again.recv().expect("reply"),
        GuestMsg::Refused {
            reason: Refusal::BadRequest,
            ..
        }
    ));
}

/// Catches a request that escapes a share or that the OS would misread, and a bad
/// request that uses up the boot's one command.
#[test]
fn bad_requests_are_refused_without_using_the_run() {
    let shares = Shares::new("bad-requests");
    std::fs::write(shares.inputs.join("file"), b"").expect("file");
    std::os::unix::fs::symlink("/", shares.inputs.join("out-link")).expect("symlink");
    std::fs::create_dir(shares.inputs.join("in")).expect("dir");
    std::os::unix::fs::symlink("in", shares.inputs.join("in-link")).expect("symlink");
    let served = Served::start(shares.config());
    let mut client = served.connect();
    let base = sh("true", &[]);
    let with = |f: &dyn Fn(&mut RunRequest)| {
        let mut r = base.clone();
        f(&mut r);
        r
    };
    let bad = [
        with(&|r| r.argv.clear()),
        with(&|r| r.argv[2] = "a\0b".into()),
        with(&|r| r.env.push(("A=B".into(), "c".into()))),
        with(&|r| r.env.push((String::new(), "c".into()))),
        with(&|r| r.cwd = "/".into()),
        with(&|r| r.cwd = "..".into()),
        with(&|r| r.cwd = "in/../..".into()),
        with(&|r| r.cwd = "in/./x".into()),
        with(&|r| r.cwd = "out-link".into()),
        with(&|r| r.cwd = "file".into()),
        with(&|r| r.cwd = "absent".into()),
        with(&|r| r.outputs = vec!["stdout".into()]),
        with(&|r| r.outputs = vec!["stderr/x".into()]),
        with(&|r| r.outputs = vec![String::new()]),
        with(&|r| r.outputs = vec!["/etc/passwd".into()]),
        with(&|r| r.outputs = vec!["a/../b".into()]),
        with(&|r| r.outputs = vec!["a//b".into()]),
        with(&|r| r.outputs = vec!["d/".into()]),
        with(&|r| r.cwd = "in/".into()),
    ];
    for req in &bad {
        assert_eq!(refusal(client.run(req)), Refusal::BadRequest, "{req:?}");
    }
    assert!(!shares.outputs.join("stdout").exists());
    // A symlink that stays inside the share is fine, and the run is still unused.
    let mut req = base.clone();
    req.cwd = "in-link".into();
    client.run(&req).expect("started");
    assert_eq!(client.wait().expect("exited").end, End::Exited(0));
}

/// Catches a program that cannot start leaving the boot able to run another, and a
/// restarted agent running a second command over the first one's stdout.
#[test]
fn a_failed_start_uses_the_run_and_an_existing_stdout_refuses_one() {
    let shares = Shares::new("start-failed");
    let served = Served::start(shares.config());
    let mut client = served.connect();
    let mut req = sh("true", &[]);
    req.argv = vec!["/nonexistent/program".into()];
    assert_eq!(refusal(client.run(&req)), Refusal::StartFailed);
    assert_eq!(refusal(client.run(&sh("true", &[]))), Refusal::AlreadyRan);
    drop(client);
    served.join();

    // A second agent on the same outputs share: what a restarted LaunchAgent sees.
    let served = Served::start(shares.config());
    let mut client = served.connect();
    assert_eq!(refusal(client.run(&sh("true", &[]))), Refusal::StartFailed);
}

/// Catches garbage after the handshake taken as a message, or answered without
/// closing the connection.
#[test]
fn an_unreadable_frame_closes_the_connection() {
    let shares = Shares::new("garbage");
    let served = Served::start(shares.config());
    let mut client = served.connect();
    client.send(&HostMsg::Kill).expect("sent");
    drop(client);
    let mut host = served.connect_raw();
    send(
        &mut host,
        &HostMsg::Hello {
            version: VERSION,
            token: TOKEN,
        },
    );
    assert!(matches!(recv(&mut host), GuestMsg::Ready { .. }));
    write_frame(&mut host, &[0x7f]).expect("sent");
    assert!(matches!(
        recv(&mut host),
        GuestMsg::Refused {
            reason: Refusal::BadRequest,
            ..
        }
    ));
    assert!(read_frame(&mut host).is_err(), "the connection stayed open");
    let agent = served.join();
    assert!(!agent.has_run());
}
