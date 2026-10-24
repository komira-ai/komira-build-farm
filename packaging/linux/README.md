# kbf-installer and kbf: the Linux packages

Two packages. `kbf-installer` is the root tool that sets a host up and the proxy that fronts
it; it ships no `kbf` runtime and `setup` installs no first release. `kbf` is the runtime,
`/usr/bin/kbf`, and the systemd units of its modes. Installing a newer `kbf` package is the
update: the package manager replaces `/usr/bin/kbf`. The contract is `docs/design/kbf-installer.md`, sections 3 (files and users), 7
(units and gating) and 9 (P11). A deb and an rpm of each are built by
[nfpm](https://nfpm.goreleaser.com), pinned in CI by SHA-256, from a configuration `cargo xtask
pkg-config` writes (for both packages): this directory holds the inputs, and
`crates/xtask/src/pkg_linux.rs` is the one place that says what goes in each.

| path | what |
|---|---|
| `sysusers.d/kbf-installer.conf` | the users `kbf`, `kbf-server`, `kbf-fetch`, `kbf-signer` (no login) and the group `kbf-signer-clients` (the signer and `kbf-server` are its members: see `kbf-signer --client-group`); installed to `/usr/lib/sysusers.d` |
| `tmpfiles.d/kbf-installer.conf` | `/run/kbf-installer` (0755), `/var/cache/kbf-fetch` (0711); the units author's file, packaged unchanged; installed to `/usr/lib/tmpfiles.d` |
| `needrestart/kbf.conf` | tells needrestart never to restart a unit named `kbf-*`, `kbf.*` or `kbf@*`; installed to `/etc/needrestart/conf.d/kbf.conf` (0644, a config file the host may edit) |
| `units/` | the unit files (`kbf-*.service`, `kbf-*.socket`, `kbf.slice`), installed to `/usr/lib/systemd/system`: the five mode units go in the `kbf` package, the others in `kbf-installer`; `testdata/units` is the fixture for the unit tests only |
| `testdata/` | fixtures for the package tests only: gated units (the five mode units among them) and three stand-in trust files |
| `test/ubuntu`, `test/fedora` | the fresh systems the install test uses, systemd as PID 1, pinned by digest |

## What the packages hold

`kbf-installer`:

- `/usr/bin/kbf-installer` (0755);
- every file of `packaging/linux/units` but the five mode units (`kbf-installer.service`,
  `kbf-proxy.service`, `kbf-proxy.socket` and `kbf.slice`; the unit files are packaging's input
  from the units author, packaged unchanged) in `/usr/lib/systemd/system` (0644). `kbf.slice`
  stays here because `kbf-proxy.service` runs in it as the mode units do;
- `/usr/share/kbf-installer/trust/`: `gh` (0755), `trusted_root.jsonl`, `installer.conf` (0644);
- the sysusers.d file (this package's) and the tmpfiles.d file (the unit author's, packaged
  unchanged; `/var/lib/kbf-installer` is made by `kbf-installer setup`, not at install);
- `/etc/needrestart/conf.d/kbf.conf` (0644): on Ubuntu, needrestart restarts every service that maps a library an
  unattended upgrade replaced. A restart of a storage member in the middle of a build leaves the blobs only that
  member holds unreadable for the restart, and the client's build fails. This drop-in sets needrestart's
  `override_rc` for kbf units to 0, so only kbf's own update path restarts them. A host without needrestart ignores it.
- all of it `root:root`, with one time (`SOURCE_DATE_EPOCH`) on every file and the package.

`kbf`:

- `/usr/bin/kbf` (0755), the `kbf` the same job built, byte for byte the one in its release tarball;
- the unit of every mode, the five files `kbf-all-in-one.service`, `kbf-daemon.service`,
  `kbf-frontend.service`, `kbf-storage-and-daemon.service` and `kbf-storage.service`
  (`KBF_UNITS`), in `/usr/lib/systemd/system` (0644). `cargo xtask pkg-config` fails when the
  unit directory lacks one;
- nothing else, all of it `root:root`, built in the same way;
- no dependency on another package, the `kbf-installer` package included: each mode unit runs
  `kbf boot-check`, which is the `kbf` file the package carries. (`kbf setup` is also this
  package's, for a host with no proxy; the `kbf-installer` package is still where `crun`,
  `uidmap`, `systemd` and `ca-certificates` come from, and the one that sets a host up behind
  the proxy.) `pkg-kbf-asset` refuses a deb whose
  `Depends` names `kbf-installer`;
- in the deb only, `Replaces: kbf-installer` (any version, no `Conflicts`). The five mode units
  were files of the `kbf-installer` package before, and dpkg refuses to unpack `kbf` over files an
  older `kbf-installer` owns unless `kbf` says it replaces that package. The rpm names nothing: an
  rpm `Obsoletes` would remove the installer, which a host still runs. An rpm host that has the
  installer of before the split must upgrade it in the same transaction as `kbf`, because that
  installer's rpm owns the five unit files. `pkg-kbf-asset` checks the deb's `Replaces`.

### Which frontend unit

A frontend host runs `kbf-frontend.service`, and may put `kbf-proxy` in front of the frontends.

- `kbf-frontend.service`: a frontend. One process, `kbf
  --mode=frontend --config /etc/kbf/kbf.toml`, run from `/usr/bin/kbf` (no slots, no
  `--instance`, no `--handover-socket`); listeners, state store and certificates are
  `kbf.toml`'s. An update is a restart after the `kbf` package is upgraded.
- `kbf-proxy` (the unprivileged proxy in the `kbf-installer` package: the public port and the
  load balancer over the frontends of its pool, which run on their own; it starts none of
  them).

`pkg-config` refuses anything else in the unit directory (a unit named none of `kbf-*.service`,
`kbf-*.socket` and `kbf.slice`, a fourth trust file, no unit of the `kbf-installer` package).

The socket-handover template (`kbf-frontend@.service`, instances `a` and `b`) is no longer
packaged: `kbf` serves no control socket
(`--instance`, `--handover-socket` and the other flags of the removed handover are accepted,
ignored and logged as retired).

Dependencies: the `kbf-installer` deb `crun, uidmap, systemd, ca-certificates`; its rpm the same
with Fedora's name for `uidmap`, `shadow-utils`. Never Podman, never Envoy (a test pins it):
nothing in these packages depends on, installs or checks Envoy, and kbf runs none. The `kbf`
package adds only `kbf-installer`.

## Installing starts and enables nothing

Neither package has a maintainer script (`xtask pkg-install-test` fails on one in the
`kbf-installer` package, in both formats; the configuration of the `kbf` package names none,
which a test in `pkg_linux/tests.rs` pins), so nothing runs at install, upgrade, remove or
purge: no unit is enabled, started or restarted, no user or directory is created by a script.

- Units ship disabled. Each gates on its configuration (`ConditionPathExists=` of
  `/etc/kbf/kbf.toml` for a mode unit, `/etc/kbf/installer.toml` for
  `kbf-installer.service`); the test starts each `--gate` unit of the `kbf-installer` package
  without its file (skipped, `ConditionResult=no`, no `ExecStart` run) and with it (condition
  holds). The mode units' gates are held by
  `crates/kbf-integration-test/tests/package_units.rs`, which reads the files.
- **Upgrading `kbf` restarts nothing.** `apt install kbf` or `dnf upgrade kbf` replaces
  `/usr/bin/kbf`; a running process keeps the old file. An operator (or the tool that drains a
  node and installs the new package) restarts the mode unit after the install
  (`systemctl restart kbf-daemon.service`; a stop drains: a storage node finishes the calls in
  flight, a daemon its leases). The `kbf` package carries no script that restarts a unit, and
  no other mechanism does it yet.
- `/etc/kbf/kbf.toml`, the file those gates test, is written by `kbf-installer setup`, not by
  the package: for a frontend or all-in-one host from flags that name the listeners, the
  certificates and keys (which the operator provides, owned by the unit's user) and, for a
  frontend, the storage node, for an all-in-one host the cold store and the daemon half's
  identity (`docs/deploy/kbf-installer-setup.md`, "The `kbf.toml` flags"). The unit's
  `ExecStartPre=... --check` reads that file, so a file `setup` wrote is one `kbf --check` accepts.
  On a host with `/usr/bin/kbf` and no release tree, `setup` starts the unit of a node mode as it
  does for a host with a release.
- The users and directories come from `systemd-sysusers` and `systemd-tmpfiles`, which
  run at boot, and from `kbf-installer setup`. After `dpkg -i` and before either, they do
  not exist; the test runs both commands as boot does and checks the result.
- `remove` deletes the packaged files and keeps `/etc/kbf`, `/var/lib/kbf-installer` and
  the users. `purge` (deb) does the same and drops the package's record: the package owns
  none of what `setup` and the operator write, so it deletes none of it.

## In CI: the real packages, attested

`beta.yml`'s `packages` job builds both packages (deb and rpm) from the test-only stub and
the real units and installs the `kbf-installer` deb and rpm. `artifacts.yml`
builds the real ones: each Linux job (`linux-x86_64` on `ubuntu-24.04`, `linux-arm64` on
`ubuntu-24.04-arm`, each with its own pinned nfpm and `gh`) runs the same verbs on the
`kbf-installer` and the `kbf` it has just built, with a trust directory `cargo xtask pkg-trust`
writes (the pinned `gh`, the Sigstore trusted root `gh` fetched, and an `installer.conf` whose
`SOURCE_REFS` is the branch the package is attested on), installs the `kbf-installer` deb in a
fresh Ubuntu container (`cargo xtask pkg-install-test`, with `--probe`s that the packaged `gh`
runs and the binary names the commit), and `cargo xtask pkg-asset` makes
`kbf-installer-<commit>-<platform>.deb` an asset, listed in `SHA256SUMS` and attested with
a bundle ([docs/artifacts.md](../../docs/artifacts.md#the-kbf-installer-package-deb)).
`cargo xtask pkg-kbf-asset` does the same for `kbf-<commit>-<platform>.deb`, refused unless its
`/usr/bin/kbf` is the `kbf` of the job's own tarball ([the `kbf` package](../../docs/artifacts.md#the-kbf-package-deb)).
The `kbf` package is built twice and compared like the other, but not installed in a
container: the install test takes one package.
Two call sites, one way to build: both go through `pkg-config` and `pkg-build`.

## What the units run

- `kbf-installer.service` runs `/usr/bin/kbf-installer run`. It names no unit: `run` restarts
  the unit `setup` enabled for the `mode` of `/etc/kbf/kbf.toml` (on a frontend or all-in-one
  host it also reads the state database from `/etc/kbf/installer.toml`), so one packaged unit
  serves every mode. `--unit` and `--launchd-label` still name the service by hand. The unit has
  no `ExecStartPre=`: the contract's `check` is not built, and a stub would fail every start.
- Each mode unit runs `/usr/bin/kbf --mode=<mode>`, the `kbf` package's file, after
  `kbf boot-check --mode=<mode>` (which brings the state database named by the `database` key of
  `/etc/kbf/installer.toml` to the schema that `kbf` itself needs, for the modes that keep state
  in it, and fails the start if it cannot) and `kbf --mode=<mode> --check`. The installer's own
  `boot-check --kbf=<kbf> --mode=<mode>` stays for one release for the units of the earlier
  packages: it runs `<kbf> boot-check --mode=<mode>` over the host's `installer.toml` and ends
  as that ended. It needs a `kbf` that has the verb: upgrade the `kbf` package with the
  installer or before it.
- `crates/kbf-integration-test/tests/package_installer_verbs.rs` parses every `kbf-installer` command line of
  these units, and of the macOS property list, with the binary's own parser and refuses a verb that
  is not built; it also holds every `kbf` the units exec to `/usr/bin/kbf`.

## Commands

```
cargo xtask pkg-config --binary <kbf-installer> --kbf <kbf> --units packaging/linux/units \
  --trust <dir of gh, installer.conf, trusted_root.jsonl> \
  --sysusers packaging/linux/sysusers.d/kbf-installer.conf \
  --tmpfiles packaging/linux/tmpfiles.d/kbf-installer.conf \
  --needrestart packaging/linux/needrestart/kbf.conf \
  --platform linux-x86_64 --version <v> --out <dir>
      # <dir>/nfpm.yaml, listing.txt (kbf-installer) and <dir>/kbf/nfpm.yaml, listing.txt (kbf)
cargo xtask pkg-build --nfpm <nfpm> --config <dir>/nfpm.yaml --out <pkgs>
cargo xtask pkg-build --nfpm <nfpm> --config <dir>/kbf/nfpm.yaml --out <kbf-pkgs>
cargo xtask pkg-install-test --package <file> --format deb|rpm --listing <dir>/listing.txt \
  --image-dir packaging/linux/test/ubuntu --gate kbf-installer.service=/etc/kbf/installer.toml ...
cargo xtask pkg-kbf-asset --package <kbf-pkgs> --platform linux-x86_64 --commit <sha> \
  --kbf-tarball <kbf-sha-platform.tar.gz> (--out <dist> | --verify-only)
```

`pkg-build` builds each package twice with the sources' modification times changed
between the builds, and fails unless both are the same bytes (nfpm takes every time from
`SOURCE_DATE_EPOCH`, which the verb sets; the rpm's build host is fixed in the
configuration).

## CI

The `packages` job of `.github/workflows/beta.yml` fetches the pinned nfpm, builds the
test-only `kbf-installer-stub` and the packages (the stub stands for both binaries), then runs
`pkg-install-test` on the `kbf-installer` deb in Ubuntu and rpm in Fedora. The real binaries
replace the stub by two path changes: the `--binary` and the `--kbf` of `pkg-config`.
