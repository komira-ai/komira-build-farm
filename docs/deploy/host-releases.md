# Host releases: the layout and the manual verbs

A host keeps each kbf release in its own directory and points two links at them.
`kbf-deploy host` reads and changes that layout, and so does the root helper that applies
updates the host pulled: both call the `kbf-installer` crate, so a host has **one state
file and one layout**, on Linux and on macOS.

```text
/usr/local/kbf/releases/<commit>/   one release: kbf (Linux) or kbf-daemon (macOS), units/
/usr/local/kbf/current              link to releases/<commit>, the release that runs
/usr/local/kbf/previous             link to releases/<commit>, the one before it
<state dir>/state.json              the deploy state (0600)
<state dir>/work/                   scratch for a stage in progress, removed after it
<lock>                              the lock the state is changed under (0600)
```

The two platforms differ in the state directory and the lock only:

| | Linux | macOS |
|---|---|---|
| state dir | `/var/lib/kbf-deploy` | `/var/db/kbf-deploy` |
| lock | `/run/kbf-deploy.lock` | `/var/run/kbf-deploy.lock` |

The Linux state directory keeps its name: live hosts use it. macOS has no `/run`, and
`/var/db` is where it keeps system state. `kbf-deploy host` takes the platform it was
built for; `--platform linux|macos` names the other (for a mounted image). On a fresh Mac
the install makes the `/usr/local` and the state directory it lacks, root-owned and 0755;
a `/usr/local` that is there is not touched.

`<commit>` is always the full 40-character lowercase commit id. The links are relative
(`releases/<commit>`). A release directory, its `units/` and its binaries are root's and
0755 (units 0644), so the user a service runs as cannot replace its own binary. A Linux
release holds `kbf`; a macOS release holds `kbf-daemon`, which is what the Mac's
LaunchDaemon runs (`/usr/local/kbf/current/kbf-daemon`). A release may hold both.

## The verbs

Every verb prints its plan and changes nothing unless `--apply` is given; `--apply`
needs root. The state file is root's alone, so run the dry runs with `sudo` too.

```sh
sudo kbf-deploy host status
sudo kbf-deploy host install --commit <commit> --from <dir>          # dry run
sudo kbf-deploy host install --commit <commit> --from <dir> --apply
sudo kbf-deploy host rollback [--apply]
sudo kbf-deploy host gc [--keep N] [--apply]
```

- `install` copies `<dir>/kbf` and `<dir>/kbf-daemon` (whichever are there) and
  `<dir>/units/*` (when present) into `releases/<commit>/`, refuses a release directory that is already there with other
  content, checks that each copied binary's sha256 is the source's, then points
  `previous` at the old current release and `current` at the new one and writes the
  state. Installing the current release again changes no link.
- `rollback` points `current` at the previous release and removes `previous` (the state
  records no release before it). It refuses when there is no previous release or its
  binary is gone.
