//! A command-line caller of `kbf-mac-session`, for the macOS CI job
//! (`tools/ci/mac-session-tests.sh`). It stands in for `kbf-daemon`: the helper is
//! started with this binary's cdhash as the daemon requirement.
//!
//! ```text
//! mac_session_client create <socket> <lease> [grant]   prints the uid
//! mac_session_client run <socket> <lease> <dir> <argv>...   prints "exit <code>" or "signal <n>"
//! mac_session_client kill <socket> <lease>
//! mac_session_client delete <socket> <lease>           prints "existed" or "absent"
//! mac_session_client grant <seed> <serial> <lease> <not-after>   prints a grant
//! mac_session_client pubkey <seed>                     prints the grant key file line
//! mac_session_client connect-then-exec <socket> <program> <args>...
//! ```
//!
//! `connect-then-exec` connects, then executes `<program>` with the connection open as
//! `KBF_CI_FD`; a `kill` run that way sends its request over that connection. It is
//! the attack S4.3 names: connect as one binary, then become the genuine one.
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
    use std::os::fd::{AsFd as _, AsRawFd as _, FromRawFd as _};
    use std::os::unix::net::UnixStream;
    use std::os::unix::process::CommandExt as _;
    use std::process::ExitCode;

    use base64::Engine as _;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use ed25519_dalek::{Signer as _, SigningKey};
    use kbf_mac_session::client::{Client, ClientError};
    use kbf_mac_session::proto::{self, Reply, Request};

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
                let uid = Client::new(socket).user_create(lease, rest.first().copied())?;
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
                let stream = match std::env::var("KBF_CI_FD") {
                    Ok(fd) => {
                        let fd = fd.parse().expect("KBF_CI_FD is a descriptor");
                        // SAFETY: the descriptor was left open across exec for this,
                        // and nothing else owns it.
                        unsafe { UnixStream::from_raw_fd(fd) }
                    }
                    Err(_) => UnixStream::connect(socket).map_err(ClientError::Io)?,
                };
                let request = Request::KillUid {
                    lease: (*lease).to_owned(),
                };
                proto::send(stream.as_fd(), &request, &[]).map_err(ClientError::Io)?;
                match proto::recv::<Reply>(stream.as_fd(), 0).map_err(ClientError::Io)? {
                    Some((Reply::Killed, _)) => {}
                    Some((Reply::Refused { reason }, _)) => {
                        return Err(ClientError::Refused(reason));
                    }
                    Some((other, _)) => return Err(ClientError::Unexpected(other)),
                    None => return Err(ClientError::Io(std::io::ErrorKind::UnexpectedEof.into())),
                }
            }
            ["delete", socket, lease] => {
                let existed = Client::new(socket).user_delete(lease)?;
                println!("{}", if existed { "existed" } else { "absent" });
            }
            ["grant", seed, serial, lease, not_after] => {
                let payload = format!(
                    "kbf-mac-admin-grant v1\nserial {serial}\nlease {lease}\nnot-after {not_after}\n"
                );
                let signature = key(seed).sign(payload.as_bytes());
                println!(
                    "{}.{}",
                    URL_SAFE_NO_PAD.encode(&payload),
                    URL_SAFE_NO_PAD.encode(signature.to_bytes())
                );
            }
            ["pubkey", seed] => println!("{}", hex::encode(key(seed).verifying_key().to_bytes())),
            ["connect-then-exec", socket, program, args @ ..] => {
                let stream = UnixStream::connect(socket).map_err(ClientError::Io)?;
                let fd = stream.as_raw_fd();
                rustix::io::fcntl_setfd(stream.as_fd(), rustix::io::FdFlags::empty())
                    .map_err(|e| ClientError::Io(e.into()))?;
                let error = std::process::Command::new(program)
                    .args(args.iter())
                    .env("KBF_CI_FD", fd.to_string())
                    .exec();
                return Err(ClientError::Io(error));
            }
            _ => {
                eprintln!("usage: see the source of examples/mac_session_client.rs");
                return Err(ClientError::Io(std::io::ErrorKind::InvalidInput.into()));
            }
        }
        Ok(())
    }
}
