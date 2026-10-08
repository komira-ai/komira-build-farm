//! A command-line caller of `kbf-mac-session`, for the macOS CI job
//! (`tools/ci/mac-session-tests.sh`). It stands in for `kbf-daemon`: the helper is
//! started with this binary's cdhash as the daemon requirement.
//!
//! ```text
//! mac_session_client create <socket> <lease> [grant JSON]   prints the uid
//! mac_session_client run <socket> <lease> <dir> <argv>...   prints "exit <code>" or "signal <n>"
//! mac_session_client kill <socket> <lease>
//! mac_session_client delete <socket> <lease>           prints "existed" or "absent"
//! mac_session_client grant <seed> <serial> <lease> <issued>   prints a grant
//! mac_session_client pubkey <seed>                     prints the grant key file line
//! mac_session_client connect-then-exec <socket> <program> <args>...
//! mac_session_client write-then-exec <socket> <lease> <program> <args>...
//! mac_session_client replies                           prints the replies on KBF_CI_FD
//! mac_session_client exec-probe                        prints audit tokens across an exec
//! ```
//!
//! The attack S4.3 names, connecting as one binary and then becoming the genuine one,
//! in two forms:
//! - `connect-then-exec` connects, waits (up to 3 seconds) until the helper has
//!   answered the connection, then executes `<program>` with the connection open as
//!   `KBF_CI_FD`; a `kill` run that way carries out the whole exchange over that
//!   connection, as the genuine binary.
//! - `write-then-exec` connects, writes a `kill-uid <lease>` request at once (with a
//!   made-up nonce), then executes `<program>`, for example `replies`, which only reads
//!   and prints what the helper answers on `KBF_CI_FD`.
//!
//! `exec-probe` measures, without the helper, what `LOCAL_PEERTOKEN` reports for a
//! process that executes another program: the pid and pid version of the token before
//! the exec, after it, and once the process has exited.
//!
//! A refusal prints the helper's reason and exits 3; any other failure exits 4.

#[cfg(target_os = "macos")]
fn main() -> std::process::ExitCode {
    macos::main()
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("mac_session_client: macOS only");
}

#[cfg(target_os = "macos")]
mod macos {
    use std::io::{Read as _, Write as _};
    use std::os::fd::{AsFd as _, AsRawFd as _, FromRawFd as _};
    use std::os::unix::net::UnixStream;
    use std::os::unix::process::CommandExt as _;
    use std::process::{Command, ExitCode};

    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;
    use ed25519_dalek::{Signer as _, SigningKey};
    use kbf_mac_session::client::{self, Client, ClientError};
    use kbf_mac_session::grant::AdminGrant;
    use kbf_mac_session::proto::{self, Reply, Request};
    use time::OffsetDateTime;
    use time::format_description::well_known::Rfc3339;

    pub fn main() -> ExitCode {
        let args: Vec<String> = std::env::args().skip(1).collect();
        let words: Vec<&str> = args.iter().map(String::as_str).collect();
        match run(&words) {
            Ok(()) => ExitCode::SUCCESS,
            Err(ClientError::Refused(why)) => {
                eprintln!("refused: {why}");
                ExitCode::from(3)
            }
            Err(why) => {
                eprintln!("failed: {why}");
                ExitCode::from(4)
            }
        }
    }

    fn key(seed: &str) -> SigningKey {
        let byte = u8::from_str_radix(seed, 16).expect("a seed is one hex byte");
        SigningKey::from_bytes(&[byte; 32])
    }