- `gc` removes release directories. It never removes the current, previous or staged
  release, nor one the `current` or `previous` link points to; `--keep N` also keeps the
  N newest others (by the directory's modification time). Anything in `releases/` that
  is not a directory named by a commit id is left alone.

Each refuses while a commit-confirm deploy is pending in the state.

**No verb restarts a service.** After `install --apply` or `rollback --apply`, restart
the units that run `/usr/local/kbf/current/kbf` by hand (`sudo systemctl restart
<unit>`), then check `kbf-deploy host status`. Installing the units and restarting them
belongs to `kbf-deploy linux install-units`.

## Staging a release that was verified offline

The root helper does not use `install` on a file it was handed. It stages: the
`kbf_installer::stage` module takes an inbox entry (a directory the unprivileged `kbf`
process wrote: `offer.json`, the release's tarballs and their attestation bundle),
checks everything below in this order, installs the release into `releases/<commit>/`
and records it as the state's `staged` release (`kbf_installer::release::stage`). It
moves no link and restarts nothing; activating is a later, separate step. A refusal
installs nothing and leaves nothing behind.

1. `gh`, the Sigstore trusted root and the host settings are root's alone (below).
2. The entry is opened without following a link, and holds only the files `offer.json`
   names. Each is opened beneath the entry (`O_NOFOLLOW`; `openat2` with
   `RESOLVE_BENEATH` on Linux) and refused if it is a link, a FIFO, a device, a
   directory, hard-linked, empty or over its size cap. On macOS, a file carrying the
   `com.apple.quarantine` attribute is refused.
3. Each file is copied by its open descriptor into a scratch directory root owns; the
   original is not read again. The copy is hashed and must have the sha256 the offer
   names.
4. `gh attestation verify` accepts each tarball with the bundle, for the offered commit.
   Nothing has been unpacked or run yet.
5. Each tarball must hold exactly one regular file named like the component, with no path
   and no link; it is unpacked, and its header must be this host's kind of binary
   (ELF on Linux, Mach-O on macOS, for this CPU). On macOS its code signature must be
   valid and ad hoc with the hardened runtime, which is what CI's `sign-darwin` makes.
6. The binary runs `--version --json` as the unprivileged user (empty environment, empty
   directory, a timeout) and must say it is the offered commit's `release` build. Its
   `protocol`, `min_protocol` and `state_schema` are reported for the rollout planner.
7. The release is installed. A release directory that is there with other bytes is
   refused, never replaced.

Every refusal is one case of `kbf_installer::refusal::Refusal` with a stable code
(`not-attested`, `hash-mismatch`, `inbox-symlink`, `wrong-platform`, ...) and a one-line
message.

### What is trusted

**MVP trust:** a host trusts the GitHub build attestation of `komira-ai/komira-build-farm`'s
artifacts workflow (`artifacts.yml`) for a source ref, `refs/heads/main` unless the host's
settings name others, verified offline with a pinned `gh` and a pinned Sigstore trusted
root. The frontend and the inbox are untrusted bytes until the attestation passes; the
frontend only says which commit and carries the files. Residuals: `main` is not
branch-protected, there is no downgrade floor in the MVP (an older attested release is
accepted), and a separate Ed25519 release key is a documented later option.

The trust list is `/usr/local/libexec/kbf-deploy/`, root-owned and closed to group and
other writes, the same on both platforms:

- `gh`: the pinned GitHub CLI release (2.102.0 at this writing; Linux amd64 and arm64
  tarballs and macOS arm64 zip are in the release's checksum list).
- `trusted_root.jsonl`: `gh attestation trusted-root`, refreshed when Sigstore rotates a
  key (a stage then says `not-attested`).
- `installer.conf` (optional): `SOURCE_REFS`, `RUN_AS_UID`, `RUN_AS_GID`.

They are installed by the host's bootstrap, not by this repository. The pins (version,
sha256 of `gh`, the trusted root's sha256) live with it. Each of them, and every directory
above it, must be owned by uid 0, not a link, and not writable by group or others;
otherwise the stage refuses and names the path (`untrusted-owner`, `untrusted-writable`,
`untrusted-link`, ...). The pins are not under `/etc` because `/etc` is a link on macOS.

### macOS requirements

- The bootstrap needs a **`gh` for darwin-arm64** (the release's `gh_<version>_macOS_arm64.zip`).
- `/usr/local` must be `root:wheel` 755 and **not group-writable**: Homebrew on Intel
  leaves it writable by the `admin` group, and the trust check then refuses, naming
  `/usr/local`. (On Apple Silicon Homebrew uses `/opt/homebrew`.)
- Untrusted bytes are handled as `nobody`, which on macOS is uid and gid 4294967294 (-2).
  UNVERIFIED on the farm's Macs, as is the uid the Mac daemon runs as and whether it is
  an administrator: the survey of the Macs must confirm both. `installer.conf` can name
  another user.
- `kbf-daemon --version --json` is not implemented today (`kbf-daemon --version` prints one
  plain line); the version check above is what a Mac release needs it for.

## Swapping a staged release in on trial

`kbf_installer::activate` is what a program that applies orders calls (no command of
`kbf-deploy` does yet). It never changes a release's files: a release directory is
written once, so the process running the old release keeps its file, and only the
`current` and `previous` links move.

| Call | Does |
|---|---|
| `activate` | an order names a staged release and an order id (the helper names it `<epoch>-<id>`). The state is written first (`current` = the new release, `previous` = the one that ran, `staged` cleared, a pending record naming the order), then `previous` and `current` move, each by an atomic rename. The caller restarts the service. A repeat of the order, or an order for the release already current, changes nothing. The installer keeps no floor; the order of orders is the node's client and the helper's (epoch, then id). |
| `confirm` | the new release came up: the pending record is cleared. It must name the order that made the swap. |
| `revert` | `current` goes back to the release that ran before; the state is written first. If that release's files are gone nothing changes and the swap stays pending. |
| `boot_check` | run before every start of the service. The first start after a swap starts the confirm window (not the swap: the frontend's own update can wait hours between the two); with a start budget, a release started that many times without a confirm is reverted before it starts again. |
| `expire` | a swap whose window ran out without a confirm is reverted. On macOS, where nothing runs before a start, the window starts at the swap and this is the only trigger. |
| `reconcile` | after an interrupted verb, make the links agree with the state. |

The window is bounded by the host (120 to 1800 seconds), not by the order. Retention is
`gc --keep 1`: the running release, the previous one and the newest other.
