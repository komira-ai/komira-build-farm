# Spike: `kbf-vmm probe` on a Mac with virtualization

The gates in [macos-vm-guests.md](../design/macos-vm-guests.md) section 11 (G1, G2, G3,
G5/P5, P0, P2, P6) and the timings in [macos-vms.md](../design/macos-vms.md) need a host
that can run a macOS guest. A GitHub-hosted macOS runner cannot (it is a virtual machine
without nested virtualization), so CI builds, signs and tests what needs no hypervisor and
a person runs the rest on a Mac. Every command prints one JSON object on stdout. Status
`ok` means the probe ran and `details` holds the answer, whatever it is; `needs-manual`
means something the binary cannot create is missing and `reason` says what; `error` means
the probe broke. A `needs-manual` result exits 0, so check the `status` field, not the
exit code.

## Getting the binary

The `vmm-probe` workflow builds `kbf-vmm` and `kbf-guest` for darwin-arm64 from a commit,
signs both with `cargo xtask sign-darwin` (ad hoc, hardened runtime; `kbf-vmm` alone gets
`com.apple.security.virtualization`, from `tools/ci/darwin-entitlements.txt`), and keeps
them as the artifact `vmm-probe-darwin-arm64` for 30 days. It is not attested; check the
SHA-256 in the job summary.

```sh
RUN=$(gh run list -R komira-ai/kbf --workflow vmm-probe.yml --branch <branch> --limit 1 --json databaseId -q '.[0].databaseId')
gh run download "$RUN" -R komira-ai/kbf -n vmm-probe-darwin-arm64 -D vmm
shasum -a 256 vmm/*.tar.gz            # compare with the job summary
tar -xzf vmm/kbf-vmm-*.tar.gz -C vmm && tar -xzf vmm/kbf-guest-*.tar.gz -C vmm
xattr -d com.apple.quarantine vmm/kbf-vmm vmm/kbf-guest 2>/dev/null
codesign -dv --entitlements - vmm/kbf-vmm   # adhoc, runtime, the virtualization key
```

## Running

Use a drained Mac: no other virtual machine running (`pgrep -fl
com.apple.Virtualization.VirtualMachine` prints nothing; quit any other hypervisor
application), nobody logged in at the console for G5, and one APFS volume for the
binaries and the work directory (`clonefile` does not cross volumes) with room for the
base image and a few clones. G5 starts a helper as a throwaway user, so that user must be
able to reach the binary and the work directory: keep both under a world-searchable path
such as `/usr/local`, not under a home directory.

The base image is an `.ipsw` for a macOS release this Mac can run as a guest (Apple's
restore image for the current release, or a file from an image catalog). `--sha256` is
the digest that file must have; take it from the publisher, not from the copy you
downloaded. `--ipsw` is a path or an `https://` URL (plain `http://` is refused). The
file is copied into the new bundle directory while it is hashed, and the installer only
sees that copy.

```sh
D=/usr/local/kbf-vmm; sudo mkdir -p $D && sudo cp vmm/kbf-vmm vmm/kbf-guest $D/
W=/usr/local/kbf-vmm-work; sudo mkdir -p $W
sudo $D/kbf-vmm image base --ipsw <https-url-or-path> --sha256 <hex> --out $W/base --offline-setup | tee $W/image-base.json
for g in g1 g2 g3 p0 p2 p6; do sudo $D/kbf-vmm probe $g --work-dir $W --guest-binary $D/kbf-guest | tee $W/$g.json; done
sudo $D/kbf-vmm probe-bench --work-dir $W --guest-binary $D/kbf-guest --runs 20 | tee $W/bench.json
```

The commands run as root because the offline write attaches a disk image, `probe-bench`
empties the page cache for its cold runs with `purge`, and P0 and G5 start helpers as
another user. A failed `image base` removes the directory it made, so it can be run
again.

`--offline-setup` is probe P0's question applied to the new image: as root it attaches the
disk image, writes the Setup Assistant marker, a mount point and five launch daemons into
the guest's Data volume, and detaches. The daemons are a root daemon that mounts the
automount share itself at `/private/var/kbf-mnt` (so the guest side needs no logged-in
user), a probe as root, a probe as `nobody`, a marker when the guest's launchd runs, a
marker when the login window exists, and a local marker (`com.kbf.probe.local`): `sysctl`
run by launchd with its output going to `/private/var/log/kbf-probe-local` on the guest's
own disk (the boot time and OS version), depending on nothing else, not even a shell. The
Data volume is chosen by what each mounted volume holds (a volume with `private/var/db`
that is not the sealed System volume, which has an empty one too), never by mount order;
if no volume or several qualify the step fails and lists what each volume holds. Every marker JSON records how long the guest waited
for the share (`share_wait_ms`); "boot to system ready" includes that wait. If the image
was built without the step, or the step failed, apply it to the existing base:

