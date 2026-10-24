# kbf-host-install: the root side of a host-driven update

`kbf-host-install` is the program a host runs, as root, when the updater asks it to install
a version of the `kbf` package. It is not a command anyone types: sshd runs it as the forced
command of one key, and it does only what the allowlist on that host permits.

## Getting the binary

A release carries `kbf-host-install` as a tarball per platform, built, signed and attested by
the same run as every other asset ([../artifacts.md](../artifacts.md)):

| Host | Asset |
|---|---|
| Linux, x86_64 | `kbf-host-install-<commit>-linux-x86_64.tar.gz` |
| Linux, arm64 | `kbf-host-install-<commit>-linux-arm64.tar.gz` |
| macOS, Apple silicon | `kbf-host-install-<commit>-darwin-arm64.tar.gz` (ad hoc signed with the hardened runtime) |

No package ships it: neither the `kbf` nor the `kbf-installer` deb installs it, so a Linux
host gets it from the tarball too. Each tarball holds one member, `kbf-host-install`. Its
`<commit>` is the commit of the release the host is on or going to. The updater,
`kbf-updater`, is the same kind of asset for the operator's Linux box and for a Mac's host agent
(`kbf-updater-<commit>-linux-<arch>.tar.gz` and `kbf-updater-<commit>-darwin-arm64.tar.gz`, one
member `kbf-updater`; the Mac one is ad hoc signed with the hardened runtime).

