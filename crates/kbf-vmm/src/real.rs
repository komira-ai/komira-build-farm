//! The real [`System`]: macOS, the host's tools and Virtualization.framework.

use std::io::{BufRead, BufReader};
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::hostfs::{chown_tree, sibling, tail_of};
use crate::spec::{Event, VmSpec};
use crate::system::{OfflineFile, Poll, Signature, System, VmProcess};
use crate::{offline, signature, vz};

pub struct RealSystem;

impl RealSystem {
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl Default for RealSystem {
    fn default() -> Self {
        Self::new()
    }
}

/// A helper process and the events its stdout carries.
struct ProcessVm {
    child: Child,
    stdin: Option<ChildStdin>,
    events: Receiver<Event>,
    /// Where the helper's stderr goes.
    log: PathBuf,
}

impl ProcessVm {
    fn gone(&mut self) -> Option<Poll> {
        match self.child.try_wait() {
            Ok(Some(s)) => Some(Poll::Gone {
                code: s.code(),
                signal: s.signal(),
            }),
            _ => None,
        }
    }
}

/// How much of the helper's log a failure report carries.
const LOG_TAIL_BYTES: usize = 2048;

impl VmProcess for ProcessVm {
    fn diagnostics(&self) -> Option<String> {
        tail_of(&self.log, LOG_TAIL_BYTES)
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }

    fn poll(&mut self, wait: Duration) -> Poll {
        match self.events.recv_timeout(wait) {
            Ok(e) => Poll::Event(e),
            Err(RecvTimeoutError::Timeout) => self.gone().unwrap_or(Poll::Quiet),
            Err(RecvTimeoutError::Disconnected) => {
                if let Some(g) = self.gone() {
                    return g;
                }
                std::thread::sleep(wait.min(Duration::from_millis(20)));
                self.gone().unwrap_or(Poll::Quiet)
            }
        }
    }

    fn kill9(&mut self) {
        let _ = self.child.kill();
    }

    fn request_stop(&mut self) {
        self.stdin = None;
    }

