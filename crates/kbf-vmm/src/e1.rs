//! E1: why a booted guest never reaches its own userspace.
//!
//! Two clones of the base boot at once, one headless and one (with `--graphics`) with a
//! graphics device. For the whole guest wait the host records every helper event with
//! its time, samples the helper's CPU, the framework's VM service processes and the
//! clone's disk allocation, and watches for the marker a launch daemon of the guest
//! writes into the share. With `--keep` the clones stay, and `image harvest` then reads
//! the guest's own logs off their disks. `docs/spikes/macos-vm-probe.md` maps each
//! outcome to a cause.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::bundle::{self, Identity};
use crate::gates::{Channels, attach_share, base_missing, clean};
use crate::guest::{self, Share};
use crate::probe::{Ctx, Vm};
use crate::report::Report;
use crate::sampler::{self, Sample};
use crate::spec::VmSpec;

/// One guest of the pair.
struct Arm {
    name: String,
    graphics: bool,
    clone: PathBuf,
    share: Share,
    samples: Vec<Sample>,
    marker_ms: Option<u64>,
}

impl Arm {
    fn marker(&self) -> PathBuf {
        self.share.results().join(guest::READY_SYSTEM)
    }
}

/// The names in `dir`, sorted; empty when it cannot be read.
fn names(dir: &Path) -> Vec<String> {
    let mut found: Vec<String> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(Result::ok)
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    found.sort();
    found
}

/// Removes the share directories of `name` (a start that failed leaves them).
fn remove_shares(ctx: &Ctx<'_>, name: &str) {
    for suffix in ["-share", "-tagged"] {
        let _ = std::fs::remove_dir_all(ctx.work.join(format!("{name}{suffix}")));
    }
}

/// Starts the clone `name` with its share and, when `graphics`, a graphics device.
fn start_arm(ctx: &Ctx<'_>, name: &str, graphics: bool) -> Result<(Vm, Arm), String> {
    let share = guest::prepare_share(ctx, name)?;
    let (vm, _) = ctx.start_clone(
        name,
        Identity::Keep,
        |spec: &mut VmSpec| {
            attach_share(spec, &share, Channels::NONE);
            spec.graphics = graphics;
        },
        None,
    )?;
    let clone = ctx.work.join(name);
    Ok((
        vm,
        Arm {
            name: name.to_owned(),
            graphics,
            clone,
            share,
            samples: Vec::new(),
            marker_ms: None,
        },
    ))
}

/// Takes one sample of every guest from one reading of the process table. An unreadable
/// table is returned as an error text and the samples carry the disk only.
fn sample_all(ctx: &Ctx<'_>, vms: &[Vm], arms: &mut [Arm]) -> Option<String> {
    let (table, error) = match ctx.sys.process_table() {
        Ok(t) => (t, None),
        Err(e) => (String::new(), Some(e)),
    };
    for (vm, arm) in vms.iter().zip(arms.iter_mut()) {
        let disk = arm.clone.join(bundle::DISK);
        arm.samples
            .push(Sample::take(&table, vm.proc.pid(), &disk, vm.ms()));
    }
    error
}

/// Watches both guests until each has written its marker or ended, or the guest wait is
/// over; samples on a fixed beat, once at the start and once at the end.
fn observe(ctx: &Ctx<'_>, vms: &mut [Vm], arms: &mut [Arm]) -> Option<String> {
    let began = Instant::now();
    let mut next = Duration::ZERO;
    let mut table_error = None;
    loop {
        if began.elapsed() >= next {
            table_error = sample_all(ctx, vms, arms).or(table_error);
            next += ctx.timing.sample_every;
        }
        for (vm, arm) in vms.iter_mut().zip(arms.iter_mut()) {
            if arm.marker_ms.is_some() {
                continue;
            }
            if vm.ended.is_none() {
                vm.watch(ctx.timing.tick, ctx.timing.tick, false);
            }
            // Existence only, without following a link the guest may have left.
            if std::fs::symlink_metadata(arm.marker()).is_ok() {
                arm.marker_ms = Some(vm.ms());
            }
        }
        let settled = vms
            .iter()
            .zip(arms.iter())
            .all(|(vm, arm)| arm.marker_ms.is_some() || vm.ended.is_some());
        if settled || began.elapsed() >= ctx.timing.guest_timeout {
            break;
        }
    }
    sample_all(ctx, vms, arms).or(table_error)
}