```sh
sudo $D/kbf-vmm image setup --base $W/base
```

Without the daemons, or if the guest cannot mount the share or write into it, the probes
that need code running in the guest report `needs-manual` and list the possible causes.

G1 judges a clone booted by the framework's running state held for 20 seconds. With
`--guest-binary` and the daemons in the base it also waits for each clone to write the
system marker and reports how many reached guest userspace; without them the summary says
the evidence is weak.

G5 (a VM start from a launch daemon, nobody logged in, as a throwaway uid, with and
without graphics) answers fully only when `kbf-vmm probe g5` runs as root in launchd's
`System` domain with the console unowned. A shell, `sudo -b` or `nohup` does not qualify
(the report records `launchctl managername` and the parent pid), so it is not in the loop
above. Run it as a one-shot launch daemon:

```sh
cat > $W/g5.plist <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>com.kbf.probe.g5</string>
  <key>ProgramArguments</key><array>
    <string>$D/kbf-vmm</string><string>probe</string><string>g5</string>
    <string>--work-dir</string><string>$W</string>
    <string>--guest-binary</string><string>$D/kbf-guest</string>
  </array>
  <key>RunAtLoad</key><true/>
  <key>StandardOutPath</key><string>$W/g5.json</string>
  <key>StandardErrorPath</key><string>$W/g5.err</string>
</dict></plist>
EOF
sudo cp $W/g5.plist /Library/LaunchDaemons/com.kbf.probe.g5.plist
sudo chown root:wheel /Library/LaunchDaemons/com.kbf.probe.g5.plist
sudo launchctl bootstrap system /Library/LaunchDaemons/com.kbf.probe.g5.plist
# wait until the job has finished and $W/g5.json holds one line
sudo launchctl bootout system/com.kbf.probe.g5
sudo rm /Library/LaunchDaemons/com.kbf.probe.g5.plist
```

## E1: why a booted guest never reaches its userspace

G1 and the other guest gates can say that a clone booted (the framework reported
"running") and that no marker came; they cannot say why. `probe e1` is one run that tells
the causes apart. It starts two clones of the base at once, `e1-x` headless and `e1-y`
with a graphics device (`--graphics`), and for the whole guest wait (320 seconds) records
every helper event with its arrival time, samples every 10 seconds the helper's CPU
time, the CPU time of every `com.apple.Virtualization.VirtualMachine` service process
(the one whose CPU rises belongs to the guest that executes) and the clone's `disk.img`
allocation, and looks for the system marker. A stop or error event ends that arm's wait at
once and is reported. `--keep` leaves both clones and their shares in the work directory.
Then `image harvest` reads the guest's own logs off each kept disk.

Run it as the console user that ran `image base` (a root run or an ssh run has no
session and fails to start a guest), then as root for the setup and the harvest. Each
harvest writes a new directory and refuses one that exists.

```sh
# 1. as root, once: put the current daemons (including the local marker) in the base
sudo $D/kbf-vmm image setup --base $W/base | tee $W/e1-setup.json
# 2. as the console user, from Terminal on the Mac (not sudo, not ssh): about 6 minutes
$D/kbf-vmm probe e1 --work-dir $W --guest-binary $D/kbf-guest --graphics --keep | tee $W/e1.json
# 3. as root: copy the allowlisted files off each kept clone's disk
sudo $D/kbf-vmm image harvest --bundle $W/e1-x --out $W/e1-x-harvest | tee $W/e1-harvest-x.json
sudo $D/kbf-vmm image harvest --bundle $W/e1-y --out $W/e1-y-harvest | tee $W/e1-harvest-y.json
sudo chown -R "$(id -un)" $W/e1-x-harvest $W/e1-y-harvest
```

