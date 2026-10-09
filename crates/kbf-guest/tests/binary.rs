//! The `kbf-guest` binary: its flags, the token file, and one run over a real socket.

mod common;

use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use common::{Shares, TOKEN, sh};
use kbf_guest::client::Client;
use kbf_guest::wire::End;

const BIN: &str = env!("CARGO_BIN_EXE_kbf-guest");

struct Killed(Child);

impl Drop for Killed {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn serve(shares: &Shares, socket: &Path, token_file: &Path) -> Command {
    let mut cmd = Command::new(BIN);
    cmd.arg("serve")
        .arg("--unix-socket")
        .arg(socket)
        .arg("--token-file")
        .arg(token_file)
        .arg("--inputs")
        .arg(&shares.inputs)
        .arg("--outputs")
        .arg(&shares.outputs);
    cmd
}

/// Catches the binary not wiring its flags to the agent: the token file's hex is the
/// token, and a command runs over the socket it was told to create.
#[test]
fn serve_runs_one_command_over_its_socket() {
    let shares = Shares::new("bin-serve");
    let token_file = shares.inputs.join("kbf-guest.token");
    std::fs::write(&token_file, format!("{}\n", hex::encode(TOKEN))).expect("token");
    let socket = shares.outputs.with_file_name("s.sock");
    let _guest = Killed(
        serve(&shares, &socket, &token_file)
            .spawn()
            .expect("spawned"),
    );
    let give_up = Instant::now() + Duration::from_secs(20);
    let stream = loop {
        if let Ok(s) = UnixStream::connect(&socket) {
            break s;
        }
        assert!(Instant::now() < give_up, "the socket never appeared");
        std::thread::sleep(Duration::from_millis(20));
    };
    let mut client = Client::connect(stream, TOKEN).expect("handshake");
    if cfg!(target_os = "macos") {
        assert!(!client.session().is_empty());
    } else {
        assert_eq!(client.session(), "");
    }
    client.run(&sh("echo ran", &[])).expect("started");
    assert_eq!(client.wait().expect("exited").end, End::Exited(0));
    assert_eq!(shares.out("stdout"), b"ran\n");
}

/// Catches a token file that is not checked: short, not hex, or absent.
#[test]
fn a_bad_token_file_stops_serve() {
    let shares = Shares::new("bin-token");
    let socket = shares.outputs.with_file_name("t.sock");
    let token_file = shares.inputs.join("token");
    for content in [Some("abcd".to_owned()), Some("zz".repeat(32)), None] {
        match &content {
            Some(c) => std::fs::write(&token_file, c).expect("token"),
            None => {
                let _ = std::fs::remove_file(&token_file);
            }
        }
        let out = serve(&shares, &socket, &token_file).output().expect("ran");
        assert_eq!(out.status.code(), Some(1), "{content:?}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains(&token_file.display().to_string()),
            "{stderr}"
        );
        assert!(!socket.exists());
    }
}

/// Catches `session` printing something off macOS, where there is no launchd session
/// to report (macOS is checked from launchd by tools/ci/guest-macos.sh).
#[test]
fn session_prints_the_session_type() {
    let out = Command::new(BIN).arg("session").output().expect("ran");
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).expect("utf-8");
    if cfg!(target_os = "macos") {
        assert!(!text.trim().is_empty());
    } else {
        assert_eq!(text, "\n");
    }
}