fn arm_json(ctx: &Ctx<'_>, vm: &Vm, arm: &Arm) -> Value {
    json!({
        "name": arm.name,
        "graphics": arm.graphics,
        "clone": arm.clone,
        "share": arm.share.dir,
        "started_ms": vm.started_ms(),
        "stayed_up": vm.started_ms().is_some() && vm.ended.is_none(),
        "guest_marker_ms": arm.marker_ms,
        "trace": vm.trace_json(),
        "samples": arm.samples.iter().map(Sample::json).collect::<Vec<_>>(),
        "activity": sampler::summarize(&arm.samples),
        "results_files": names(&arm.share.results()),
        "helper_log": vm.proc.diagnostics(),
        "harvest": format!(
            "sudo kbf-vmm image harvest --bundle {} --out {}-harvest",
            arm.clone.display(),
            arm.clone.display()
        ),
        "kept": ctx.keep,
    })
}

fn one_line(arm: &Value) -> String {
    format!(
        "{} ({}): started {}, guest marker {}, disk blocks grown {}, ended {}",
        arm["name"].as_str().unwrap_or_default(),
        if arm["graphics"] == json!(true) {
            "graphics"
        } else {
            "headless"
        },
        arm["started_ms"],
        arm["guest_marker_ms"],
        arm["activity"]["disk_blocks_512_grown"],
        arm["trace"]["ended"],
    )
}

