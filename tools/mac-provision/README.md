# kbf-mac-provision

`kbf-mac-provision` applies and checks the profile of a Mac node: the host settings and
the worker profile that
[docs/design/mac-node-provisioning.md](../../docs/design/mac-node-provisioning.md)
(sections 2, 3 and 5) describes. It is one POSIX `sh` script that runs only programs
macOS ships, over a profile of `KEY=VALUE` lines that it parses and never sources.

```text
kbf-mac-provision check   --profile FILE [--keys K,K]   # one PASS/FAIL line per item; exit 1 on any FAIL
kbf-mac-provision apply   --profile FILE [--keys K,K]   # root: fix each FAIL, one CHANGE line per change
kbf-mac-provision print   --profile FILE                # the resolved profile, its SHA-256, the node label
kbf-mac-provision restart --profile FILE                # root: restart kbf-daemon, truncating its error log
```

- `apply` fixes each failing item, then checks again and prints only what still fails.
  A second `apply` prints nothing. Exit status: 0 when every item passes, 1 when one
  fails, 2 for bad usage or a refused profile.
- **Check only** items are never changed by `apply`; it reports them and exits 1. An
  erase or a restore is the remedy for macOS and for simulator runtimes; FileVault and
  the administrator are the operator's.
- The node label is `profile=<profile>@<first 12 hex digits of the profile's SHA-256>`,
  passed to `kbf-daemon` as a `--label`.
- `--keys` limits `check` and `apply` to the named items. `--root DIR` runs everything
  under DIR; only the tests use it.
- An example profile, with placeholder values, is [example.profile](example.profile).

## Items

Every item is named by its profile key. `xcode`, `probe` and `expect_probe` may repeat;
every other key appears exactly once, and an unknown key is refused. A key is lower-case
letters and `_` only.