`image harvest` attaches the disk read-only, picks the Data volume as `image setup` does,
and copies only: `private/var/log/kbf-probe-local`, `private/var/log/com.kbf.probe.*.log`,
and everything under `private/var/db/diagnostics`, `private/var/db/uuidtext`,
`private/var/db/timesync` and `Library/Logs/DiagnosticReports`. A symbolic link, anywhere
on the way or inside a tree, is listed and not followed; a pipe, device or directory where
a file belongs is listed; a file over 256 MiB, a total over 2 GiB, more than 50000 entries
or a tree deeper than 8 levels stops the copy of the rest of that part and is listed. The
JSON lists what was copied (`copied`), what was refused and why (`skipped`), which
allowlisted paths the guest never made (`absent`), and the times of the Setup Assistant
marker and the log directory (`stats`).

To read the guest's log, make an archive from the copied files and ask `log` about the
boot (this assembly has not been tried on a real copy; if `log` refuses the directory,
`log show --file <file>.tracev3 --info --debug` reads the files one at a time):

```sh
A=$W/e1-x.logarchive; mkdir -p $A
cp -R $W/e1-x-harvest/private/var/db/diagnostics/. $A/
cp -R $W/e1-x-harvest/private/var/db/uuidtext/. $A/
cp -R $W/e1-x-harvest/private/var/db/timesync $A/ 2>/dev/null
log show --archive $A --info --debug --predicate 'process == "launchd" OR process == "mount_virtiofs" OR process CONTAINS "kbf" OR eventMessage CONTAINS "AMFI"' | tail -200
```

Read the report (`e1.json`, per arm under `details.arms`) and the harvest together:

| observation | cause |
|---|---|
| In `activity`, `disk_blocks_512_grown` is 0 or stops growing within seconds (`disk_last_growth_ms`), `helper_cpu_ms_gained` and `vm_service_max_cpu_ms_gained_between_samples` are small; or `trace.ended` names a stop or error; and the harvest has no `kbf-probe-local` and no diagnostics newer than the setup | The guest never runs launchd to a steady state: it stalls or powers off early. If only the headless arm is like this and the graphics arm differs, the missing display is the cause |
| CPU and disk writes keep going, `kbf-probe-local` is absent, and the diagnostics show launchd running | launchd did not load our plists: the plists are ignored, or the setup wrote to a volume the guest does not boot from. The log lines name the reason; `stats` shows the marker time |
| `kbf-probe-local` is present, `com.kbf.probe.mount.log` holds a `mount_virtiofs` error, or `com.kbf.probe.system.log` says the binary under `/private/var/kbf-mnt` does not exist | The share never mounted: the hand mount of the automount tag fails |
| `kbf-probe-local` is present, `com.kbf.probe.mount.log` is empty, and `com.kbf.probe.system.log` shows a permission or `Killed: 9` error (or the unified log shows AMFI killing the process) | The guest binary cannot run from the share: `nobody` cannot traverse it, the mount is `noexec`, or code signing kills the binary |
| Everything is present and `guest_marker_ms` is set, or the marker appeared only in the graphics arm | The guest is only slow (beyond 320 seconds), or it needs the graphics device |

`kbf-probe-local` in the manifest's `copied` list has the guest's own modification time
(`mtime_unix`): that is when launchd ran the daemon, to compare with the boot time the file
holds and with the setup marker's time in `stats`. Compare the two arms: a file in one and
not the other points at the graphics device.

## What to send back

Everything the commands wrote, including each helper's log (the cause of a VM that did
not start is in `*.helper.log`; the tail of the log is also in the JSON failure text):

```sh
tar -czf kbf-vmm-results.tgz -C $W *.json *.err *.helper.log e1-x-harvest e1-y-harvest 2>/dev/null
```

(`e1-*-harvest` are the E1 copies; the other names exist after the other commands.) Send
`kbf-vmm-results.tgz`, the output of `sw_vers` and `sysctl hw.model hw.memsize`, and
`shasum -a 256 $D/kbf-vmm`.

## What is not known until a Mac runs it

Nothing here has run against a hypervisor. "Stop" is a force stop: the helper asks the
framework to stop the VM at once, so a stopped guest sees an unclean shutdown. The macOS
code type-checks against `aarch64-apple-darwin`; the parts that call
Virtualization.framework (`src/vz/`), the `hdiutil` steps (`src/offline.rs`) and the
guest-side probes (`kbf-guest probe-guest`) are exercised only by a run on a Mac.
`probe-bench` times an idle host, so its numbers are lower bounds for a host that is
compiling; the guest's marker times are good to about 0.6 seconds (the host polls every
100 ms and the guest every 500 ms).
