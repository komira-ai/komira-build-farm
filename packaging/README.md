# packaging

- `install.sh`: the curl|sh entry of `kbf-installer`
  (docs/design/kbf-installer.md section 9 P11, docs/design/distribution.md section 2).
  It detects the platform (`linux-x86_64`, `linux-arm64`; a Mac gets the `.pkg`),
  downloads `SHA256SUMS` and the `kbf-installer-payload-<commit>-<platform>.tar.gz` it
  lists from a release URL, refuses unless the tarball's sha256 is the listed one and
  every entry is a regular file at a path the package owns, then places the files under
  `/` (or `--root DIR`). It starts and enables nothing and prints the next step,
  `sudo kbf-installer setup`. After placing it makes the users and directories from the
  two placed files (`systemd-sysusers`, `systemd-tmpfiles --create`, those files only); a
  host without those tools is refused before anything is placed. It is the one shell file the distribution keeps
  (`tools/interpreter_budget.tsv`); every builder is Rust.
- The payload is `cargo xtask installer-tarball`: the `kbf-installer` Debian package's file
  list, read from the same Rust list (`xtask::pkg::files`, which the deb and rpm are built
  from), so paths and modes cannot drift: `usr/bin/kbf-installer` and the pinned `gh` 0755, the
  `kbf-*.service` and `kbf-*.socket` units and `kbf.slice`, exactly the three trust files, and
  `usr/lib/sysusers.d/kbf-installer.conf` and `usr/lib/tmpfiles.d/kbf-installer.conf`
  (the files under `packaging/linux/`), with fixed owners and times so the same inputs
  are the same bytes. It holds no `kbf` and none of the five mode units: the `kbf` package
  (`/usr/bin/kbf` and the mode units, `packaging/linux/README.md`) carries them. `setup` still
  enables the unit of its mode, so on a host that has this payload and not the `kbf` package it
  fails on the missing unit, after it has written its other files; install the `kbf` package
  first. `install.sh` installs no deb, and no rpm is published as an asset, so a host set up
  through `install.sh` has no route to the mode units yet.
- Tested by `crates/xtask/tests/install_sh.rs` against a local HTTP server, with a stub
  installer (`xtask-fake`) and stub unit, trust, sysusers.d and tmpfiles.d files, and fake systemd tools.
- `container/frontend/Dockerfile` and `container/kbf-coordinator/Dockerfile`: the images of
  the frontend (`kbf --mode=frontend`) and of the update coordinator (docs/design/updates.md): the
  binary on a distroless `cc` base pinned by digest, run as the base's unprivileged user (uid
  65532), its settings, keys and state mounted by the orchestrator and none baked in. The release
  workflow builds both for each Linux platform from the binaries it checked (the frontend's is the
  attested `kbf` of its tarball) and ships them as `kbf-<service>-image-<commit>-<platform>.oci.tar`,
  an OCI image layout, in `SHA256SUMS` and attested (docs/artifacts.md); there is no registry, and
  docs/deploy/containers.md imports and runs them. Each Dockerfile's header says how to build one
  from a local binary.
  Operating notes: the coordinator takes its own `--oidc-config`, and its audience should be
  one no other verifier accepts (not the frontend proxy's), so a token minted for another
  service cannot set the desired state. An updater certificate is not revocable: a stolen
  one acts only as its own node, but it can hold its role's update slot or report a dead host
  healthy, and an old version reported by any daemon, storage or Mac host holds back the
  frontends. Replace the farm CA or let the certificate expire to end it.
