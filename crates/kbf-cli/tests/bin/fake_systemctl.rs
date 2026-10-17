//! `kbf-cli-fake-systemctl`: the stand-in `systemctl` of `tests/frontend_handover.rs`.
//! A test links `<dir>/systemctl` to it; the fake keeps each unit's state and the test's
//! knobs in `<dir>` (found from the path it was run as) and logs every call, one per
//! line, in `<dir>/log`. It answers the two verbs `kbf frontend handover` uses and
//! refuses any other (exit 3), so a stop, a restart or a kill is a failed test.
//!
//! - `show -p ActiveState --value UNIT`: prints `state.UNIT` (`inactive` when absent). A
//!   `looks.UNIT` holding N makes the unit exit after N looks: each look lowers N, and at
//!   0 the state becomes `inactive` first. `show-fails` makes every `show` fail.
//!   `show-fails-after-start` makes every `show` fail once a `start` was made (`started`).
//! - `show -p Result --value UNIT`: prints `result.UNIT` (`success` when absent);
//!   `result-fails` makes it fail.
//! - `start UNIT`: sets `state.UNIT` to `active`, as a `Type=notify` start that was told
//!   `READY=1` ends; `start-fails` makes it fail instead. Either way `started` is made.

use std::fs;
use std::io::Write as _;
use std::path::Path;
use std::process::ExitCode;

fn main() -> ExitCode {
    let mut args = std::env::args_os();
    let me = args.next().unwrap_or_default();
    let dir = Path::new(&me).parent().unwrap_or(Path::new("."));
    let args: Vec<String> = args.map(|a| a.to_string_lossy().into_owned()).collect();
    if let Ok(mut log) = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("log"))
    {
        let _ = writeln!(log, "{}", args.join(" "));
    }
    let knob = |name: &str| dir.join(name).exists();
    match args.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        ["show", "-p", property, "--value", unit] => {
            let failing = knob("show-fails") || (knob("show-fails-after-start") && knob("started"));
            if failing || (property == "Result" && knob("result-fails")) {
                eprintln!("no such unit");
                return ExitCode::FAILURE;
            }
            let state = dir.join(format!("state.{unit}"));
            if property == "Result" {
                let result = fs::read_to_string(dir.join(format!("result.{unit}")));
                println!("{}", result.as_deref().unwrap_or("success").trim());
                return ExitCode::SUCCESS;
            }
            let looks = dir.join(format!("looks.{unit}"));
            if let Ok(left) = fs::read_to_string(&looks) {
                let left: i64 = left.trim().parse().unwrap_or(0);
                if left <= 0 {
                    let _ = fs::remove_file(&looks);
                    let _ = fs::write(&state, "inactive");
                } else {
                    let _ = fs::write(&looks, (left - 1).to_string());
                }
            }
            let state = fs::read_to_string(&state);
            println!("{}", state.as_deref().unwrap_or("inactive").trim());
            ExitCode::SUCCESS
        }
        ["start", unit] => {
            let _ = fs::write(dir.join("started"), "");
            if knob("start-fails") {
                eprintln!("Job for {unit} failed.");
                return ExitCode::FAILURE;
            }
            let _ = fs::write(dir.join(format!("state.{unit}")), "active");
            ExitCode::SUCCESS
        }
        _ => {
            eprintln!("unexpected call: {}", args.join(" "));
            ExitCode::from(3)
        }
    }
}
