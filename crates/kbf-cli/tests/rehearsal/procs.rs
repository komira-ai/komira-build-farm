//! A real `kbf` process, its output in files so a failed rehearsal can say why.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

/// How long a rehearsal waits for what should happen promptly: it ends a hang only.
pub(crate) const HANG: Duration = Duration::from_secs(60);

/// The binary under test.
const BIN: &str = env!("CARGO_BIN_EXE_kbf");

pub(crate) struct Proc {
    pub(crate) name: String,
    child: Child,
    out: PathBuf,
    err: PathBuf,
}

impl Proc {
    /// Starts `kbf argv` in `cwd` with `env` added to this process's environment, its
    /// stdout and stderr in `<logs>/<name>.out` and `.err`.
    pub(crate) fn start(
        name: &str,
        logs: &Path,
        argv: &[String],
        env: &[(String, String)],
        cwd: &Path,
    ) -> Self {
        let out = logs.join(format!("{name}.out"));
        let err = logs.join(format!("{name}.err"));
        let child = Command::new(BIN)
            .args(argv)
            .envs(env.iter().map(|(k, v)| (k, v)))
            .current_dir(cwd)
            .stdout(Stdio::from(File::create(&out).expect("stdout file")))
            .stderr(Stdio::from(File::create(&err).expect("stderr file")))
            .spawn()
            .expect("spawn kbf");
        Self {
            name: name.to_owned(),
            child,
            out,
            err,
        }
    }

    /// What the process printed, for a failure message.
    pub(crate) fn log(&self) -> String {
        let read = |path: &Path| std::fs::read_to_string(path).unwrap_or_default();
        format!(
            "--- {} stdout ---\n{}--- {} stderr ---\n{}",
            self.name,
            read(&self.out),
            self.name,
            read(&self.err)
        )
    }

    /// Its exit status, if it has ended.
    pub(crate) fn exited(&mut self) -> Option<ExitStatus> {
        self.child.try_wait().expect("wait")
    }

    /// The first stdout line that starts with `prefix`, once the process printed it.
    pub(crate) fn line(&self, prefix: &str) -> Option<String> {
        let text = std::fs::read_to_string(&self.out).ok()?;
        text.lines()
            .find(|line| line.starts_with(prefix))
            .map(str::to_owned)
    }

    /// SIGTERM, and the exit status once the process has drained (the process is then
    /// ended; dropping it kills nothing).
    pub(crate) fn term(&mut self) -> ExitStatus {
        let pid = rustix::process::Pid::from_child(&self.child);
        rustix::process::kill_process(pid, rustix::process::Signal::TERM).expect("SIGTERM");
        let deadline = Instant::now() + HANG;
        loop {
            if let Some(status) = self.exited() {
                return status;
            }
            assert!(Instant::now() < deadline, "still running\n{}", self.log());
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// SIGKILL: the process is gone, as a host losing power would take it.
    pub(crate) fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        self.kill();
    }
}

/// The value of `key` in a start line such as `kbf-frontend 1.0 reapi=127.0.0.1:80 ...`.
pub(crate) fn field<'a>(line: &'a str, key: &str) -> &'a str {
    line.split(' ')
        .find_map(|word| word.strip_prefix(key))
        .unwrap_or_else(|| panic!("no {key} in {line:?}"))
}
