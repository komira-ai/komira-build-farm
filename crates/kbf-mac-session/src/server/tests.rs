use std::io::Read as _;
use std::os::unix::fs::MetadataExt as _;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;
use crate::client::{Client, ClientError, Exit};
use crate::helper::Settings;
use crate::lease::UidRange;
use crate::ledger::Ledger;
use crate::proto::Request;
use crate::sweep::SweepPlan;
use crate::testing::{FakeHost, scratch, trace};

fn me() -> u32 {
    rustix::process::getuid().as_raw()
}

fn my_gid() -> u32 {
    rustix::process::getgid().as_raw()
}

/// The process a [`Scripted`] check names when its script runs out.
fn caller(pid: i64) -> Caller {
    Caller {
        token: vec![u8::try_from(pid).unwrap(); 4],
        pid,
        version: 1,
    }
}

/// A caller check over a script: `identify` answers from `ids` in turn (then
/// `caller(1)`), `check` answers `verdict`; `asked` counts the identifications.
struct Scripted {
    ids: Mutex<Vec<Result<Caller, String>>>,
    verdict: Result<(), String>,
    asked: AtomicUsize,
}

impl Scripted {
    fn new(ids: Vec<Result<Caller, String>>, verdict: Result<(), String>) -> Arc<Self> {
        Arc::new(Self {
            ids: Mutex::new(ids.into_iter().rev().collect()),
            verdict,
            asked: AtomicUsize::new(0),
        })
    }

    fn fixed(verdict: Result<(), String>) -> Arc<Self> {
        Self::new(Vec::new(), verdict)
    }
}

impl CallerCheck for Scripted {
    fn identify(&self, _socket: &UnixStream) -> Result<Caller, String> {
        self.asked.fetch_add(1, Ordering::SeqCst);
        self.ids
            .lock()
            .unwrap()
            .pop()
            .unwrap_or_else(|| Ok(caller(1)))
    }

    fn check(&self, _caller: &Caller) -> Result<(), String> {
        self.verdict.clone()
    }
}

struct Served {
    host: FakeHost,
    client: Client,
    dir: PathBuf,
}

/// A helper over a fake host, serving on a socket in a scratch directory.
fn served(name: &str, verdict: Result<(), String>) -> Served {
    served_with(name, Scripted::fixed(verdict))
}

