use std::io::Read as _;
use std::os::unix::fs::MetadataExt as _;
use std::path::PathBuf;

use super::*;
use crate::client::{Client, ClientError, Exit};
use crate::helper::Settings;
use crate::lease::UidRange;
use crate::ledger::Ledger;
use crate::sweep::SweepPlan;
use crate::testing::{FakeHost, scratch, trace};

fn me() -> u32 {
    rustix::process::getuid().as_raw()
}

fn my_gid() -> u32 {
    rustix::process::getgid().as_raw()
}

/// A caller check with a fixed answer.
struct Fixed(Result<(), String>);

impl CallerCheck for Fixed {
    fn check(&self, _socket: &UnixStream) -> Result<(), String> {
        self.0.clone()
    }
}

struct Served {
    host: FakeHost,
    client: Client,
    dir: PathBuf,
}

/// A helper over a fake host, serving on a socket in a scratch directory.
fn served(name: &str, check: Result<(), String>) -> Served {
    trace();
    let dir = scratch(&format!("server-{name}"));
    std::fs::create_dir(dir.join("Users")).unwrap();
    let host = FakeHost::default();
    let settings = Settings {
        range: UidRange::new(me(), me() + 1).unwrap(),
        gid: my_gid(),
        homes: dir.join("Users"),
        sweep: SweepPlan {
            named: Vec::new(),
            owned: Vec::new(),
        },
        grant_keys: None,
        serial: "S".to_owned(),
    };
    let ledger = Ledger::open(&dir.join("ledger")).unwrap();
    let helper = Arc::new(Helper::new(Box::new(host.clone()), settings, ledger));
    let socket = dir.join("run/socket");
    let listener = bind(&socket, my_gid()).unwrap();
    let check: Arc<dyn CallerCheck> = Arc::new(Fixed(check));
    std::thread::spawn(move || serve(&listener, &helper, &check));
    Served {
        host,
        client: Client::new(socket),
        dir,
    }
}

/// Catches: a socket any local user can reach (S10: "socket 0666"), or a directory
/// that lets other users in, or one whose group is not the daemon's.
#[test]
fn the_socket_and_its_directory_admit_only_their_group() {
    let dir = scratch("server-bind");
    let socket = dir.join("run/socket");
    let listener = bind(&socket, my_gid()).unwrap();
    let meta = std::fs::metadata(&socket).unwrap();
    assert_eq!(meta.permissions().mode() & 0o777, 0o660);
    assert_eq!(meta.gid(), my_gid());
    let parent = std::fs::metadata(dir.join("run")).unwrap();
    assert_eq!(parent.permissions().mode() & 0o777, 0o750);
    assert_eq!(parent.gid(), my_gid());
    drop(listener);

    // A stale socket is replaced, and a directory left too open is closed up.
    std::fs::set_permissions(dir.join("run"), std::fs::Permissions::from_mode(0o777)).unwrap();
    let _again = bind(&socket, my_gid()).unwrap();
    let parent = std::fs::metadata(dir.join("run")).unwrap();
    assert_eq!(parent.permissions().mode() & 0o777, 0o750);
}

/// Catches: the socket's directory taken through a link someone planted.
#[test]
fn a_linked_or_missing_directory_is_refused() {
    let dir = scratch("server-bind-bad");
    std::fs::create_dir(dir.join("elsewhere")).unwrap();
    std::os::unix::fs::symlink(dir.join("elsewhere"), dir.join("run")).unwrap();
    assert!(bind(&dir.join("run/socket"), my_gid()).is_err());
    assert!(bind(&dir.join("no/such/socket"), my_gid()).is_err());
    let error = bind(Path::new("socket"), my_gid()).unwrap_err();
    assert!(error.to_string().contains("needs a directory"), "{error}");
    let error = bind(&dir.join("elsewhere/.."), my_gid()).unwrap_err();
    assert!(error.to_string().contains("needs a name"), "{error}");
    std::fs::create_dir(dir.join("elsewhere/socket")).unwrap();
    assert!(bind(&dir.join("elsewhere/socket"), my_gid()).is_err());
}