| Item (keys) | Values | Read with | Applied with |
|---|---|---|---|
| `macos_version`, `macos_build` | version, build | `sw_vers` | check only |
| `hostname` | `<farm>-mac-<nn>`, lower case, at most 63 characters | `scutil --get` `HostName`, `LocalHostName`, `ComputerName` | `scutil --set` each that differs |
| `admin_user` | account | `dscl`: it exists, and it and `root` are the only administrators: the names in `admin`'s `GroupMembership`, and every account in `dscl . -list /Users` that `dseditgroup -o checkmember` counts as a member of `admin` (which finds one by primary group 80 or a nested group) | check only |
| `role_user` (`role_id`) | `_name`, id 200-499 | `dscl`: user and group with that id, shell `/usr/bin/false`, home `/var/empty`, hidden, no password (`Password` is `*` and there is no `AuthenticationAuthority`); no other user and no other group holds the id | `dscl . -create`, and `-delete` of `AuthenticationAuthority` (refused if the id is taken) |
| `power_sleep`, `power_standby`, `power_powernap`, `power_autorestart`, `power_womp` | numbers | `pmset -g` | `pmset -a` |
| `updates_auto_download`, `updates_auto_install_macos`, `updates_auto_install_security`, `updates_config_data`, `appstore_auto_update` | 0 or 1 | `defaults read /Library/Preferences/com.apple.SoftwareUpdate` `AutomaticDownload`, `AutomaticallyInstallMacOSUpdates`, `CriticalUpdateInstall`, `ConfigDataInstall`, `AutomaticallyInstallAppUpdates` | `defaults write ... -bool`. **Verify on the pinned version:** the SoftwareUpdate payload is removed in macOS 27, and whether these local preferences are still honoured is not known; every line says so |
| `remote_login` | 0 or 1 | `systemsetup -getremotelogin` | `systemsetup -f -setremotelogin` |
| `ssh_password_auth` | 0 or 1 | `/etc/ssh/sshd_config.d/000-kbf.conf` holds `PasswordAuthentication`, `KbdInteractiveAuthentication` and `PermitRootLogin no`, root-owned, mode 644; `sshd_config` includes the directory. What sshd uses: no file in `sshd_config.d` sorts before the drop-in, `sshd_config` sets none of `PasswordAuthentication`, `KbdInteractiveAuthentication`, `ChallengeResponseAuthentication`, `PermitRootLogin` before its `Include` line, and, as root, `sshd -T` reports the drop-in's three values (as another user the line says `sshd -T` was not run) | writes the drop-in (`000-` sorts first, and sshd keeps the first value it reads). It never removes an earlier drop-in or edits `sshd_config`: those stay FAIL for the operator |
| `firewall` | on, off | `socketfilterfw --getglobalstate` | `socketfilterfw --setglobalstate` |
| `filevault` | on, off | `fdesetup status` | check only: never toggled |
| `autologin` (`lease_uids`) | off; range, default in the design 600-699 | `autoLoginUser` in `/Library/Preferences/com.apple.loginwindow`, `/etc/kcpassword` | passes when auto-login is off and no `kcpassword` is left, or when the user is a `kbf-lease-*` account whose uid is in `lease_uids` (set by `kbf-mac-session` for a `whole_machine` lease; `apply` leaves it). Otherwise `apply` clears both. Nothing records a lease yet, so a lease user left behind also passes ([#132](https://github.com/komira-ai/komira-build-farm/issues/132)) |
| `time_server` | host | `systemsetup -getusingnetworktime`, `-getnetworktimeserver` | `systemsetup -setusingnetworktime on`, `-setnetworktimeserver` |
| `time_max_offset_ms` | ms | `sntp -t 5` (read-only) | `sntp -sS` |
| `xcode` (`xcode_xip_dir`) | `BUILD:/Applications/Xcode*.app/Contents/Developer:SHA256` | `ProductBuildVersion` of each app's `version.plist`; any other `Xcode*.app` directory in `/Applications` is drift (links are skipped) | installs a missing or wrong Xcode from the `.xip` in `xcode_xip_dir` whose SHA-256 the line names (`xip --expand`, its build checked, `xcodebuild -license accept`, `-runFirstLaunch`); removes unpinned Xcodes |
| `default_developer_dir` | a pinned Xcode's path | `xcode-select -p` | `xcode-select -s` |
| `probe` (`expect_probe`) | `KEY:/PATH:SHA256`; `KEY:XCODE_BUILD=VALUE` | each probe script's SHA-256 | check only. `expect_probe` lines are validated (a known probe, a pinned Xcode, once each) and printed |
| `simulator_runtimes` | none | `/Library/Developer/CoreSimulator`: `Images/*.dmg`, `Profiles/Runtimes/*.simruntime`, `Volumes/*` | check only: runtimes live in VM images |
| `launchd_label` (`kbf_server`, `kbf_cas`, `kbf_ca_cert`, `kbf_cert`, `kbf_key`, `kbf_scratch`, `kbf_labels`, `hostname`, `role_user`) | label | `/Library/LaunchDaemons/<label>.plist` equals the one the profile renders (section 5.3: `UserName`/`GroupName` the role account, `ProcessType` `Interactive`, `ExitTimeOut` 30, the daemon's flags), root-owned, mode 644; `/Library/Logs/kbf` root's, mode 755; the error log in it a regular file with one link, the role account's, mode 644; the job loaded | makes the directory root's, replaces anything else at the error log's path (unlinked, never followed) with a new file of the role account, writes the plist, then restarts the job: `launchctl bootout`, the error log truncated while it is stopped, `launchctl bootstrap`. `restart` refuses, before it stops anything, when the error log is not such a file, so a link the role account plants is never emptied as root |

## Tests

| What | Where it runs | What it proves |
|---|---|---|
| [test/run.sh](test/run.sh) | CI job `mac-provision` (Linux) | Every item against a fake Mac ([test/fakecmd](test/fakecmd) stands in for each macOS program and keeps its state in files): a fresh Mac fails check on each item and passes after one apply; a second apply prints nothing and changes nothing; on a converged Mac each drift fails exactly its item, and apply fixes it or (check only) changes nothing; refused profiles; root; `--keys`; restart; a symbolic or hard link planted at the error log (never emptied); `sshd -T` skipped when not root; the Xcode sources; the lease auto-login rule |
| [test/coverage.sh](test/coverage.sh) | `mac-provision` | Runs `run.sh` with the script under `bash -x` and measures line and branch coverage from the trace ([test/coverage.awk](test/coverage.awk) says how); fails below 100% |
| [test/mutants.sh](test/mutants.sh) | `mac-provision` | Plants each listed defect in a copy of the script and fails unless the named tests go red |
| [test/hosted-macos.sh](test/hosted-macos.sh) | CI job `native-macos` (a hosted macOS VM, as root) | The same script on real macOS for what a VM can prove; its header lists what is applied, asserted, and only read |

What only a real node of the pinned version can prove (design section 10): `pmset`
on hardware, FileVault, Remote Login, the time server, whether the update preferences
are honoured, an Xcode install from a real `.xip`, and whether a freshly erased Mac
lists `_mbsetupuser` as an administrator. The hosted VM does, so `admin_user` fails
there; the script allows no exception today, and if real nodes list it too, removing
it or allowing it is a decision to make explicitly.