    fn run(words: &[&str]) -> Result<(), ClientError> {
        match words {
            ["create", socket, lease, rest @ ..] => {
                let grant: Option<AdminGrant> = rest
                    .first()
                    .map(|json| serde_json::from_str(json).expect("a grant-admin answer"));
                let uid = Client::new(socket).user_create(lease, grant.as_ref())?;
                println!("{uid}");
            }
            ["run", socket, lease, dir, argv @ ..] => {
                let dir = std::fs::File::open(dir).map_err(ClientError::Io)?;
                let null = std::fs::File::open("/dev/null").map_err(ClientError::Io)?;
                let argv: Vec<String> = argv.iter().map(|&a| a.to_owned()).collect();
                let stdout = std::io::stdout();
                let stderr = std::io::stderr();
                let fds = [null.as_fd(), stdout.as_fd(), stderr.as_fd(), dir.as_fd()];
                let exit = Client::new(socket).run(lease, &argv, &[], fds)?.wait()?;
                match (exit.code, exit.signal) {
                    (Some(code), _) => println!("exit {code}"),
                    (_, Some(signal)) => println!("signal {signal}"),
                    _ => println!("unknown"),
                }
            }
            ["kill", socket, lease] => {
                let stream = match inherited() {
                    Some(stream) => stream,
                    None => UnixStream::connect(socket).map_err(ClientError::Io)?,
                };
                let request = Request::KillUid {
                    lease: (*lease).to_owned(),
                };
                match client::call_on(stream, &request, &[])?.1 {
                    Reply::Killed => {}
                    Reply::Refused { reason } => return Err(ClientError::Refused(reason)),
                    other => return Err(ClientError::Unexpected(other)),
                }
            }
            ["replies"] => {
                let stream = inherited().expect("KBF_CI_FD names the connection");
                let mut last = None;
                while let Some((reply, _)) =
                    proto::recv::<Reply>(stream.as_fd(), 0).map_err(ClientError::Io)?
                {
                    println!("{reply:?}");
                    last = Some(reply);
                }
                match last {
                    Some(Reply::Refused { reason }) => return Err(ClientError::Refused(reason)),
                    Some(Reply::Killed) => {}
                    other => return Err(ClientError::Unexpected(other.unwrap_or(Reply::Killed))),
                }
            }
            ["exec-probe"] => exec_probe().map_err(ClientError::Io)?,
            ["probe-child"] => {
                // exec-probe's child: says it is there, waits for the go, becomes sleep.
                let stream = inherited().expect("KBF_CI_FD names the probe's socket");
                (&stream).write_all(b"x").map_err(ClientError::Io)?;
                let mut go = [0u8; 1];
                (&stream).read_exact(&mut go).map_err(ClientError::Io)?;
                let error = Command::new("/bin/sleep").arg("30").exec();
                return Err(ClientError::Io(error));
            }
            ["delete", socket, lease] => {
                let existed = Client::new(socket).user_delete(lease)?;
                println!("{}", if existed { "existed" } else { "absent" });
            }
            ["grant", seed, serial, lease, issued] => {
                // What the gate's `grant-admin` answers (kbf-mdm's `GrantKey::sign`):
                // valid for an hour from `issued`.
                let issued: i64 = issued.parse().expect("issued is unix seconds");
                let time = |secs: i64| {
                    OffsetDateTime::from_unix_timestamp(secs)
                        .expect("a time")
                        .format(&Rfc3339)
                        .expect("RFC 3339")
                };
                let grant = format!(
                    "kbf-grant-v1\nserial {serial}\nlease {lease}\nissued {}\nnot-after {}\n",
                    time(issued),
                    time(issued + 3600)
                );
                let signature = key(seed).sign(grant.as_bytes());
                let answer = serde_json::json!({
                    "grant": grant,
                    "signature": STANDARD.encode(signature.to_bytes()),
                    "key": STANDARD.encode(key(seed).verifying_key().to_bytes()),
                    "erase_at": time(issued + 7200),
                });
                println!("{answer}");
            }
            ["pubkey", seed] => {
                println!("{}", STANDARD.encode(key(seed).verifying_key().to_bytes()))
            }
            ["connect-then-exec", socket, program, args @ ..] => {
                let stream = UnixStream::connect(socket).map_err(ClientError::Io)?;
                // Wait for the helper's first frame without reading it, so that the
                // helper has seen this process, not the program it becomes.
                let mut poll = [libc::pollfd {
                    fd: stream.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                }];
                // SAFETY: one valid pollfd, which the call may write.
                if unsafe { libc::poll(poll.as_mut_ptr(), 1, 3000) } < 0 {
                    return Err(ClientError::Io(std::io::Error::last_os_error()));
                }
                exec_with(&stream, program, args)?;
            }
            ["write-then-exec", socket, lease, program, args @ ..] => {
                let stream = UnixStream::connect(socket).map_err(ClientError::Io)?;
                let call = proto::Call {
                    nonce: "0".repeat(32),
                    request: Request::KillUid {
                        lease: (*lease).to_owned(),
                    },
                };
                proto::send(stream.as_fd(), &call, &[]).map_err(ClientError::Io)?;
                exec_with(&stream, program, args)?;
            }
            _ => {
                eprintln!("usage: see the source of examples/mac_session_client.rs");
                return Err(ClientError::Io(std::io::ErrorKind::InvalidInput.into()));
            }
        }
        Ok(())
    }

    /// The connection a parent left open as `KBF_CI_FD`, if any.
    fn inherited() -> Option<UnixStream> {
        let fd = std::env::var("KBF_CI_FD").ok()?;
        let fd = fd.parse().expect("KBF_CI_FD is a descriptor");
        // SAFETY: the descriptor was left open across exec for this, and nothing else
        // owns it.
        Some(unsafe { UnixStream::from_raw_fd(fd) })
    }

    /// Executes `program` with `stream` left open as `KBF_CI_FD`; returns only on
    /// failure.
    fn exec_with(stream: &UnixStream, program: &str, args: &[&str]) -> Result<(), ClientError> {
        let fd = stream.as_raw_fd();
        rustix::io::fcntl_setfd(stream.as_fd(), rustix::io::FdFlags::empty())
            .map_err(|e| ClientError::Io(e.into()))?;
        let error = Command::new(program)
            .args(args.iter())
            .env("KBF_CI_FD", fd.to_string())
            .exec();
        Err(ClientError::Io(error))
    }

    /// The pid and pid version `LOCAL_PEERTOKEN` reports for the other end of `socket`
    /// (words 5 and 7 of the audit token), or the error.
    fn peer(socket: &UnixStream) -> String {
        let mut token = [0u32; 8];
        let mut len = std::mem::size_of_val(&token) as libc::socklen_t;
        // SAFETY: `token` is a writable buffer of `len` bytes and `len` a writable
        // socklen_t; the call writes at most `len` bytes.
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
            return format!("no token ({})", std::io::Error::last_os_error());
        }
        format!("pid {} version {}", token[5], token[7])
    }

    /// See the module's documentation.
    fn exec_probe() -> std::io::Result<()> {
        let (mut mine, theirs) = UnixStream::pair()?;
        rustix::io::fcntl_setfd(theirs.as_fd(), rustix::io::FdFlags::empty())?;
        let mut child = Command::new(std::env::current_exe()?)
            .arg("probe-child")
            .env("KBF_CI_FD", theirs.as_raw_fd().to_string())
            .spawn()?;
        drop(theirs);
        let mut byte = [0u8; 1];
        mine.read_exact(&mut byte)?;
        let before = peer(&mine);
        mine.write_all(b"g")?;
        std::thread::sleep(std::time::Duration::from_secs(1));
        let after = peer(&mine);
        child.kill()?;
        child.wait()?;
        let exited = peer(&mine);
        println!(
            "exec-probe: child {}; before exec {before}; after exec {after}; after exit {exited}",
            child.id()
        );
        Ok(())
    }
}