/// The four verbs end to end over the socket, descriptors included.
#[test]
fn the_verbs_work_over_the_socket() {
    let served = served("verbs", Ok(()));
    let client = &served.client;
    assert_eq!(client.user_create("1.1", None).unwrap(), me());
    let (mut out, write) = std::io::pipe().unwrap();
    let null = std::fs::File::open("/dev/null").unwrap();
    let lease_dir = std::fs::File::open(&served.dir).unwrap();
    let argv = ["/bin/sh", "-c", "echo $PWD; exit 3"].map(str::to_owned);
    let running = client
        .run(
            "1.1",
            &argv,
            &[],
            [null.as_fd(), write.as_fd(), null.as_fd(), lease_dir.as_fd()],
        )
        .unwrap();
    drop(write);
    assert!(running.pid > 0);
    assert_eq!(
        running.wait().unwrap(),
        Exit {
            code: Some(3),
            signal: None
        }
    );
    let mut text = String::new();
    out.read_to_string(&mut text).unwrap();
    assert!(!text.is_empty());

    let argv = ["/bin/sh", "-c", "kill -9 $$"].map(str::to_owned);
    let running = client
        .run(
            "1.1",
            &argv,
            &[],
            [null.as_fd(), null.as_fd(), null.as_fd(), lease_dir.as_fd()],
        )
        .unwrap();
    assert_eq!(
        running.wait().unwrap(),
        Exit {
            code: None,
            signal: Some(9)
        }
    );

    client.kill_uid("1.1").unwrap();
    assert!(client.user_delete("1.1").unwrap());
    assert!(!client.user_delete("1.1").unwrap());
    match client.kill_uid("1.1") {
        Err(ClientError::Refused(why)) => assert!(why.contains("was deleted"), "{why}"),
        other => panic!("{other:?}"),
    }
}

/// Catches: the caller check removed (S10: "caller check removed"), or done after
/// the request was carried out: a refused caller changes nothing.
#[test]
fn a_refused_caller_gets_nothing_done() {
    let served = served("refused", Err("not the daemon".to_owned()));
    match served.client.user_create("2.1", None) {
        Err(ClientError::Refused(why)) => {
            assert_eq!(why, "caller refused: not the daemon");
        }
        other => panic!("{other:?}"),
    }
    assert!(served.host.state().log.is_empty());
    assert!(served.host.state().users.is_empty());
}

#[test]
fn a_malformed_request_is_refused_and_an_empty_one_ignored() {
    let served = served("malformed", Ok(()));
    let socket = served.client.clone();
    let stream = UnixStream::connect(socket_path(&served)).unwrap();
    proto::send(stream.as_fd(), &Reply::Killed, &[]).unwrap();
    let (reply, _) = proto::recv::<Reply>(stream.as_fd(), 0).unwrap().unwrap();
    assert!(
        matches!(&reply, Reply::Refused { reason } if reason.starts_with("bad request")),
        "{reply:?}"
    );
    drop(UnixStream::connect(socket_path(&served)).unwrap());
    // The server is still up.
    assert_eq!(socket.user_create("3.1", None).unwrap(), me());
}

fn socket_path(served: &Served) -> PathBuf {
    served.dir.join("run/socket")
}