fn served_with(name: &str, check: Arc<Scripted>) -> Served {
    trace();
    let dir = scratch(&format!("server-{name}"));
    std::fs::create_dir(dir.join("Users")).unwrap();
    let host = FakeHost::default();
    let settings = Settings {
        range: UidRange::new(me(), me() + 1).unwrap(),
        gid: my_gid(),
        homes: dir.join("Users"),
        schedules: SweepPlan {
            named: Vec::new(),
            owned: Vec::new(),
        },
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
    let check: Arc<dyn CallerCheck> = check;
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

/// Catches: the caller checked only after its request was read (S4.3, the
/// connect-then-exec case: a process that writes its request and then executes the
/// genuine daemon is checked as the daemon). The refusal comes before the caller has
/// written a byte, and the caller is identified once, at accept.
#[test]
fn a_caller_is_checked_before_anything_is_read() {
    let check = Scripted::fixed(Err("not the daemon".to_owned()));
    let served = served_with("checked-first", Arc::clone(&check));
    let stream = UnixStream::connect(socket_path(&served)).unwrap();
    let (reply, _) = proto::recv::<Reply>(stream.as_fd(), 0).unwrap().unwrap();
    assert_eq!(
        reply,
        Reply::Refused {
            reason: "caller refused: not the daemon".to_owned()
        }
    );
    assert!(proto::recv::<Reply>(stream.as_fd(), 0).unwrap().is_none());
    assert_eq!(check.asked.load(Ordering::SeqCst), 1);
}

/// Catches: the second identification dropped, or compared loosely: a process that
/// passed the check hands the connection to another (a child it forked) to write the
/// request, or the kernel can no longer say who is there.
#[test]
fn a_request_from_another_process_than_the_checked_one_is_refused() {
    let check = Scripted::new(
        vec![
            Ok(caller(7)),
            Ok(caller(8)),
            Ok(caller(7)),
            Err("gone".to_owned()),
        ],
        Ok(()),
    );
    let served = served_with("changed", check);
    match served.client.user_create("2.2", None) {
        Err(ClientError::Refused(why)) => assert_eq!(
            why,
            "caller refused: the process at the other end changed since the check \
             (pid 7 version 1, now pid 8 version 1)"
        ),
        other => panic!("{other:?}"),
    }
    match served.client.user_create("2.2", None) {
        Err(ClientError::Refused(why)) => assert_eq!(why, "caller refused: gone"),
        other => panic!("{other:?}"),
    }
    assert!(served.host.state().users.is_empty());
    // The same pid with another version is another process.
    let check = Scripted::new(
        vec![
            Ok(caller(7)),
            Ok(Caller {
                version: 2,
                ..caller(7)
            }),
        ],
        Ok(()),
    );
    let served = served_with("changed-version", check);
    match served.client.user_create("2.3", None) {
        Err(ClientError::Refused(why)) => assert!(why.contains("now pid 7 version 2"), "{why}"),
        other => panic!("{other:?}"),
    }
    assert!(served.host.state().users.is_empty());
}

/// Catches: the nonce not checked, so a request written before the helper's check (by
/// a process that then executed the genuine daemon before the helper accepted) is
/// carried out.
#[test]
fn a_request_written_before_the_hello_is_refused() {
    let served = served("early", Ok(()));
    let stream = UnixStream::connect(socket_path(&served)).unwrap();
    let call = Call {
        nonce: "00000000000000000000000000000000".to_owned(),
        request: Request::UserCreate {
            lease: "2.4".to_owned(),
            grant: None,
        },
    };
    proto::send(stream.as_fd(), &call, &[]).unwrap();
    let hello = proto::recv::<Reply>(stream.as_fd(), 0).unwrap().unwrap().0;
    assert!(
        matches!(&hello, Reply::Hello { nonce } if nonce.len() == 32 && *nonce != call.nonce),
        "{hello:?}"
    );
    let (reply, _) = proto::recv::<Reply>(stream.as_fd(), 0).unwrap().unwrap();
    assert_eq!(
        reply,
        Reply::Refused {
            reason: "caller refused: the request does not carry this connection's nonce".to_owned()
        }
    );
    assert!(served.host.state().users.is_empty());
}

/// Catches: a nonce that repeats, which a request written early could then carry.
#[test]
fn every_connection_gets_a_fresh_nonce() {
    let nonces: std::collections::BTreeSet<String> = (0..64).map(|_| fresh_nonce()).collect();
    assert_eq!(nonces.len(), 64);
}

/// Catches: no read timeout, under which a member of the socket's group ties up a
/// thread and its descriptors for ever by never sending its request.
#[test]
fn a_caller_that_sends_nothing_is_dropped() {
    trace();
    let dir = scratch("server-silent");
    let helper = quiet_helper(&dir);
    let (caller_end, helper_end) = UnixStream::pair().unwrap();
    let check = Scripted::fixed(Ok(()));
    connection(
        &helper_end,
        &helper,
        check.as_ref(),
        Duration::from_millis(50),
    );
    let hello = proto::recv::<Reply>(caller_end.as_fd(), 0)
        .unwrap()
        .unwrap()
        .0;
    assert!(matches!(hello, Reply::Hello { .. }), "{hello:?}");
    let (reply, _) = proto::recv::<Reply>(caller_end.as_fd(), 0)
        .unwrap()
        .unwrap();
    assert!(
        matches!(&reply, Reply::Refused { reason } if reason.starts_with("bad request")),
        "{reply:?}"
    );
    // A timeout the socket does not take is a refusal, not a wait without end.
    let (caller_end, helper_end) = UnixStream::pair().unwrap();
    connection(&helper_end, &helper, check.as_ref(), Duration::ZERO);
    proto::recv::<Reply>(caller_end.as_fd(), 0)
        .unwrap()
        .unwrap();
    let (reply, _) = proto::recv::<Reply>(caller_end.as_fd(), 0)
        .unwrap()
        .unwrap();
    assert!(
        matches!(&reply, Reply::Refused { reason } if reason.contains("setting a read timeout")),
        "{reply:?}"
    );
}

#[test]
fn a_malformed_request_is_refused_and_an_empty_one_ignored() {
    let served = served("malformed", Ok(()));
    let socket = served.client.clone();
    let stream = UnixStream::connect(socket_path(&served)).unwrap();
    proto::recv::<Reply>(stream.as_fd(), 0).unwrap().unwrap();
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
        // First a helper that answers the connection with something other than Hello.
        let (stream, _) = listener.accept().unwrap();
        proto::send(stream.as_fd(), &Reply::Killed, &[]).unwrap();
        drop(stream);
        let mut answers = answers.into_iter();
        while let Some(answer) = answers.next() {
            let (stream, _) = listener.accept().unwrap();
            let hello = Reply::Hello {
                nonce: "n".to_owned(),
            };
            proto::send(stream.as_fd(), &hello, &[]).unwrap();
            let _ = proto::recv::<Call>(stream.as_fd(), RUN_FDS);
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
    assert!(unexpected(client.kill_uid("1.1")).contains("Killed"));
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
            schedules: SweepPlan {
                named: Vec::new(),
                owned: Vec::new(),
            },
            sweep: SweepPlan {
                named: Vec::new(),
                owned: Vec::new(),
            },
            grant_keys: None,
            serial: "S".to_owned(),
        };
        Arc::new(Helper::new(Box::new(FakeHost::default()), settings, ledger))
    };
    let check: Arc<dyn CallerCheck> = Scripted::fixed(Ok(()));
    let error = serve(&listener, &helper, &check);
    assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
}

/// A caller that hangs up before the hello or the reply costs the helper nothing but a
/// log line.
#[test]
fn a_caller_that_left_is_not_an_error() {
    trace();
    let dir = scratch("server-left");
    let helper = quiet_helper(&dir);
    let check = Scripted::fixed(Ok(()));
    let (caller, helper_end) = UnixStream::pair().unwrap();
    drop(caller);
    connection(&helper_end, &helper, check.as_ref(), REQUEST_TIMEOUT);
    // Gone after its request: the reply finds no one.
    let (caller, helper_end) = UnixStream::pair().unwrap();
    let thread = std::thread::spawn(move || {
        let hello = proto::recv::<Reply>(caller.as_fd(), 0).unwrap().unwrap().0;
        let Reply::Hello { nonce } = hello else {
            panic!("{hello:?}")
        };
        let request = Request::KillUid {
            lease: "1.1".to_owned(),
        };
        proto::send(caller.as_fd(), &Call { nonce, request }, &[]).unwrap();
    });
    connection(&helper_end, &helper, check.as_ref(), REQUEST_TIMEOUT);
    thread.join().unwrap();
}

/// A helper over a fake host whose leases nothing in the test creates.
fn quiet_helper(dir: &Path) -> Helper {
    let ledger = Ledger::open(&dir.join("ledger")).unwrap();
    let settings = Settings {
        range: UidRange::new(600, 601).unwrap(),
        gid: 20,
        homes: dir.to_path_buf(),
        schedules: SweepPlan {
            named: Vec::new(),
            owned: Vec::new(),
        },
        sweep: SweepPlan {
            named: Vec::new(),
            owned: Vec::new(),
        },
        grant_keys: None,
        serial: "S".to_owned(),
    };
    Helper::new(Box::new(FakeHost::default()), settings, ledger)
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