Fetch the tarball with its bundle and check the bundle before the binary is placed anywhere
(the flags are those of [../artifacts.md](../artifacts.md#provenance-bundles-verifying-a-tarball-offline)):

```text
gh release download <tag> --repo komira-ai/kbf \
  --pattern 'kbf-host-install-<commit>-<platform>.tar.gz*' --pattern SHA256SUMS --dir dl
gh attestation trusted-root > dl/trusted_root.jsonl
cd dl && sha256sum --check --strict --ignore-missing SHA256SUMS      # shasum -a 256 -c on a Mac
gh attestation verify kbf-host-install-<commit>-<platform>.tar.gz \
  --bundle kbf-host-install-<commit>-<platform>.tar.gz.sigstore.json \
  --custom-trusted-root trusted_root.jsonl --repo komira-ai/kbf \
  --signer-workflow komira-ai/kbf/.github/workflows/artifacts.yml \
  --cert-oidc-issuer https://token.actions.githubusercontent.com \
  --source-ref refs/heads/main --source-digest <commit> \
  --predicate-type https://slsa.dev/provenance/v1 --deny-self-hosted-runners
tar -xzf kbf-host-install-<commit>-<platform>.tar.gz kbf-host-install
```

Place the binary at the path the key or the Mac section below names, as root, in directories
only root can write. `kbf-host-install --version` prints `kbf-host-install <package version>`
and does nothing else: it is the one argument the program takes outside the Mac's two
internal ones, and what the release uses to check the signed Mac binary runs.

## The key

The updater's ssh key is installed in root's `authorized_keys` as

```text
from="203.0.113.10",restrict,command="/usr/libexec/kbf/kbf-host-install" ssh-ed25519 AAAA...
```

`from=` is the address the updater connects from. `restrict` turns off forwarding, a
terminal and the rest. `command=` runs this program whatever the client asked for; the
client's request arrives only in `SSH_ORIGINAL_COMMAND`. The program reads nothing from its
arguments or standard input (bar `--version`, above).

One more entry point exists, and it is not reachable over ssh: an install runs this same
program again inside a systemd unit (see below), with the request in the variable
`KBF_HOST_INSTALL_UNIT`, which only `systemd-run` sets. The variable counts only when none of
`SSH_ORIGINAL_COMMAND`, `SSH_CONNECTION`, `SSH_CLIENT` and `SSH_TTY` is set; in an ssh session it
is ignored, and the program handles `SSH_ORIGINAL_COMMAND` as usual.

## The three requests

The whole string must be one of these, with single spaces and nothing before or after:

```text
install kbf=<version> from <channel>
restart frontend=<name>
query
```

- `<version>` is `[0-9][0-9A-Za-z.+~-]*`, at most 128 bytes, without an epoch (no `:`), and a
  version `dpkg` itself accepts (so `1.0-` is refused).
- `<channel>` is `[a-z0-9][a-z0-9-]{0,31}`.
- `<name>` is `-`, which names the unit `kbf-frontend.service`, or an instance name
  `[a-z][a-z0-9-]{0,31}`, which names `kbf-frontend@<name>.service`. The unit string is built
  from the checked name alone; no other unit can be named, and nothing is expanded.
- The package is always `kbf`.

Anything else is refused: exit status 2, and no process is started.

## The allowlist

`/etc/kbf/host-install.toml` names the channels an install may use. It must be owned by
root and not writable by its group or others, or the program refuses to run. So must every
directory above it, up to `/` (whoever can write a directory can replace what is in it); the
same holds for each source file and each key file below.

```toml
[channels.candidate]
suite = "candidate"
source_list = "/etc/kbf/host-install/candidate.sources"

[channels.stable]
suite = "stable"
source_list = "/etc/kbf/host-install/stable.sources"
```

`source_list` is the one apt source the install may read: a deb822 file (`Types:`,
`Signed-By:`) or a one-line apt list (`deb [signed-by=/path] ...`). It must be owned by
root and not writable by others, and every entry in it must name `Signed-By` as absolute
paths of key files that are themselves owned by root and not writable by others. A source
without `signed-by`, with a key given as a fingerprint or inline, or with a key a user could
replace, is refused (`channel_unsigned`, `key_untrusted`).

A source that names a key and still weakens the check is refused too (`channel_insecure`):
the options `trusted`, `allow-insecure`, `allow-weak` and `allow-downgrade-to-insecure` of the
one-line form, and the deb822 fields `Trusted`, `Allow-Insecure`, `Allow-Weak` and
`Allow-Downgrade-To-Insecure`, in any case, with any value but a plain no (`no`, `false`,
`off`, `without`, `disable`, `0`). A directory above one of these files that its group or
others can write is `config_unsafe` (`key_untrusted` for a key).

The path of this file is a constant of the program. No flag and no environment variable
changes it.

## What an install does

The process sshd starts checks the request, then hands the work to one transient systemd unit
and relays its answer. The work is in one unit, run by one process that holds the install
lock for all of it, so a dropped ssh connection stops neither `dpkg` nor the lock, and there is
no moment between the two apt steps at which an install looks idle.

The process sshd starts:

1. Reads the allowlist and the channel's source and checks their signing (above); refuses
   if any check fails. Opens the audit log; fails (`audit_unavailable`) if it cannot.
2. Probes `/run/kbf-host-install.lock` without taking it; if another install holds it, or it
   cannot be asked about, answers `failed` with reason `busy` and starts nothing. It never
   holds the lock itself.
3. Runs `systemd-run --wait --collect --pipe --quiet --unit=kbf-host-install-<version>.service
   --property=RuntimeMaxSec=1800 --setenv=KBF_HOST_INSTALL_UNIT=<the request> --
   /usr/libexec/kbf/kbf-host-install`, and relays the last line the unit prints as its own
   answer, with the unit's stderr as the detail of a failure. A unit that gives no answer (it
   could not start, was killed, printed something else) is `install_failed`; an answer for a
   version other than the one asked for is `verify_failed`.

The unit (this program again, as root, with the fixed environment) does the install:

0. Reads the allowlist and the source again, takes a non-blocking `flock` on
   `/run/kbf-host-install.lock` and keeps it until it exits; if another install holds it,
   answers `failed` with reason `busy` and runs nothing.
1. Reads the installed version with `dpkg-query`. The same version is a no-op that reports
   `already_installed`; any other version, older or newer, is installed. A rollback is an
   install of an older version, and nothing here refuses it. A package dpkg flags with an
   error (the third character of its status, `R` for reinstall required) is not cleanly
   installed: an install of any version goes ahead, the same one included (apt repairs it).
2. Runs `apt-get update` restricted to the channel's source file
   (`-o Dir::Etc::SourceList=<file> -o Dir::Etc::SourceParts=-`), then
   `apt-get install -y --no-install-recommends --allow-downgrades -o Dpkg::Options::=--force-confold
   -o DPkg::Lock::Timeout=300 -t <suite> kbf=<version>` with the same restriction.
3. Reads the installed version again and reports `installed` only if it is the target and
   the package is cleanly installed.

No shell runs anywhere: every process is started with an explicit argument vector and an
environment of `PATH`, `LANG=C` and `DEBIAN_FRONTEND=noninteractive` alone.

## What a restart does

A package install restarts no `kbf` unit, so a client that has installed a new version asks for
the restart of the one frontend unit it rolls, after the install. Linux only: a Mac refuses
the request as `bad_command` (it has no such unit).

The process sshd starts does all of it, and holds the install lock (`flock` on
`/run/kbf-host-install.lock`) from before the first command until it answers, so while it runs a
restart and an install do not overlap: an install that finds the lock held is `busy`, and so is a
restart.

1. Opens the audit log; fails (`audit_unavailable`) if it cannot, and runs nothing.
2. Takes the lock without waiting; if another install or restart holds it (or it cannot be
   opened), answers `failed` with reason `busy` (`lock_unavailable`) and runs nothing.
3. Runs `systemctl restart -- <unit>` (killed after 120 seconds). A start failure or an exit
   status but 0 is `failed` with reason `restart_failed`, and the detail on stderr holds the
   exit status and what `systemctl` wrote.
4. Runs `systemctl is-active -- <unit>` (each look killed after 10 seconds) every half second
   until it exits 0, which it does for the states `active` and `reloading` (`activating` does
   not count).
   A unit that is not active 120 seconds after the restart returned is `failed` with reason
   `restart_timeout`. Otherwise the answer is `restarted`, naming the unit.

The restart does not read the channel allowlist. Nothing in the program notices a dropped ssh
connection: it goes on with the restart and the wait (at most about 250 seconds in all),
holds the lock until then, and its answer is lost, so a restart asked for again in that time is
`busy`. If something outside ends the process, the lock goes with it and systemd carries on with
the restart. Either way the client asks the host again after that bound.

## The answer

One JSON line on stdout.

| request | line | exit |
|---|---|---|
| install, done | `{"result":"installed","installed_version":"...","unit_state":"finished"}` | 0 |
| install, same version | `{"result":"already_installed","installed_version":"...","unit_state":"not_run"}` | 0 |
| restart, done | `{"result":"restarted","unit":"kbf-frontend@<name>.service"}` (`kbf-frontend.service` for `-`) | 0 |
| query | `{"result":"query","installed_version":"..."\|null,"running_commit":"..."\|null,"node_id":"..."\|null,"in_progress":true\|false}` | 0 |
| refused | `{"result":"refused","reason":"<code>"}` | 2 |
| failed | `{"result":"failed","reason":"<code>"}`, detail on stderr | 3 |

Refusal codes: `bad_command`, `channel_not_allowed`, `config_invalid`, `config_unsafe`,
`channel_unsigned`, `key_untrusted`, `channel_insecure`, and, on a Mac, for a channel whose source
is GitHub, `unknown_release`, `ambiguous_release` and `fetch_failed`. Failure codes:
`audit_unavailable`, `query_failed`, `installed_version_invalid`, `package_state_unclean`,
`update_failed`, `install_failed`, `verify_failed`, `busy`, `lock_unavailable`, and for a restart
`restart_failed` and `restart_timeout`.

Every line has a `result`: `installed`, `already_installed`, `restarted`, `query`, `refused` or
`failed`.

`installed_version` of a query is the version dpkg reports when `kbf` is cleanly installed,
and null when it is not installed or dpkg flags an error on the package (the updater then
installs, which repairs it). `running_commit` is the commit `kbf --version --json` of the
installed binary reports, or null (it is the installed binary's, not the running process's: a daemon that was not restarted after an upgrade still runs the older one, which the Mac side avoids by reading the process); the binary is killed after 10 seconds, and a binary that
fails, hangs or prints something else has none. `node_id` is the id of the node: `[node] id`
of `/etc/kbf/kbf.toml`, else its `[identity] node-id`, else the short host name in lower case
(the id an enrolling node takes when its configuration names none). It is null when the file
is missing or is not root's alone to write, or when the value is not a DNS label. A client that
acts on one host among several compares it with the id it expects before it does anything.

`in_progress` is true when any of three things holds, and false only when none does: the
install lock `/run/kbf-host-install.lock` is held (probed, then let go at once; the process
that does the work holds it from before it reads the package database until it ends, so the
gap between the two apt steps is covered; a running restart holds it too, so `in_progress` is
true during one); a `kbf-host-install-*` unit is listed by
`systemctl list-units` in the state `active`, `activating`, `deactivating` or `reloading`; or
some process holds the dpkg frontend lock (read from `/proc/locks`; nothing is locked or
waited for). If the listing itself fails, the query fails (`query_failed`) and does not
guess.

## The audit log

Every invocation, refused ones included, appends one JSON line to
`/var/log/kbf-host-install.log` (created root-owned, mode 0600, never through a symbolic
link): the time, the verb (`install`, `restart` or `query`), the version, the channel, the
outcome and the reason, and, on the line of a restart, the `unit`. On a Mac the
apply job appends one more line when it ends: verb `apply`, the version, outcome `applied` or
`failed`, and `result`, the status line (`done`, `failed:not_welcomed:rolled_back`, ...). A request
that was not understood is recorded escaped and cut at 200 characters. An install or a restart
is not started when the log cannot be opened.

## On a Mac

A Mac takes the same `install kbf=<version> from <channel>` and `query` requests (a `restart`
is refused as `bad_command`) and answers in the same shape, but it installs the attested release tarball
`kbf-<commit>-darwin-arm64.tar.gz` (not a package), into the kit layout, and restarts the
daemon's launchd job on it. The program is the same binary; the platform picks the Mac side.
Nothing here is a Homebrew formula or a `kbf-installer` order.

### The layout it works in

```text
/usr/local/kbf/releases/<commit>/kbf        the release (root:wheel, 0755), and beside it
/usr/local/kbf/releases/<commit>/VERSION    the version text it was installed as
/usr/local/kbf/current   -> releases/<commit>   relative links, moved by atomic rename
/usr/local/kbf/previous  -> releases/<commit>
/usr/local/kbf/etc/av2_mac.conf             NODE_ID=<id> (the node's id)
/usr/local/kbf/log/kbf-daemon.err.log       where launchd writes the daemon's stderr
/Library/LaunchDaemons/com.komira.kbf-daemon.plist    the daemon's job, run as _kbf
```

The error-log path is the `StandardErrorPath` of the kit's daemon plist. The `/Library/Logs/kbf`
paths of `kbf-mac-provision` belong to that tool's own layout; this program does not read them
and this document does not describe them.

`<version>` is the version text of a build, `0.1.<seq>+g<12 hex digits>-<run>`. Its 12 digits
pick the one tarball `kbf-<40 hex digits>-darwin-arm64.tar.gz` whose commit starts with them:
among the file names of the channel's `dir`, or, when the channel pins its releases with
`sums`, among the lines of its `SHA256SUMS`, or, for a channel whose source is GitHub, among the
assets of the repository's releases. No tarball is an unknown release and two are ambiguous
(`verify_failed`, with the reason on stderr; a channel on GitHub refuses with `unknown_release`
and `ambiguous_release`). The version text is kept in `VERSION`,
and `query` reports it as `installed_version`.

### The files, once per Mac (the operator's one root session)

Everything below is done once, as an administrator with `sudo`; this program does none of it.

1. The binary is the `kbf-host-install` of the verified darwin-arm64 release tarball ([Getting the binary](#getting-the-binary)), placed at `/usr/local/libexec/kbf/kbf-host-install`, `root:wheel` 0755, in directories
   (`/usr/local`, `libexec`, `kbf`) that group and others cannot write. An install checks this
   before it does anything else and refuses with `config_unsafe` when the program or one of
   those directories is not root's or is writable by group or others (some Intel Homebrew
   setups leave `/usr/local` group-writable).
2. An ordinary account for the updater's ssh login. It is not root and not an administrator;
   the examples call it `kbfupdate`. Remote Login is on, and its
   `~kbfupdate/.ssh/authorized_keys` has exactly one line:

   ```text
   from="203.0.113.10",restrict,command="/usr/local/libexec/kbf/kbf-host-install" ssh-ed25519 AAAA...
   ```

3. The single sudoers rule, in `/etc/sudoers.d/kbf-host-install` (`root:wheel` 0440, checked
   with `visudo -cf`):

   ```text
   kbfupdate ALL=(root) NOPASSWD: /usr/local/libexec/kbf/kbf-host-install --as-root
   ```

   It allows one program with one argument, to one account. The program then reads the
   request from its standard input and checks it against the grammar again, so the rule does
   not widen what the key can ask for. The sudoers entry, not the ssh key, is what bounds
   `kbfupdate`; a `Defaults:kbfupdate !setenv, env_reset` line keeps its environment out of the
   program.
4. `/usr/local/etc/kbf/host-install.toml`, root's alone to write:

   ```toml
   [channels.stable]
   dir = "/usr/local/kbf-host-install/stable"
   sums = "/usr/local/etc/kbf/host-install/stable.SHA256SUMS"   # optional
   source_ref = "refs/heads/main"      # optional; this is the default

   [channels.online]                   # a channel on GitHub instead of a directory
   source = "github"
   token_file = "/usr/local/etc/kbf/host-install/github.token"   # optional: a higher rate limit; see below
   ```

   A channel takes its releases from a `dir` or from `source = "github"`, never both (the
   allowlist is `config_invalid` when it names both or neither). `sums` and `source_ref` go with
   either. A channel on GitHub is described in [its own section](#a-channel-on-github).

   The trust anchor is the build attestation, which every release must pass (check 4 below).
   `sums` is optional. When it is set it is an extra freeze list: the pinned `SHA256SUMS` of the
   releases the channel offers, root's alone to write, and a release it does not list is
   refused, and a tarball is taken only when its sha256 is listed there. When it is absent the
   attestation alone decides. `dir` is where the release files
   are put: `kbf-<commit>-darwin-arm64.tar.gz` and `kbf-<commit>-darwin-arm64.tar.gz.sigstore.json`.
   Whoever puts them there is not trusted; they are copied once and checked. Putting them there
   is not done by this program.
5. The pinned `gh` and the Sigstore trusted root that the `kbf-installer` package installs
   (`/usr/local/kbf-installer/trust/gh` and `trusted_root.jsonl`), root-owned as that package
   leaves them.

The first install into a Mac that the kit set up needs nothing else: the kit's links, plist and
`av2_mac.conf` are what this program reads.

### The three entries

| process | started by | does |
|---|---|---|
| `kbf-host-install` as `kbfupdate` | sshd (the forced command) | passes the request, unchanged, to the root process through `sudo -n ... --as-root` and prints its line and exit status |
| `kbf-host-install --as-root` as root | that sudo rule | reads the request from standard input; checks it, audits it, answers; for an install, checks the release and launches the apply job |
| `kbf-host-install --apply <stage>` as root | launchd, as the one-shot job `com.komira.kbf-host-install-apply` | lays the release out, moves the links, restarts the daemon, records the result |

A login that is already root (the key is root's) handles the request in its own process and
needs no sudo. `--as-root` and `--apply` are refused for any other user, and `--apply` for any
stage but the program's own.

### What an install checks first

Before anything on the Mac changes, in this order (a failure changes nothing and removes the
stage):

1. the request, the channel and its pinned sums (when it has any), and that `gh` and the trusted
   root are root's;
2. the files of the channel's `dir` (or, for a channel on GitHub, the two files fetched as
   `nobody` into a directory it owns inside the stage) are copied once, into `/var/db/kbf-host-install/stage`
   (root's, mode 0700), without following links, as regular files with one link and under a size
   limit; only that copy is checked from here on;
3. when the channel has sums, the sha256 of the tarball is the one they list (a channel without
   sums skips this);
4. `gh attestation verify` accepts the tarball and its bundle, offline, run as `nobody` and
   never as root. This is the trust anchor, and it binds: the tarball's sha256 as a subject
   (`gh` hashes the staged copy), the build's source digest to the full 40-digit commit (not the
   version's 12 digits), its source ref to the channel's branch, the signer to
   `komira-ai/kbf`'s `artifacts.yml` workflow (the repository's former name is still accepted,
   for releases attested under it), and GitHub-hosted runners only;
5. the tarball holds exactly one regular file named `kbf`, a Mach-O file for arm64;
6. the binary carries no `com.apple.quarantine` attribute (it is removed, and proven gone);
7. `codesign --verify --strict` accepts it and its signature has the hardened runtime;
8. the new binary, run as `_kbf`, accepts the daemon's live configuration
   (`kbf --mode=daemon --config /usr/local/etc/kbf/kbf.toml --check`);
9. the staged binary is root's, in directories only root can write.

Steps 4 and 8 start their program through `/usr/sbin/chroot -u <uid> -g <gid> -G <gid> / ...`:
started by `Command::uid` and `gid` alone, a child on macOS keeps the old effective gid (group 0,
`wheel`) as a supplementary group, and `chroot` sets the one group itself. The real-Mac test
checks that the child's groups are its own gid and nothing else.

The stage is opened to traverse-only (0711) for steps 4 and 8, which run as other users, and
is 0700 otherwise. While it is 0711 the users `nobody` and `_kbf` can pass through it but
cannot list it or write to it. Every file in it is root's and not writable by them except
`gh-home`, the directory of `gh`'s own home, which belongs to `nobody`; the files are public
release bytes. It is closed to 0700 again before `plan.json` is written. The same version already installed and running is `already_installed`; an
older version is installed like a newer one.

### The apply

The install verb returns once it has launched the apply job, so a dropped ssh session cannot
end the work between `launchctl bootout` and `launchctl bootstrap`. It answers

```json
{"result":"apply_started","version":"...","commit":"...","apply":"running"}
```

and exits 0. The job:

0. starts from the links the install saw: `current` and `previous` as they were when the install
   was accepted are in `plan.json` (so the job, a rollback, and whoever has to repair a job that
   was killed halfway work from the same facts; the next install repairs the kit too);
1. lays out `releases/<commit>/kbf` and `VERSION` (a release already there must be the same
   bytes), after checking that the file and every directory above it up to `/usr/local/kbf` are
   root's and closed to others, again without a quarantine attribute;
2. moves `previous` to the release that ran and `current` to the new one, each by renaming a
   link made aside;
3. runs `launchctl bootout system/com.komira.kbf-daemon`, waits (at most 60 seconds) until
   `launchctl print` no longer finds the service (bootout returns before the service is gone, and
   a bootstrap in that gap fails with an input/output error), then
   `launchctl bootstrap system /Library/LaunchDaemons/com.komira.kbf-daemon.plist`;
4. waits (at most 180 seconds) for the `welcomed` line of the NEW process in the daemon's error
   log, counting only what the log gained since before the bootstrap, and then requires (for at
   most 60 seconds) that the daemon's process runs the release that was started: the same reading
   `query` reports as `running_commit`;
5. if step 3 or 4 fails, moves both links back, checks that the old release's file is root's,
   restarts it the same way (with the same two checks) and records what became of it;
6. removes the stage, then writes the result line, so that the status leaves `running` only when
   nothing of the job is left to remove; and appends the outcome to the audit log.

Nothing here waits for leases: the drain that keeps builds from being cut is the updater's, done
before it asks for the install. `bootout` is synchronous and launchd kills the daemon after its
exit timeout.

The result is one line in `/var/db/kbf-host-install/status` and the `apply` field of `query`:

| `apply` | meaning |
|---|---|
| `null` | no apply has been recorded |
| `running` | the job is running (`in_progress` is true) |
| `done` | the new daemon logged `welcomed`; `running_commit` is the new commit |
| `failed:<reason>` | before anything restarted: `plan_invalid`, `stage_untrusted`, `stage_changed`, `release_differs`, `quarantine`, `layout_failed`, `release_untrusted`, `links_failed`, `launch_failed`; and `apply_lost` for a `running` status whose job is gone |
| `failed:<reason>:rolled_back` | the restart failed (`bootout_timeout`, `bootstrap_failed`, `not_welcomed`, `wrong_release`) and the previous release is running again |
| `failed:<reason>:rollback_failed` | the previous release did not come up either |
| `failed:<reason>:no_previous` | there was no release before this one to go back to |

A reader polls `query` until `in_progress` is false and then reads `apply`.

A second install while the apply job is loaded and has not ended, including the moment after
launchd has loaded it and before it has started, is `busy`: the stage is not touched. A failed
move of the links is undone before `failed:links_failed` is recorded, so `previous` keeps naming
the release before the last apply.

### A rollback is an install of an older version

There is no floor and no "not older" refusal. To go back, install the older version from the
channel that offers it; it takes the same path, and `previous` is only where the last apply came
from.

The other side of this is that whoever holds the updater's key can install any release the
channel offers that passes the attestation, including an old one with a known defect. On a
channel with `sums` the way to make a release uninstallable is to remove its line from that file;
on one without, it is to remove its files from `dir`.

### `query` on a Mac

```json
{"result":"query","installed_version":"..."|null,"running_commit":"..."|null,"node_id":"..."|null,"in_progress":true|false,"apply":"..."|null}
```

- `installed_version` is the `VERSION` of the release `current` names, or null.
- `running_commit` is the commit of the program the RUNNING daemon executes: the pid from
  `launchctl print system/com.komira.kbf-daemon`, and the file `lsof` says that process has
  mapped as its program, which must be `releases/<commit>/kbf`. It is null when any step does not
  say; it is never read from the disk. A release that `current` names and the daemon has not
  restarted onto is therefore not reported as running.
- `node_id` is `NODE_ID` of `av2_mac.conf`, read as data: every line must be blank, a `#`
  comment or `KEY=VALUE` with a plain value, the value must be a DNS label, and it must appear
  once; any other file gives null.
- `apply` is new on a Mac, and the install answer `apply_started` is too. The failures reuse the
  closed codes of the Linux side (`verify_failed` for every check above, `install_failed` for a
  stage or launch that failed, `busy`, `package_state_unclean` for a missing kit); the words are
  on stderr.

### A channel on GitHub

A channel with `source = "github"` needs no file delivery and no edit of the allowlist for a new
release: the install fetches the release itself, as `nobody`, from the GitHub releases of
`komira-ai/kbf`, and then handles the two files exactly as it handles the files of a `dir`
(copied once into the stage, `sums` checked when set, then the offline attestation check and
everything after it). The build attestation is still the only trust anchor; the listing and the
download only bring the bytes. A directory channel stays for a Mac that cannot reach GitHub.

What the install does, in this order, with the system `curl` (listing) and the pinned `gh`
(download), both run as `nobody` (the same privilege drop as the attestation check) in a
directory that `nobody` owns inside the stage, with its own `HOME`:

1. lists the repository's releases with `/usr/bin/curl` from the public REST endpoint
   `https://api.github.com/repos/komira-ai/kbf/releases?per_page=100&page=N`, with no token. The
   flags are fixed: no configuration file (`-q` first), HTTPS only, no redirect, an HTTP error
   fails the command, a time limit, a size limit, and an empty environment. It is 100 a page, newest first, page
   after page until a page is empty, and at most 30 pages (an empty page ends it; a short one
   does not); never only the latest, so a rollback to an old release works;
2. finds the asset named `kbf-<40 hex>-darwin-arm64.tar.gz` whose commit starts with the 12 hex
   digits of the version, across all the pages read. None is `unknown_release`; assets of two
   different commits are `ambiguous_release`; the same commit in two releases is one release (the
   newest). A listing cut at the 30-page limit without a match is `fetch_failed`, not
   `unknown_release`, since not all of it was seen. When the channel has `sums`, the commit is
   the one the sums list, and the listing must hold it;
3. builds the download itself from the release's tag and the two asset names:
   `gh release download <tag> --repo komira-ai/kbf --pattern <tarball> --pattern <bundle> --dir <dir>`.
   The URLs in the listing (`browser_download_url`, `url`) are never read or followed, and an entry
   whose tag is not letters, digits and `. _ + -` is not used;
4. runs that download under a time limit (15 minutes) and a limit on what it may write (the
   tarball limit plus the bundle limit, 512 MiB and 256 KiB, watched while it runs; the sizes the
   listing gives are checked before it starts). A file over its limit, a timeout, a network
   failure, an answer that is not a listing and a release without its bundle are `fetch_failed`;
5. copies the two files once, as root, into the stage, and removes the directory `nobody` wrote
   in. From here on nothing differs from a directory channel.

Every one of these refusals is exit status 2 with no change to the kit, the status file or
launchd, and no stage left behind.

#### What the Mac needs

- Outbound HTTPS to GitHub: `api.github.com` and `github.com`, and the host GitHub serves
  release assets from. The child's environment is cleared, so no proxy variable reaches `gh`;
  a Mac that can reach GitHub only through a proxy cannot use this source.
- No token. The listing and the download of a public repository need none, and by default
  no secret is on the Mac.
- Optionally, a token for a higher rate limit. A channel may name a `token_file`: a file that is
  root's alone to write (checked like the allowlist) holding a GitHub token, one line of letters,
  digits, `_` and `-`. It needs no scope: the repository is public. The root process reads it and
  sends it to `curl` as an `Authorization` header in a configuration on `curl`'s standard input
  (never in its arguments, which every user can read), and gives it to `gh release download` as
  `GH_TOKEN`; the attestation check never gets it.
- The system `curl` that macOS ships, `/usr/bin/curl`, which must be root's and not writable by
  others (checked like the pinned `gh`, and only for a GitHub channel: a directory channel never
  runs it).

#### The residuals

- A token, when a channel names one, is a credential on the Mac. It can read public data only,
  but it is an account's token; `nobody`'s `curl` process holds it in memory and `gh release
  download` in its environment while they run. Without a `token_file` there is none.
- The listing is read through GitHub's API and is not itself trusted: it can only choose among
  releases (an old attested release is installable, as with a directory) or withhold one (the
  install fails). What it picks must still pass the attestation for the full commit.
- Without a token GitHub's API allows 60 requests an hour per IP address, shared by every
  machine behind the same address. One install costs one request per 100 releases listed (a
  repository with fewer than 100 releases: one request for the releases and one for the empty
  page that ends the listing, so two), and the download is not counted against the API limit.
  Past the limit GitHub answers 403, and the install is `fetch_failed` with nothing staged:
  retry later (the limit resets within the hour), or name a `token_file`, which raises it to
  the token's own. A listing of about 3000 releases or more does not
  show its oldest releases, so whoever can publish many releases can make a rollback to an old
  one fail (`fetch_failed`); it cannot make a wrong release install.
- The size limit is watched about every 10 ms, so a fast download can write a little past it
  before it is killed; the copy then refuses the file, and the stage is removed.
- A channel with `sums` pins the commit and the tarball's hash, and needs an edit of the sums file
  for each new release; the point of this source is a channel without them.

## The local socket

`kbf-host-install --serve` is the program's other door, for the agent mode of `kbf-updater`
([kbf-updater.md](kbf-updater.md#the-agent-mode), [../design/updates.md](../design/updates.md)).
It serves a unix socket instead of the ssh forced command, which stays as described above and
is not changed. It does one host's A/B: put the next release beside the one that runs, switch to
it on trial, keep it or go back.

```text
kbf-host-install --serve --socket PATH --socket-user NAME [--window-secs N] [--restart-unit UNIT]...
```

- `--socket`: an absolute path. The program binds it itself (a `umask` of 0177, then a `chown`
  to the account), mode 0600, in a directory that is root's and that others cannot write. A
  stale socket file is replaced; a file of another kind in the way is an error.
- `--socket-user`: the one account that may connect. On Linux it is looked up in `/etc/passwd`,
  on a Mac with `dscl`; root is refused.
- `--window-secs`: how long a release on trial has to be confirmed, 120 to 1800 (default 600).
- `--restart-unit`: on Linux, a unit that runs the release (`kbf-<name>.service` or
  `kbf-<name>@<instance>.service`), repeated for each; at least one is required. A Mac restarts
  the daemon's launchd job and takes none.

Nothing else is configured by a flag or the environment: the files this program reads as root are
constants, as above. The units and jobs that start it are in
[`packaging/host-agent`](../../packaging/host-agent/README.md).

### The five requests

One line in, one JSON line out, one connection at a time. The same grammar for the version and
the channel as the forced command's (single spaces, no control byte; no path or URL anywhere).

```text
query
install kbf=<version> from <channel>
switch kbf=<version>
confirm kbf=<version>
revert
```

| request | does |
|---|---|
| `install` | looks `<version>` up in the channel, fetches the release **on the host**, verifies it, and lays it out beside the one that runs. No link moves and nothing restarts. The same version asked again is answered from what is laid out; the version the host runs already is answered at once (`current: true`) and teaches the host its version text |
| `switch` | puts the laid-out release in place **on trial**: the pending marker, with its deadline, is written before the first link moves; then the links move and the units restart on it. A unit that does not come up puts the host back at once |
| `confirm` | keeps the release on trial. Asked again after it was answered, the answer is the same |
| `revert` | goes back to the release that ran, now |
| `query` | what runs, what is on trial and until when, how the last update ended, and whether another install, switch or restart is running |

Answers: `staged` (`version`, `commit`, `current`), `switched` (`version`, `commit`,
`deadline_unix`), `confirmed`, `reverted` (`version`, the release the host is back on, when it
knows its text), `query` (`commit`, `version`, `node_id`, `pending`, `last`, `busy`), and
`{"result":"refused","reason":CODE}` (nothing ran) or `{"result":"failed","reason":CODE}` with a
code from a closed set: `bad_command`, `channel_not_allowed`, `config_invalid`, `config_unsafe`,
`unknown_release`, `ambiguous_release`, `fetch_failed`, `verify_failed`, `pending`, `not_staged`,
`nothing_pending`, `no_current`, `busy`, `stage_failed`, `switch_failed`, `revert_failed`,
`state_unavailable`, `audit_unavailable`. The detail of a refusal goes to the journal, cleaned,
never to the line.

### The revert is this program's

A release on trial has a deadline, in wall-clock seconds, on disk with the marker. The program
checks it about five times a second between connections: a release that nobody confirmed by its
deadline is put back by this program, **needing nothing of the release on trial to run**, the
updater to be alive, or the host to have stayed up. The first check after a start is the same
check, so a host that rebooted with a release on trial goes back at once if the deadline passed
while it was down, and waits out what is left of the window if it did not. A revert that
cannot be done (the units will not start on the release that ran) leaves the marker, and the next
check tries again.

Every request but `query` is audited like a forced-command request (verb `stage`, `switch`,
`confirm` or `revert`), and the install, the switch and the confirm are not run when the audit log
cannot be opened. A revert is never held back by it. The install lock is the same file as the
forced command's, so the two never overlap.

### On Linux

The release goes in the layout of `kbf-installer` ([host-releases.md](host-releases.md)):
`/var/lib/kbf-installer/releases/<commit>`, the `current` and `previous` links, and the
installer's `state.json`, which is also where the pending marker lives (`pending_confirm`, with
the deadline). A host that was set up by hand with `kbf-deploy host install` has all of it; a host
with no `current` release has nothing to go back to and is refused (`no_current`). The channels
are in `/etc/kbf/host-install-releases.toml`, in the format of the Mac's allowlist below (a
channel whose `source = "github"`; the ssh path's `host-install.toml` is the apt channels' and is
not read):

```toml
[channels.stable]
source = "github"
```

`install` lists the repository's releases, downloads `kbf-<commit>-linux-<arch>.tar.gz` and its
bundle as the installer's unprivileged user, copies them once into the installer's scratch, and
runs the installer's verify pipeline over the copy with the pinned `gh` and trusted root
([host-releases.md](host-releases.md#what-is-trusted)): the attestation must be of this
repository's `artifacts.yml` on a hosted runner, for the full commit, on the channel's branch; the
tarball must hold the one executable of this CPU; and the binary run as the unprivileged user must
say it is that commit. Nothing is fetched when the trust files are not root's. A release whose
`protocol` or `state_schema` is newer than this program's `kbf-installer` understands is refused
(the installer's rule, `verify_failed`): put the newer installer on the host first.

### On a Mac

The release is checked and laid out by the same code as the ssh path's install (above), into the
kit tree, with the channels of the same `host-install.toml`. The swap is the apply job's: the links,
then a restart of the daemon's job that must say it was welcomed, and the old release started again
if it does not. The pending marker is `pending.json` in the work directory, root's and 0600, and
holds the links as they were.
