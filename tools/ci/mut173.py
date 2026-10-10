#!/usr/bin/env python3
"""Mutants for PR #173: each is an exact replacement that must match once; runs the
given cargo command, records the failing tests, restores the file."""
import subprocess, sys, re, pathlib
MUTANTS = {
 "mH_hello_waits_for_survey": ("crates/kbf-driver-native/src/xcode_watch.rs",
   "let mut last = xcode::not_surveyed(&apps);", "let mut last = xcode::survey(&apps, &probe);"),
 "mR_not_surveyed_reported_ready": ("crates/kbf-driver-native/src/xcode.rs",
   "State::NotSurveyed => XcodeState::NotSurveyed,", "State::NotSurveyed => XcodeState::Ready,"),
 "mP_configured_xcodes_kept_until_survey": ("crates/kbf-driver-native/src/runtime.rs",
   "*self.xcodes.write().unwrap_or_else(PoisonError::into_inner) = ready;",
   "if installed.iter().any(|x| x.state != xcode::State::NotSurveyed) { *self.xcodes.write().unwrap_or_else(PoisonError::into_inner) = ready; }"),
 "mA_not_surveyed_needs_attention": ("crates/kbf-server/src/fleet.rs",
   'if matches!(self.state, "ready" | "not_surveyed") {', 'if self.state == "ready" {'),
 "mC2_showComponent_unsandboxed": ("crates/kbf-driver-native/src/xcode.rs",
   "answer(program, args, &dir, probe.within, sandbox)",
   'answer(program, args, &dir, probe.within, if args.first() == Some(&"-showComponent") { None } else { sandbox })'),
 "mC_find_metal_unsandboxed": ("crates/kbf-driver-native/src/xcode.rs",
   'question(&probe.xcrun, &["--find", "metal"])',
   'answer(&probe.xcrun, &["--find", "metal"], &dir, probe.within, None)'),
}
cmd = sys.argv[1:]
only = [m for m in MUTANTS if not any(a.startswith("--only=") for a in cmd)] 
for a in cmd:
    if a.startswith("--only="): only = a[7:].split(",")
cmd = [a for a in cmd if not a.startswith("--only=")]
for name in only:
    path, old, new = MUTANTS[name]
    p = pathlib.Path(path); text = p.read_text()
    assert text.count(old) == 1, (name, text.count(old))
    p.write_text(text.replace(old, new))
    try:
        out = subprocess.run(cmd, capture_output=True, text=True)
    finally:
        p.write_text(text)
    log = out.stdout + out.stderr
    pathlib.Path(f"mut-{name}.log").write_text(log)
    failed = sorted(set(re.findall(r"^test (\S+) \.\.\. FAILED", log, re.M)))
    compiled = "error[E" not in log and "could not compile" not in log
    print(f"{name} exit={out.returncode} compiled={compiled} red={' '.join(failed) or 'NONE'}", flush=True)
print("MUTANTS-DONE", flush=True)
