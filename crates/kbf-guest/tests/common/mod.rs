//! Shares, an agent on one end of a socket pair, and a client on the other.

#![allow(dead_code)]

use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use kbf_guest::client::Client;
use kbf_guest::server::{Agent, Config};
use kbf_guest::wire::{RunRequest, TOKEN_LEN};

pub const TOKEN: [u8; TOKEN_LEN] = [0x5a; TOKEN_LEN];

/// A fresh inputs and outputs share for one test.
pub struct Shares {
    pub inputs: PathBuf,
    pub outputs: PathBuf,
}

impl Shares {
    pub fn new(name: &str) -> Self {
        let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join("kbf-guest")
            .join(name);
        let _ = std::fs::remove_dir_all(&root);
        let inputs = root.join("inputs");
        let outputs = root.join("outputs");
        std::fs::create_dir_all(&inputs).expect("inputs");
        std::fs::create_dir_all(&outputs).expect("outputs");
        Self { inputs, outputs }
    }

    pub fn config(&self) -> Config {
        Config {
            token: TOKEN,
            inputs: self.inputs.clone(),
            outputs: self.outputs.clone(),
            session: "TestSession".into(),
            hello_timeout: Duration::from_secs(10),
        }
    }

    pub fn out(&self, name: &str) -> Vec<u8> {
        std::fs::read(self.outputs.join(name)).expect("output file")
    }
}

/// An agent serving connections handed to it, one at a time, until the sender is
/// dropped; joining it returns the agent.
pub struct Served {
    conns: Option<mpsc::Sender<UnixStream>>,
    thread: Option<JoinHandle<Agent>>,
}

impl Served {
    pub fn start(config: Config) -> Self {
        let (tx, rx) = mpsc::channel::<UnixStream>();
        let thread = std::thread::spawn(move || {
            let mut agent = Agent::new(config);
            for stream in rx {
                agent.serve_connection(stream).expect("served");
            }
            agent
        });
        Self {
            conns: Some(tx),
            thread: Some(thread),
        }
    }

    /// A new connection; the host's end.
    pub fn connect_raw(&self) -> UnixStream {
        let (host, guest) = UnixStream::pair().expect("socket pair");
        self.conns
            .as_ref()
            .expect("open")
            .send(guest)
            .expect("agent alive");
        host
    }

    /// A new connection past the handshake.
    pub fn connect(&self) -> Client<UnixStream> {
        Client::connect(self.connect_raw(), TOKEN).expect("handshake")
    }

    /// Stops accepting and returns the agent once every connection is done.
    pub fn join(mut self) -> Agent {
        drop(self.conns.take());
        self.thread
            .take()
            .expect("thread")
            .join()
            .expect("agent thread")
    }
}

/// `sh -c script` with a fixed PATH and the extra variables.
pub fn sh(script: &str, env: &[(&str, &str)]) -> RunRequest {
    let mut vars = vec![("PATH".to_owned(), "/usr/bin:/bin".to_owned())];
    vars.extend(env.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())));
    RunRequest {
        argv: vec!["/bin/sh".into(), "-c".into(), script.into()],
        env: vars,
        ..RunRequest::default()
    }
}

/// Waits for a file a command writes and returns its contents, trimmed.
pub fn wait_for_file(path: &Path) -> String {
    let give_up = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(s) = std::fs::read_to_string(path)
            && s.ends_with('\n')
        {
            return s.trim().to_owned();
        }
        assert!(
            Instant::now() < give_up,
            "{} never appeared",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Whether `pid` names a live process (or an unreaped zombie).
pub fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    let rc = unsafe { libc::kill(pid, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}