    fn wait_gone(&mut self, wait: Duration) -> bool {
        let end = Instant::now() + wait;
        loop {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return true;
            }
            if Instant::now() >= end {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for ProcessVm {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn run_text(program: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

impl System for RealSystem {
    fn clonefile(&self, src: &Path, dst: &Path) -> std::io::Result<()> {
        let to_c = |p: &Path| {
            std::ffi::CString::new(p.as_os_str().as_encoded_bytes())
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))
        };
        let (s, d) = (to_c(src)?, to_c(dst)?);
        // SAFETY: both are NUL-terminated paths; flags 0 follows no symlinks specially.
        if unsafe { libc::clonefile(s.as_ptr(), d.as_ptr(), 0) } == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }

    fn new_machine_identifier(&self) -> Result<Vec<u8>, String> {
        Ok(vz::new_machine_identifier())
    }

    fn start_vm(&self, spec: &VmSpec, run_as: Option<u32>) -> Result<Box<dyn VmProcess>, String> {
        let spec_path = sibling(&spec.bundle, "spec.json");
        let log_path = sibling(&spec.bundle, "helper.log");
        std::fs::write(
            &spec_path,
            serde_json::to_vec(spec).map_err(|e| e.to_string())?,
        )
        .map_err(|e| format!("write {}: {e}", spec_path.display()))?;
        let log = std::fs::File::create(&log_path)
            .map_err(|e| format!("create {}: {e}", log_path.display()))?;
        if let Some(uid) = run_as {
            let mut paths = vec![spec.bundle.clone(), spec_path.clone(), log_path.clone()];
            paths.extend(spec.automount_dir.clone());
            paths.extend(spec.tagged.as_ref().map(|t| t.path.clone()));
            for p in paths {
                chown_tree(&p, uid, uid)?;
            }
        }
        let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
        let mut cmd = Command::new(exe);
        cmd.arg("vm-run")
            .arg("--spec")
            .arg(&spec_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(log);
        if let Some(uid) = run_as {
            cmd.uid(uid).gid(uid);
        }
        let mut child = cmd.spawn().map_err(|e| format!("spawn the helper: {e}"))?;
        let stdout = child.stdout.take().ok_or("no helper stdout")?;
        let stdin = child.stdin.take();
        let (tx, events) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Ok(e) = Event::parse(&line)
                    && tx.send(e).is_err()
                {
                    break;
                }
            }
        });
        Ok(Box::new(ProcessVm {
            child,
            stdin,
            events,
            log: log_path,
        }))
    }

    fn purge_cache(&self) -> Result<(), String> {
        let out = Command::new("/usr/sbin/purge")
            .stdin(Stdio::null())
            .output()
            .map_err(|e| format!("run purge: {e}"))?;
        if out.status.success() {
            Ok(())
        } else {
            Err(format!(
                "purge failed ({}): {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            ))
        }
    }

    fn uid(&self) -> u32 {
        // SAFETY: getuid cannot fail.
        unsafe { libc::getuid() }
    }

    fn ppid(&self) -> u32 {
        // SAFETY: getppid cannot fail.
        u32::try_from(unsafe { libc::getppid() }).unwrap_or(0)
    }

    fn launchd_context(&self) -> Option<String> {
        run_text("/bin/launchctl", &["managername"])
    }

    fn console_uid(&self) -> Option<u32> {
        std::fs::metadata("/dev/console").ok().map(|m| m.uid())
    }

    fn host_facts(&self) -> Value {
        json!({
            "macos": run_text("/usr/bin/sw_vers", &["-productVersion"]),
            "macos_build": run_text("/usr/bin/sw_vers", &["-buildVersion"]),
            "arch": std::env::consts::ARCH,
            "model": run_text("/usr/sbin/sysctl", &["-n", "hw.model"]),
            "cpus": run_text("/usr/sbin/sysctl", &["-n", "hw.ncpu"]),
            "memory_bytes": run_text("/usr/sbin/sysctl", &["-n", "hw.memsize"]),
            "kbf_vmm": env!("CARGO_PKG_VERSION"),
        })
    }

    fn signature(&self) -> Result<Signature, String> {
        let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
        let display = Command::new("/usr/bin/codesign")
            .args(["--display", "--verbose=4"])
            .arg(&exe)
            .output()
            .map_err(|e| format!("run codesign: {e}"))?;
        let ents = Command::new("/usr/bin/codesign")
            .args(["--display", "--entitlements", "-", "--xml"])
            .arg(&exe)
            .output()
            .map_err(|e| format!("run codesign: {e}"))?;
        // `--display` writes its report to stderr.
        Ok(signature::parse(
            &String::from_utf8_lossy(&display.stderr),
            &String::from_utf8_lossy(&ents.stdout),
        ))
    }

    fn offline_write(&self, disk: &Path, files: &[OfflineFile]) -> Result<Value, String> {
        offline::write(disk, files)
    }

    fn offline_harvest(&self, disk: &Path, out: &Path) -> Result<Value, String> {
        offline::harvest(disk, out)
    }

    fn process_table(&self) -> Result<String, String> {
        let out = Command::new("/bin/ps")
            .args(["-axo", "pid=,cputime=,comm="])
            .stdin(Stdio::null())
            .output()
            .map_err(|e| format!("run ps: {e}"))?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        } else {
            Err(format!("ps failed ({})", out.status))
        }
    }

    fn fetch(&self, url: &str, dest: &Path) -> Result<(), String> {
        let status = Command::new("/usr/bin/curl")
            .args(crate::image::FETCH_FLAGS)
            .arg("--output")
            .arg(dest)
            .arg(url)
            .stdin(Stdio::null())
            .status()
            .map_err(|e| format!("run curl: {e}"))?;
        if status.success() {
            Ok(())
        } else {
            Err(format!("curl exited with {status}"))
        }
    }

    fn install_image(&self, ipsw: &Path, out: &Path, disk_gib: u64) -> Result<Value, String> {
        vz::install::install(ipsw, out, disk_gib)
    }

    fn run_vm_helper(&self, spec: &VmSpec) -> Result<(), String> {
        vz::helper::run(spec)
    }
}