/// The client's side of replies it does not expect, and of a helper that hangs up.
#[test]
fn the_client_reports_unexpected_replies_and_hangups() {
    let dir = scratch("server-client");
    let path = dir.join("socket");
    let listener = UnixListener::bind(&path).unwrap();
    let answers = [
        Some(Reply::Killed),
        Some(Reply::Created { uid: 1 }),
        Some(Reply::Killed),
        Some(Reply::Killed),
        Some(Reply::Started { pid: 7 }),
        None,
        Some(Reply::Started { pid: 7 }),
        Some(Reply::Killed),
    ];
    std::thread::spawn(move || {
        let mut answers = answers.into_iter();
        while let Some(answer) = answers.next() {
            let (stream, _) = listener.accept().unwrap();
            let _ = proto::recv::<Request>(stream.as_fd(), RUN_FDS);
            if let Some(answer) = answer {
                proto::send(stream.as_fd(), &answer, &[]).unwrap();
                if matches!(answer, Reply::Started { .. })
                    && let Some(Some(then)) = answers.next()
                {
                    proto::send(stream.as_fd(), &then, &[]).unwrap();
                }
            }
        }
    });
    let client = Client::new(&path);
    let unexpected = |result: Result<(), ClientError>| match result {
        Err(error @ ClientError::Unexpected(_)) => error.to_string(),
        other => panic!("{other:?}"),
    };
    assert!(unexpected(client.user_create("1.1", None).map(drop)).contains("Killed"));
    assert!(unexpected(client.kill_uid("1.1")).contains("Created"));
    assert!(unexpected(client.user_delete("1.1").map(drop)).contains("Killed"));
    let null = std::fs::File::open("/dev/null").unwrap();
    let fds = [null.as_fd(), null.as_fd(), null.as_fd(), null.as_fd()];
    assert!(unexpected(client.run("1.1", &[], &[], fds).map(drop)).contains("Killed"));
    // Started, then the helper hangs up before the exit.
    let running = client.run("1.1", &[], &[], fds).unwrap();
    match running.wait() {
        Err(error @ ClientError::Io(_)) => assert!(error.to_string().contains("kbf-mac-session")),
        other => panic!("{other:?}"),
    }
    let running = client.run("1.1", &[], &[], fds).unwrap();
    assert!(unexpected(running.wait().map(drop)).contains("Killed"));
    let refused = ClientError::Refused("why".to_owned());
    assert_eq!(refused.to_string(), "kbf-mac-session refused: why");
    let gone = Client::new(dir.join("no-socket"));
    assert!(matches!(gone.kill_uid("1.1"), Err(ClientError::Io(_))));
}

/// A failed `accept` ends serving with its error (the service manager restarts the
/// helper) rather than spinning.
#[test]
fn serve_returns_when_accept_fails() {
    let dir = scratch("server-accept");
    let listener = UnixListener::bind(dir.join("socket")).unwrap();
    listener.set_nonblocking(true).unwrap();
    let helper = {
        let ledger = Ledger::open(&dir.join("ledger")).unwrap();
        let settings = Settings {
            range: UidRange::new(600, 601).unwrap(),
            gid: 20,
            homes: dir.clone(),
            sweep: SweepPlan {
                named: Vec::new(),
                owned: Vec::new(),
            },
            grant_keys: None,
            serial: "S".to_owned(),
        };
        Arc::new(Helper::new(Box::new(FakeHost::default()), settings, ledger))
    };
    let check: Arc<dyn CallerCheck> = Arc::new(Fixed(Ok(())));
    let error = serve(&listener, &helper, &check);
    assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
}

/// A caller that hangs up before the reply costs the helper nothing but a log line.
#[test]
fn a_caller_that_left_is_not_an_error() {
    trace();
    let dir = scratch("server-left");
    let ledger = Ledger::open(&dir.join("ledger")).unwrap();
    let settings = Settings {
        range: UidRange::new(600, 601).unwrap(),
        gid: 20,
        homes: dir.clone(),
        sweep: SweepPlan {
            named: Vec::new(),
            owned: Vec::new(),
        },
        grant_keys: None,
        serial: "S".to_owned(),
    };
    let helper = Helper::new(Box::new(FakeHost::default()), settings, ledger);
    let (caller, helper_end) = UnixStream::pair().unwrap();
    let request = Request::KillUid {
        lease: "1.1".to_owned(),
    };
    proto::send(caller.as_fd(), &request, &[]).unwrap();
    drop(caller);
    connection(&helper_end, &helper, &Fixed(Ok(())));
}

#[test]
fn a_wait_that_fails_is_reported() {
    let reply = exited(Err(io::Error::other("gone")));
    assert_eq!(
        reply,
        Reply::Refused {
            reason: "waiting for the process: gone".to_owned()
        }
    );
}