/// E1: see the module documentation.
#[must_use]
pub fn e1(ctx: &Ctx<'_>) -> Report {
    if let Some(r) = base_missing(ctx, "e1") {
        return r;
    }
    if ctx.guest_binary.is_none() {
        return Report::needs_manual(
            "e1",
            "e1 looks for the marker a launch daemon of the guest writes, so it needs --guest-binary (a kbf-guest built for macOS) and a base with the probe launch daemons (`image setup`)",
            json!({"guest_binary": null}),
        );
    }
    let mut vms = Vec::new();
    let mut arms = Vec::new();
    for (name, graphics) in [("e1-x", false), ("e1-y", ctx.graphics)] {
        match start_arm(ctx, name, graphics) {
            Ok((vm, arm)) => {
                vms.push(vm);
                arms.push(arm);
            }
            Err(e) => {
                clean(ctx, &mut vms);
                if !ctx.keep {
                    remove_shares(ctx, name);
                }
                return Report::error("e1", e);
            }
        }
    }
    let table_error = observe(ctx, &mut vms, &mut arms);
    let arm_reports: Vec<Value> = vms
        .iter()
        .zip(&arms)
        .map(|(vm, arm)| arm_json(ctx, vm, arm))
        .collect();
    clean(ctx, &mut vms);
    let summary = arm_reports
        .iter()
        .map(one_line)
        .collect::<Vec<_>>()
        .join("; ");
    Report::ok(
        "e1",
        summary,
        json!({
            "arms": arm_reports,
            "process_table_error": table_error,
            "clones_kept": ctx.keep,
            "next": if ctx.keep {
                "run the `harvest` command of each arm as root, then read the results as docs/spikes/macos-vm-probe.md says"
            } else {
                "the clones were removed; run again with --keep to harvest the guests' disks"
            },
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::{FakeSystem, Script, ctx, work_with_base};
    use crate::report::Status;
    use crate::spec::Event;
    use crate::system::Poll;

    fn guest_ctx<'a>(sys: &'a FakeSystem, work: &Path) -> Ctx<'a> {
        let mut c = ctx(sys, work);
        let bin = work.join("kbf-guest-src");
        std::fs::write(&bin, "bin").expect("bin");
        c.guest_binary = Some(bin);
        c
    }

    fn arm(r: &Report, i: usize) -> &Value {
        &r.details["arms"][i]
    }

    /// Catches an E1 that needs nothing it cannot use: without a guest binary the marker
    /// cannot arrive, so it says what to give instead of reporting a silent guest.
    #[test]
    fn without_a_guest_binary_e1_says_what_is_missing_and_starts_nothing() {
        let work = work_with_base("e1-nobin");
        let sys = FakeSystem::new();
        let r = e1(&ctx(&sys, &work));
        assert_eq!(r.status, Status::NeedsManual);
        assert!(r.reason.expect("reason").contains("--guest-binary"));
        assert!(sys.starts.borrow().is_empty());
    }

    /// Catches a pair that is not started together, a graphics flag that reaches the wrong
    /// VM (the first must stay headless), shares that are not attached, and a pair without
    /// `--graphics` that still gets a display.
    #[test]
    fn two_guests_start_together_and_only_the_second_may_have_graphics() {
        let work = work_with_base("e1-start");
        let sys = FakeSystem::new();
        let mut c = guest_ctx(&sys, &work);
        c.graphics = true;
        let r = e1(&c);
        assert_eq!(r.status, Status::Ok, "{r:?}");
        let starts = sys.starts.borrow();
        assert_eq!(starts.len(), 2);
        assert_eq!((starts[0].0.graphics, starts[1].0.graphics), (false, true));
        assert!(starts.iter().all(|(s, _)| s.automount_dir.is_some()));
        assert!(starts.iter().all(|(_, uid)| uid.is_none()));
        assert_eq!(arm(&r, 0)["graphics"], false);
        assert_eq!(arm(&r, 1)["graphics"], true);
        drop(starts);
        sys.starts.borrow_mut().clear();
        c.graphics = false;
        // The first run removed its clones; a new run may use the same names.
        let r = e1(&c);
        assert_eq!(r.status, Status::Ok);
        assert!(sys.starts.borrow().iter().all(|(s, _)| !s.graphics));
    }

    /// Catches a report that drops the helper's events or their times: a silent guest must
    /// still show its started event and the beat of samples, with disk and CPU read.
    #[test]
    fn a_silent_guest_is_reported_with_its_events_samples_and_no_marker() {
        let work = work_with_base("e1-silent");
        let sys = FakeSystem::new();
        *sys.table.borrow_mut() = Ok(
            "1000 0:02.50 /usr/local/kbf-vmm/kbf-vmm\n1001 0:00.10 /usr/local/kbf-vmm/kbf-vmm\n900 0:30.00 /x/com.apple.Virtualization.VirtualMachine\n"
                .to_owned(),
        );
        let r = e1(&guest_ctx(&sys, &work));
        assert_eq!(r.status, Status::Ok);
        let x = arm(&r, 0);
        assert!(x["guest_marker_ms"].is_null());
        assert_eq!(x["started_ms"], 100);
        assert_eq!(x["stayed_up"], true);
        let ev = &x["trace"]["events"][0];
        assert_eq!(ev["event"]["event"], "started");
        assert!(ev["host_ms"].is_u64());
        let samples = x["samples"].as_array().expect("samples");
        assert!(samples.len() >= 2, "a first and a last sample at least");
        assert_eq!(samples[0]["helper_cpu_ms"], 2500);
        assert_eq!(arm(&r, 1)["samples"][0]["helper_cpu_ms"], 100);
        assert_eq!(samples[0]["vm_services"][0]["pid"], 900);
        assert!(samples[0]["disk_blocks_512"].as_u64().is_some());
        assert!(r.details["process_table_error"].is_null());
        let summary = r.summary.expect("summary");
        assert!(summary.contains("e1-x (headless)"), "{summary}");
        assert!(summary.contains("e1-y (headless)"), "{summary}");
    }

    /// Catches a wait that sits out the whole guest timeout after the VM stopped, and
    /// loses the reason: a stop event ends that arm at once, is reported with its time,
    /// and the other arm's marker is still found.
    #[test]
    fn a_stopped_vm_ends_its_wait_and_is_reported_while_the_other_arm_is_judged() {
        let work = work_with_base("e1-stopped");
        let sys = FakeSystem::new();
        sys.script(Script::booted().then(Poll::Event(Event::Stopped { ms: 7 })));
        sys.script(Script::booted().with_guest_file(guest::READY_SYSTEM, "{}"));
        let mut c = guest_ctx(&sys, &work);
        c.timing.guest_timeout = Duration::from_secs(60);
        let began = Instant::now();
        let r = e1(&c);
        assert!(
            began.elapsed() < Duration::from_secs(10),
            "did not wait out the timeout"
        );
        let x = arm(&r, 0);
        assert_eq!(x["stayed_up"], false);
        assert_eq!(x["trace"]["ended"], "the VM stopped");
        assert_eq!(x["trace"]["events"][1]["event"]["event"], "stopped");
        assert!(x["guest_marker_ms"].is_null());
        let y = arm(&r, 1);
        assert!(y["guest_marker_ms"].is_u64());
        assert_eq!(y["results_files"], json!([guest::READY_SYSTEM]));
        assert!(r.summary.expect("summary").contains("the VM stopped"));
    }

    /// Catches an arm that has already written its marker being watched (and its helper
    /// polled) again while the other arm is still silent, and a start that failed being
    /// reported as a VM that stayed up.
    #[test]
    fn a_settled_arm_is_left_alone_while_the_other_is_still_waited_for() {
        let work = work_with_base("e1-one-silent");
        let sys = FakeSystem::new();
        sys.script(Script::booted());
        sys.script(Script::booted().with_guest_file(guest::READY_SYSTEM, "{}"));
        let r = e1(&guest_ctx(&sys, &work));
        assert!(arm(&r, 0)["guest_marker_ms"].is_null());
        assert!(arm(&r, 1)["guest_marker_ms"].is_u64());
        assert_eq!(arm(&r, 0)["stayed_up"], true);
        let work = work_with_base("e1-never-started");
        let sys = FakeSystem::new();
        sys.script(Script::failing("no display", None));
        let r = e1(&guest_ctx(&sys, &work));
        let x = arm(&r, 0);
        assert!(x["started_ms"].is_null());
        assert_eq!(x["stayed_up"], false);
        assert_eq!(x["trace"]["ended"], "no display");
    }

    /// Catches a helper that died without a word being lost: its exit and log tail are the
    /// reason, and the arm is reported not up.
    #[test]
    fn a_helper_that_exits_is_reported_with_its_log() {
        let work = work_with_base("e1-gone");
        let sys = FakeSystem::new();
        sys.script(
            Script::booted()
                .then(Poll::Gone {
                    code: Some(1),
                    signal: None,
                })
                .with_log("framework said no"),
        );
        let r = e1(&guest_ctx(&sys, &work));
        let x = arm(&r, 0);
        assert_eq!(x["stayed_up"], false);
        assert!(
            x["trace"]["ended"]
                .as_str()
                .expect("why")
                .contains("framework said no")
        );
        assert_eq!(x["helper_log"], "framework said no");
    }

    /// Catches `--keep` removing the clones (the harvest would have nothing to read) and
    /// its absence leaving them behind, and a report that does not say which happened.
    #[test]
    fn keep_leaves_the_clones_and_shares_and_without_it_they_are_removed() {
        let work = work_with_base("e1-keep");
        let sys = FakeSystem::new();
        let mut c = guest_ctx(&sys, &work);
        c.keep = true;
        let r = e1(&c);
        for n in ["e1-x", "e1-y"] {
            assert!(work.join(n).join("disk.img").is_file(), "{n}");
            assert!(work.join(format!("{n}-share")).is_dir(), "{n}");
        }
        assert_eq!(r.details["clones_kept"], true);
        assert!(
            arm(&r, 0)["harvest"]
                .as_str()
                .expect("cmd")
                .contains("image harvest --bundle")
        );
        assert!(
            r.details["next"]
                .as_str()
                .expect("next")
                .contains("harvest")
        );
        // The same names again: a kept clone is not overwritten.
        let again = e1(&c);
        assert_eq!(again.status, Status::Error);
        assert!(again.reason.expect("reason").contains("exists"));
        let work = work_with_base("e1-nokeep");
        let sys = FakeSystem::new();
        let r = e1(&guest_ctx(&sys, &work));
        assert!(!work.join("e1-x").exists());
        assert!(!work.join("e1-y-share").exists());
        assert_eq!(r.details["clones_kept"], false);
        assert!(r.details["next"].as_str().expect("next").contains("--keep"));
    }

    /// Catches an unreadable process table failing the whole run, or being silently
    /// ignored: the disk is still sampled and the failure is in the report.
    #[test]
    fn an_unreadable_process_table_is_reported_and_the_disk_is_still_sampled() {
        let work = work_with_base("e1-ps");
        let sys = FakeSystem::new();
        *sys.table.borrow_mut() = Err("ps not found".into());
        let r = e1(&guest_ctx(&sys, &work));
        assert_eq!(r.status, Status::Ok);
        assert_eq!(r.details["process_table_error"], "ps not found");
        let s = &arm(&r, 0)["samples"][0];
        assert!(s["helper_cpu_ms"].is_null());
        assert!(s["disk_blocks_512"].as_u64().is_some());
    }

    /// Catches a start failure that leaves the first guest running or its clone behind,
    /// and one that is not reported as an error.
    #[test]
    fn a_failed_start_stops_what_was_started_and_is_an_error() {
        let work = work_with_base("e1-fail");
        let sys = FakeSystem::new();
        sys.clone_budget.set(Some(1));
        let r = e1(&guest_ctx(&sys, &work));
        assert_eq!(r.status, Status::Error);
        assert!(!work.join("e1-y").exists());
        assert!(
            !work.join("e1-x").exists(),
            "the first clone was cleaned up"
        );
        assert!(
            !work.join("e1-y-share").exists(),
            "so was the failed one's share"
        );
    }
}
