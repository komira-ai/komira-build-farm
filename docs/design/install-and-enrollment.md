# kbf install and enrollment: packages, one config file, tokens, a frontend-issued certificate

Status: design, nothing built. It defines the units B2 to B5 and the retirement B6 of the product-install work. Labels: **[F]** fact read in the code of `feature/arch-v2` (and `feature/pull-updates` where said), **[D]** a decision this document makes, **[I]** inference, **[V]** verify before building on it, **[owner]** a choice that is the owner's. The owner's decisions of the review round are recorded as **Decided** in section 12 and applied throughout; what is still open there keeps its lean. Where this document and older prose disagree, the code wins and the disagreement is named (section 2.3).

Related: [architecture-v2.md](architecture-v2.md) (modes, membership, security), [pull-updates.md](pull-updates.md) and [split-mode-updates.md](split-mode-updates.md) (the updater and the rollout; on `feature/pull-updates` until it merges), [../deploy/kbf-modes.md](../deploy/kbf-modes.md) (the unit rules), [../deploy/tls-renewal.md](../deploy/tls-renewal.md) (certificate reload), [../api.md](../api.md).

## 1. The target, and the answer in one page

The owner's target, seven lines, and where each is answered:

| # | Target | Answer | Section |
|---|---|---|---|
| 1 | OS package install | a `.deb` (Linux), a signed `.pkg` installed by a Homebrew cask (macOS), a container image (frontend) | 3 |
| 2 | Joining a node = a config file: the frontend URL and an enroll-token file | `kbf.toml`, two required values | 4 |
| 3 | Frontend-issued enroll tokens: roles, expiry, uses, stored hashed, shown once | `kbf token create`, rows in the state store | 5.1 |
| 4 | The node makes its key and sends token plus CSR; the frontend issues a short-lived, auto-renewed mTLS certificate and registers the node | the `kbf.enroll.v1` service on a listener of its own; the CA lives in the frontend | 5, 6 |
| 5 | mTLS inside the farm, plain server TLS for buck2 and CI | unchanged listeners; REAPI keeps server TLS only; client API tokens stay later | 7 |
| 6 | Updates only through the frontend's updater | the updater updates the kbf release; apt updates only the package's root-owned files; the boundary is a table | 3.5 |
| 7 | Our own infra (the private storage network, k3s, the object store) stays separate one-shots | not product code; stays out of this repository | 9 |

Also binding: crun only (no Podman), and no repeated shell tooling: every step a script did on a host is Rust in this repository with a hermetic test (section 9).

The shape, in ten lines:

1. An operator runs `kbf token create --role storage --role daemon --uses 6 --expires 24h` on the frontend host. The token is printed once; the state store keeps only `SHA-256(secret)`. The token string also carries the SHA-256 of the farm CA certificate (the pin).
2. The operator installs the package on a node, writes two values in `/etc/kbf/kbf.toml` (the frontend URL, the token file) and starts `kbf-node`.
3. `kbf node` generates one ECDSA P-256 key per role it will hold, in the node's own state directory. The key never leaves the node.
4. It dials the frontend's enrollment listener, checks the server against the **pin from the token** (not against any public CA), and sends the token secret plus a CSR per role.
5. The frontend verifies the token (hash, expiry, uses, role), validates the CSRs (the proof of possession and the key type; **nothing else in a CSR is copied**), builds the certificate itself (node id, role URI, names, key usage), writes the registry row and the use count in **one compare-and-set transaction**, and only then returns the certificate, the CA bundle and the node's configuration.
6. The node writes the files; `kbf-tls` already reloads certificate files while a process runs (#873), so no restart is needed.
7. The certificate lives 7 days. At half its life the node asks the frontend to renew it: a request signed by the key the registry holds for that node. No token, no old certificate is needed, so an outage cannot lock a node out by expiry.
8. Revocation writes deny rows (node id, serial, key hash) in the same transaction as the registry change; they ride in the map, which already carries a `denied` field.
9. Updates arrive over the session the node holds, applied by the root helper of the pull updater, not by the package.
10. Everything the old host scripts did on a product host is now a Rust verb with a test, run by the package's maintainer scripts or by `kbf node`.

## 2. What exists, and what does not

### 2.1 Facts this design builds on

- **The role is in the certificate.** One URI subjectAltName `urn:kbf:role:<frontend|storage|daemon>`, read in one place (`kbf_peer::role_of`) **[F]**. The worker listener has a table of roles per gRPC method (`kbf-server/src/worker_roles.rs`, #821); storage has the same per endpoint (`kbf-storage` `service/admission.rs`) **[F]**.
- **The daemon's identity is its certificate.** Exactly one DNS subjectAltName, equal byte for byte to the `node_id` of every `Hello`; the common name is not read (`kbf-server/src/identity.rs`) **[F]**.
- **The deny list is a file today.** `--worker-deny-list`, re-read at every check, with three entry kinds (certificate serial, SHA-256 of the SubjectPublicKeyInfo, node id), one canonical spelling (`kbf_map::DenyEntry`) **[F]**.
- **The map has a `denied` field and nothing writes it.** `Map::new` takes `denied`, `kbf-proto` carries it, and `Members::refresh` copies the previous map's list forward (`members.rs`); no code adds an entry **[F]**. Storage nodes do not enforce it: `admission.rs` says "planned, phase P7" **[F]**. Section 8.3 is the work to close this.
- **Membership trusts a role, not a name.** `MembershipService::admit_leaf` takes any certificate with the storage role, and `Registrant.instance_id` is whatever the caller sends **[F]**. A storage certificate of node A can therefore register as instance B. The enrollment work binds them (section 8.1).
- **Certificates reload in place.** `kbf-tls` re-reads certificate, key and client CA every `--tls-reload-interval-secs` (default 30) and applies only a working set **[F]**. A client-side identity (the daemon's and the frontend's calls out) is not known to reload **[V]**: B4 verifies it and, if a channel keeps its first identity, rebuilds the channel on renewal.
- **The frontend has three listeners:** REAPI (server TLS with a certificate the farm CA does not issue, today a `tailscale cert` pair), worker (mutual TLS, farm CA) and the operator API (loopback only, bearer token for writes) **[F]**.
- **The state store** is `kbf-state` (compare-and-set rows, counters, time-to-live keys, an injectable clock); SQLite behind a single frontend **[F]**. Storage nodes never talk to it **[F]**.
- **Host layout.** `/usr/local/kbf/releases/<commit>/`, `current`, `previous`, state in `/var/lib/kbf-deploy`, offline-verification trust in `/usr/local/libexec/kbf-deploy` (`kbf-installer` on `feature/pull-updates`) **[F]**. The paths are constants of that crate.
- **A node's units** are rendered from templates (`kbf-deploy linux render-units --mode`) and checked by rules (`check-units`), with the `storage` and `daemon` templates still carrying `TODO(registration)` **[F]**.
- **The container driver** can run `crun` directly (`--container-runtime=oci`) beside Podman **[F]**; the Podman backend is being deleted (#936 and its follow-up).
- **The existing certificate script, in its v2 mode** (outside this repository) issues 15 certificates from the existing CA by a table: `frontend-worker` (frontend; serverAuth, clientAuth), `storage-<node>` (storage; both), `daemon-<node>` and `daemon-<mac>` (daemon; clientAuth), each with exactly one role, daemon names equal to the node id **[F]**.

### 2.2 What the host scripts do today (the work this replaces for a product install)

The four existing host-prep scripts (outside this repository), in order: **1** prepares the build host (kit copy, CA, certificates for every node, copy to nodes). **2** sets one node (stop old units, sudoers, the private storage network, a hosts line, the `kbf` user with subordinate ids and lingering, certificates in `/etc/kbf`, `crun` and `uidmap`, the dev installer). **3** is two cluster writes (a worker label, an object-store NodePort). **4** sets the frontend box (the `kbf-server` account, the worker certificate, a `tailscale cert`, a renew timer, the dev installer). What a **product** install needs from that list is only: the user, subordinate ids, lingering, `crun`, `uidmap`, the units, the first release, and certificates. Everything else is our infrastructure (section 9).

### 2.3 Where older prose and code disagree

- architecture-v2.md says the deny entries "ride in the map, so a denied daemon loses storage access when the generation turns". The map carries the field; nothing populates it and nothing on storage reads it (above). It is a plan, not a fact.
- architecture-v2.md says "Holding a valid storage-role certificate gets a node a member entry". True of the code, and too weak: any storage-role certificate registers under any `instance_id`.
- pull-updates.md says the helper, units, sudoers entry and pinned trust material "change only by a script a human runs". With a package they change by the operating system's package manager. The owner decided that split (section 12, item 1); section 3.5 draws the boundary.
- `kbf-modes.md` says `storage` and `daemon` do not register yet. `kbf node` (section 4) is the registered form of both; the five existing modes stay for the infra one-shots and tests.
- The target says roles include "client". There is no client role: `PeerRole` is `frontend`, `storage`, `daemon`, and buck2 or CI present no certificate. This document keeps three (section 6.3).

## 3. Packages

### 3.1 What ships

| Artifact | For | Contents |
|---|---|---|
| `kbf_<version>_<arch>.deb` | Linux node and, with the next package, the frontend | the first release (`kbf`, `kbf-deploy`, the root helper), node units, tmpfiles and sysusers, the pinned `gh` and trusted root for offline verification, maintainer scripts that are a small Rust executable (no shell) |
| `kbf-frontend_<version>_<arch>.deb` | the frontend host | `kbf-frontend@.service` (the two-instance template of `kbf-modes.md`), the frontend account and directories; `Depends: kbf (= same version)` |
| `kbf-<version>-macos-arm64.pkg`, and a Homebrew cask that installs it | Macs | `kbf-daemon`, the root helper, the LaunchDaemon plist, the trust directory |
| `ghcr.io/<owner>/kbf-frontend` (name decided at publish) | the frontend where an orchestrator runs it | the `kbf` binary, entrypoint `kbf frontend` with flags; no updater inside |

**Version.** `0.<yyyymmdd>.<run>+g<commit12>`; the full 40-character commit is in the package's `manifest.json`, because a release directory is named by it. The package is built from the **same tarball and attestation** the artifacts workflow already publishes **[F]** (a release is verified, not rebuilt); the build job downloads the attested `kbf-<commit>-<platform>.tar.gz`, checks it, and wraps it. It is built and attested on pushes to `main` and `feature/arch-v2` only, like the tarballs.

**Why a `.pkg` under a cask for the Mac [D].** The Mac daemon is a **root LaunchDaemon** (macos-end-state.md); a Homebrew formula installs as the user and cannot place `/Library/LaunchDaemons` or root-owned `0755` release directories, and `brew services` for a system daemon needs `sudo` by hand. A `.pkg` does it in one installer run; the cask is the `brew install --cask kbf` front door and `uninstall launchctl:` and `pkgutil:` stanzas give removal. The `.pkg` is signed with the farm identity (the same identity that signs `kbf-daemon`, so a Full Disk Access grant survives). A plain formula remains possible for a developer's non-root `kbf` and is not a product path.

### 3.2 Where each platform puts files

| Path (Linux) | Owner | Owner of the content | Purpose |
|---|---|---|---|
| `/usr/lib/kbf/release/` | root | dpkg | the first release as shipped (`kbf`, `kbf-deploy`, `kbf-helper`, `units/`, `manifest.json`), never modified |
| `/usr/lib/kbf/libexec/` | root | dpkg | the pinned `gh`, `trusted_root.jsonl`, default `installer.conf` |
| `/usr/local/kbf/releases/<commit>/`, `current`, `previous` | root | the updater and `kbf-installer` | the layout of pull-updates.md section 7 |
| `/usr/local/libexec/kbf-deploy/` | root | `kbf-deploy package postinst` (copies from `/usr/lib/kbf/libexec`) | what offline verification trusts; the installer's constants name this path |
| `/lib/systemd/system/kbf-node.service`, `kbf.slice` | root | dpkg | units; `ExecStart` is `/usr/local/kbf/current/kbf node --config /etc/kbf/kbf.toml` |
| `/etc/kbf/kbf.toml` | root:kbf `0640` | the operator | the node's config (section 4) |
| `/var/lib/kbf/enroll.token` | kbf `0600` | the operator | the token file, in the node's own directory so the unprivileged node can read it and **unlink it** after a successful enrollment (a file in root-owned `/etc/kbf` could be neither removed nor, for a `0600` root file, read by `kbf`) |
| (no cold-store key file) | | | The cold-store keys are never a file on a node: a storage node receives them from the frontend over its mTLS session, holds them in memory only, and fetches them again at each start (section 4.3) |
| `/var/lib/kbf/pki/` | kbf `0700` | the node | `ca.pem`, `<role>.key` (`0600`), `<role>.pem`, `node.json` (ids, serials, config generation) |
| `/var/lib/kbf/{hot,meta,data}` | kbf `0700` | the node | hot store, metadata log, daemon data |
| `/var/lib/kbf-deploy/` | root | the installer | state, inbox, `state.json` |

The frontend package adds the account `kbf-frontend` (its keys and database must not be readable by the account builds run as, #819), `/var/lib/kbf-frontend/{state.db,ca/}` (`0700`) and `/etc/kbf/frontend.token` (the operator API's bearer token, as `api-token` today).

macOS: the same layout with the platform differences of host-releases.md (`/var/db/kbf-deploy`), config in `/usr/local/etc/kbf/kbf.toml`, token at `/var/db/kbf/enroll.token` (`root:wheel 0600`; the daemon is root and can unlink it; the operator delivers it by `install -m 0600` over a management session or by a file push from the device-management system, never inside the `.pkg`). The Mac token is a **one-use token bound to the Mac's `node_id`**, so a copy lifted from the delivery channel is worthless after the install; an administrator of the device-management system can read what it pushes, which is the accepted exposure and is why one use matters. CI signs the package ad hoc, so the real signed path is first exercised in the cutover rehearsal (section 10.3), PKI in `/var/db/kbf/pki` (`root:wheel`, key `0600`, as macos-end-state.md decides for the node key), plist `/Library/LaunchDaemons/<label>.plist` running `/usr/local/kbf/current/kbf-daemon node --config ...`, logs in `/Library/Logs/kbf`. The label is the profile's `launchd_label` **[F]**, defaulting for the package to `com.komira.kbf-daemon`.

### 3.3 The maintainer scripts are Rust

`postinst`, `prerm` and `postrm` are **the executable itself**, not shell: dpkg runs any executable file, so each is a copy of one small Rust program (`kbf-package-hook`, linked statically so it runs before any other package is configured) that dispatches on its own file name. No shell logic exists to budget, and the interpreter budget gains no rows. The verbs (also reachable as `kbf-deploy package <verb>`), each idempotent, each `DRY-RUN` unless run by dpkg (they use `kbf_host::Plan`, like every kit **[F]**):

| Verb | Does | Replaces |
|---|---|---|
| `package postinst` | create the `kbf` system user and group (home `/var/lib/kbf`, no login); give it a subordinate uid and gid block of 65536 ids that overlaps no existing range (`/etc/subuid`, `/etc/subgid` read, the block chosen, the file changed through `usermod`); enable lingering (`loginctl enable-linger`, or create `/var/lib/systemd/linger/kbf` when no systemd runs, as in a container build); check `crun` and `newuidmap` run; install the trust files; install the first release; reload systemd; enable `kbf-node` only when `kbf.toml` and the token file exist | steps 7 and 8 of script `02`, the `SUBUID_RANGE` bookkeeping, the dev installer |
| `package prerm` | stop `kbf-node` gracefully (it drains on SIGTERM; `TimeoutStopSec=infinity` stays) on remove, not on upgrade | hand `systemctl` |
| `package postrm` | on `remove` keep config, keys and data; on `purge` remove `/etc/kbf`, `/var/lib/kbf/pki` and the user, **not** the hot and meta directories unless `--purge-data` was given to the verb first | n/a |

The subordinate-id rule is the one place the shell scripts carried state (a range chosen by hand to avoid a neighbour's). The Rust verb reads both files, takes the first free block at or above `200000` that overlaps none, refuses (exit 2, naming the clash) rather than widening a smaller existing range, and a test plants a neighbouring range (section 10.2).

**Dependencies.** `Depends: crun, uidmap, systemd, ca-certificates`. `Conflicts:` nothing; `Podman` is neither depended on nor installed, and the package's smoke test (section 10) fails if `podman` is present in the resolved closure. The container driver runs `--container-runtime=oci` by default in node units; the `podman` backend is deleted by the crun-only work, and B5 does not build against it.

### 3.4 The frontend image

`FROM` a base image pinned by digest, `COPY` the attested `kbf` binary, `ENTRYPOINT ["/usr/local/bin/kbf", "frontend"]`, a non-root numeric user, volumes `/var/lib/kbf-frontend` (state database and CA directory) and `/etc/kbf`. Configuration is flags in the orchestrator's manifest, as everywhere. The image runs no updater: an orchestrator replaces the image, which for a frontend behind a load balancer is the cloud shape of architecture-v2.md. On our own frontend host the `.deb` and the Envoy handover (split-mode-updates.md) stay the update path **[D]**. A frontend in a container still needs a durable volume for `ca/`; losing it is the failure of section 6.5.

### 3.5 How the package and the updater share a host

**Decided (owner): apt updates the package's root-owned files; the pull updater updates the kbf release.** The boundary:

| | apt (the package) owns | the pull updater owns |
|---|---|---|
| Files | the root helper (`kbf-helper`), `kbf-deploy`, the systemd units and `kbf.slice`, `kbf-package-hook` (the maintainer executables), sysusers and tmpfiles, the offline-verification trust files (`gh`, trusted root, `installer.conf`), the shipped first release under `/usr/lib/kbf` | `/usr/local/kbf/releases/<commit>/`, `current`, `previous`, `state.json`: the `kbf` / `kbf-daemon` binary that runs |
| Changes when | the operator runs `apt upgrade` (`unattended-upgrades` only if the operator enables it; the package does not) | the frontend offers a desired release and the helper applies it |
| Never | flips or removes `current` or `previous` after the first install; changes a release directory | touches a package-owned file; changes the helper, units or trust material (pull-updates.md rule 9) |
| Visible as | the package version, reported by the node in `NodeFacts.package_version` and shown by the UI as "package update available" when it is older than the frontend's | the running commit and `UpdateStatus` **[F]** |

The two meet at one file, the units' `ExecStart=<current>/kbf node`. A new package's units must run against the `current` binary of the previous release (the N and N-1 rule of split-mode-updates.md), and `check-units --mode node` refuses a unit that names a flag `kbf check-args` of the shipped binary rejects; a unit that needs a newer flag waits until the updater moves `current`.

1. `package postinst` installs the shipped release into `releases/<commit>/` through the `kbf-installer` library (the `install` verb: refuse if the directory exists with other contents, copy, `0755` root). It flips `current` **only when `current` does not exist** (a first install). On an upgrade of the package it never touches `current` or `previous`.
2. From the first start the node's `UpdateOffer` stream and root helper (pull-updates.md) own `current`. The package's release is, at most, one of the `previous` or retained directories.
3. Root code changes only through the operating system, per the table: the pulled tarball carries only the unprivileged binary.
4. `dpkg -r kbf` while `current` points elsewhere leaves `/usr/local/kbf` in place (dpkg does not own it); `package postrm purge` removes the releases only if `--purge-data` is given.
5. A package older than `current` is installed beside it, never activated. A downgrade of the running release is a desired-generation change in the frontend, not a package act.
6. The frontend host: `kbf-frontend` installs the release the same way and the **helper must not run on the frontend host** until the frontend-host `activate` of split-mode-updates.md exists **[F: pull-updates.md section 7]**; the package ships the helper disabled there.

## 4. `kbf.toml`: the node's config

One file, TOML, closed key set. A mode of the `kbf` binary reads it with `--config FILE`; every flag it also has overrides the file for that run (tests and the infra one-shots use flags only). The file holds **no secret**: the token is a path.

```toml
[frontend]
url = "https://build.example.com:8990"      # required

[enroll]
token_file = "/var/lib/kbf/enroll.token"     # default shown; required until the node is enrolled

[node]
id = "node-02"                              # default: the short hostname, lower case
advertise = "node-02.farm.example.com:8983" # storage role only; default: <id>:<storage port from the frontend>
zone = "rack-a"                             # optional label

[dirs]
state = "/var/lib/kbf"                      # default shown; pki/, hot/, meta/, data/ live under it

[storage]                                   # optional, overrides of node-local paths
hot_dir = "/var/lib/kbf/hot"
meta_dir = "/var/lib/kbf/meta"

[daemon]                                    # optional
data_dir = "/var/lib/kbf/data"
reserve_cpus = 2
```

### 4.1 Every field and its validation

Unknown keys, repeated keys (TOML itself refuses duplicates), a wrong type and an out-of-range value are errors that name the key and the line. Parsing is `serde` with `deny_unknown_fields` into one struct; `Config::validate` runs after and reports **every** problem, not the first. There is no environment expansion, no `~`, no include.

| Key | Type | Validation | Default |
|---|---|---|---|
| `frontend.url` | string | scheme exactly `https`; a host (DNS name, or an IP only with `tls.trust = "pin"`); an optional port 1 to 65535; **no** userinfo, path, query or fragment; at most 253 bytes; the host is lower-cased | required |
| `enroll.token_file` | absolute path | must exist while the node is unenrolled; a regular file, not a symlink, owned by the service user (root on macOS), mode `0600` or `0400`, at most 4096 bytes, one line (the same checks `token.rs` applies to the API token **[F]**); ignored once `node.json` says enrolled | `/var/lib/kbf/enroll.token` |
| `node.id` | string | `^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$`; becomes the certificate's one DNS name, the member `instance_id` and the `Hello` `node_id` | the short hostname, lower-cased; refused if it fails the pattern (a host with `_` in its name must set it) |
| `node.advertise` | `host:port` | storage only; host is a DNS name or IP literal; not loopback, unspecified or link-local; not a name of the frontend; port 1 to 65535; no wildcard | `<node.id>:<storage port>` |
| `node.zone` | string | at most 63 bytes of `[a-z0-9._-]` | empty |
| `dirs.state` | absolute path | no `..`, no symlink in the path; the service user owns it, mode `0700` | `/var/lib/kbf` (`/var/db/kbf` on macOS) |
| `storage.hot_dir`, `storage.meta_dir` | absolute path | as `dirs.state`; `hot_dir` on the same filesystem as `daemon.data_dir` (one cache) **[F: kbf-modes.md]** | `<state>/hot`, `<state>/meta` |
| `daemon.data_dir` | absolute path | as above | `<state>/data` |
| `daemon.reserve_cpus` | integer | 0 to the node's CPU count minus 1 | kbf's default |
| `tls.trust` | `"pin"` or `"public"` | `"pin"`: the enroll listener must present a chain containing a certificate whose SHA-256 is the token's pin. `"public"`: the system roots verify the name and the token's pin must still match the CA bundle in the answer (section 5.4) | `"pin"` (the default; `"public"` must be written down on purpose) |

Cross-field checks: a `[storage]` or `[daemon]` table the frontend does not grant a role for is a warning in the log, not an error (the roles come from the token); `node.advertise` set on a node granted only `daemon` is a warning.

### 4.2 "The frontend URL is one value"

`frontend.url` is the only place a node learns the frontend. Everything else the node needs is **pulled**: the enroll answer and every renew answer carry a `NodeConfig` (below) whose endpoints default to the URL's host with the frontend's configured ports. Changing the farm's public name is two edits and no node edit: the frontend gets a certificate for the new name (a new entry in its `--frontend-name` list; the CA does not change, so the pin does not change) and DNS points both names at it. The nodes may then be moved to the new name by editing the one line, or, with the open decision of section 12 (item 9), by the frontend sending `NodeConfig.frontend_url`, which a node adopts into `node.json` only after reaching it under the same pin and keeps the `kbf.toml` value as the fallback.

### 4.3 What is pulled from the frontend

```proto
enum CredentialMode { CREDENTIAL_MODE_UNSPECIFIED = 0; KEYS = 1; AMBIENT = 2; }

message NodeConfig {
  uint64 generation = 1;             // strictly increases at every change
  repeated string roles = 2;         // granted: "storage", "daemon"; decides the mode
  string worker_endpoint = 3;        // https URL the daemon dials (--server)
  string cas_endpoint = 4;           // what --cas is today for a daemon-only node
  uint32 storage_port = 5;
  string cold_store = 6;             // s3://bucket/prefix/?endpoint=... , never a key
  CredentialMode credential_mode = 10;  // KEYS (fetch them, 4.3) or AMBIENT (the node's own cloud identity)
  uint32 min_protocol = 7;
  uint32 tls_reload_interval_s = 8;  // 30
  string frontend_url = 9;           // optional, section 4.2
}
```

The mode `kbf node` runs is a function of `roles`: `{daemon}` is `daemon`, `{storage}` is `storage`, both is `storage-and-daemon`. A change of `roles` or an endpoint is applied at the next restart of the node (the frontend shows "restart pending" from the node's reported generation); `kbf node` never re-executes itself.

**The object store is frontend configuration (decided).** This is a private-cloud product, so the endpoint, the bucket, the prefix and the keys belong to the frontend, not to each node. The frontend takes `--object-store-endpoint`, `--object-store-bucket`, `--object-store-prefix`, `--object-store-region` and one of two credential choices:

- `--object-store-keys-file PATH`: an access key and secret in a file the frontend account owns (`0600`, checked like the API token file **[F]**), outside the state store, its backups and every release; or
- `--object-store-credentials=ambient`: **no keys at all.** The node uses its own cloud identity (an instance role or workload identity), and the frontend only tells it so (`credential_mode = AMBIENT`). Whether `kbf-objstore` resolves the platform's default credential chain is **[V]**; B4b proves it or adds it.

`NodeConfig` carries the endpoint, bucket, prefix and the mode, **never a key**. In `KEYS` mode, once a storage node has enrolled, it calls a new RPC on its **mTLS session** to the frontend's worker listener:

```proto
service ColdStore { rpc GetKeys(GetKeysRequest) returns (GetKeysResponse); }   // role table: storage only
message GetKeysRequest {}
message GetKeysResponse { string access_key_id = 1; string secret_access_key = 2; }
```

The service sits behind the worker listener's role table (storage role only, a row in `worker_roles.rs`), is bound to the certificate's node (the registry row must be `active` and not denied), and is refused to a revoked node at once, so revocation also ends cold-store access for any node that restarts. On the node the keys are held **in memory only**: handed to the object-store client in-process (not through the environment, an argument list or a file), never logged (a `Debug` that prints `Keys(..)`), never in `NodeConfig`, never on disk, with core dumps disabled for the process (`RLIMIT_CORE` 0, not dumpable). They are **fetched again at every start** and again when the object store refuses them (so replacing the frontend's keys file rotates them with no node action). A storage node that cannot reach the frontend at start serves its hot tier and retries; cold-tier calls fail until the keys arrive, the same behaviour as a cold store that is down **[F: architecture-v2.md, failure modes]**. A `daemon`-only node never calls it and the role table would refuse it.

The per-node `s3.env` file and its `EnvironmentFile=` line go away when this ships; `check-units --mode node` then **refuses** a node unit that carries one. A short-lived or scoped credential (a prefix-limited key the object store mints) is a later option, not v1. Until the RPC ships (B3b serves it, B4a/B4b fetch it), a storage node keeps using the existing root-owned file, and a token that grants `storage` must carry an exact `node_id` so it cannot be spent by an arbitrary host.

### 4.4 Why `kbf node` and not a rendered unit per role

A unit rendered after enrollment would need a root step when the role becomes known, which breaks "the node is unprivileged and the frontend decides". `kbf node` is one unit for every non-frontend role: `ExecStartPre=kbf enroll` (idempotent: exits 0 at once when the certificates are good) then `ExecStart=kbf node`, which builds the flags of the mode from `kbf.toml` and `NodeConfig` and runs it in-process. The existing five modes and their units stay (the frontend's template, the infra one-shots, rehearsals). `Mode::Node` is added to `kbf-cli/src/names.rs` and `kbf-deploy`'s `Mode`, which a kbf-cli test already holds equal **[F]**.

## 5. The enrollment protocol

### 5.1 Tokens

**Format.** One line, ASCII:

```text
kbft1.<id>.<secret>.<pin>
```

- `kbft1`: the format version.
- `id`: 12 characters of lower-case base32 (60 random bits). It is the lookup key and is not secret.
- `secret`: 32 bytes from the operating system's CSPRNG (`ring::rand::SystemRandom`), base64url without padding (43 characters). **256 bits of entropy.**
- `pin`: SHA-256 of the DER of the **root CA certificate**, base64url without padding (43 characters). It is public information that the node needs for first contact (section 5.4).

A malformed token is refused by the node before any network call, naming which part is wrong (never echoing the secret).

**At rest.** The state store keeps, under `enroll/token/<id>`:

| Field | Meaning |
|---|---|
| `secret_sha256` | `SHA-256(secret bytes)`. A fast hash is right here: the secret is 256 random bits, there is nothing to brute-force, and a slow KDF would only make an unauthenticated listener easier to exhaust |
| `roles` | a non-empty subset of `{storage, daemon}`. `frontend` is never grantable by token (section 6.3) |
| `max_uses` | 1 to 1000; 1 is the default |
| `uses` | consumed so far; changes only inside the enrollment transaction |
| `expires_at` | store clock; at most 30 days after creation, default 24 hours |
| `node_id` | optional exact node id the token may enroll (a one-node, one-use token for a Mac, say) |
| `node_prefix` | optional prefix every enrolling node id must carry |
| `names` | optional list of allowed `advertise` hosts or suffixes |
| `replace` | whether the token may enroll a node id that already exists (a reprovision), default false |
| `label`, `created_at`, `created_by` | for the operator; `created_by` is the API caller's label |
| `revoked_at` | set by `kbf token revoke` |

**Shown once.** `POST /v1/enroll/tokens` returns the full token string once, in the response body; no later call (`GET`, `list`, the UI) returns the secret or the pin-bearing string, and the log never holds it (the type has a `Debug` that prints `Token(..)` like `ApiToken` **[F]**). The CLI writes it to stdout, or with `--out FILE` to a `0600` file it creates exclusively.

**Comparison.** Lookup by `id`; compute `SHA-256(presented secret)`; compare with `subtle::ConstantTimeEq` against the row, or against a fixed dummy hash when the id is unknown, so an unknown id and a wrong secret take the same work and give the same answer (`UNAUTHENTICATED`, message `enrollment refused`). Only after the secret is right does the frontend say why a token cannot be used (`expired`, `used up`, `revoked`, `role not granted`); a caller who knows the secret may be told. This is the one deliberate asymmetry and it leaks nothing to a guesser.

**Operator interface (B2).**

| Call | Does |
|---|---|
| `POST /v1/enroll/tokens` | create; body `{roles, uses, expires_in_s, node_id?, node_prefix?, names?, replace?, label}` |
| `GET /v1/enroll/tokens` | list: id, label, roles, uses of max, expires, revoked; never the secret |
| `DELETE /v1/enroll/tokens/{id}` | revoke (sets `revoked_at`; nodes already enrolled stay enrolled) |
| `GET /v1/enroll/nodes`, `POST /v1/enroll/nodes/{id}/revoke` | the registry (section 5.7) |

All writes need the existing bearer token gate (loopback only, `403` otherwise **[F]**), so `kbf token ...` runs on the frontend host, like `kbf cluster drain`. The operator API's own roles are planned **[F: api.md]**; until they exist, whoever holds the API token can mint enrollment tokens, which is exactly as powerful as being able to drain the frontend today and must be said in the operator documentation. The page in the UI is a follow-up (unit B2b).

### 5.2 The listener

`--enroll-listen ADDR` (off unless a CA exists), TLS from files like the other listeners, **server authentication only**, serving exactly one gRPC service, `kbf.enroll.v1.Enroll`, and nothing else (a method table with two rows, and the same "a method not in the table is refused" rule as `worker_roles.rs`). Reasons for a listener of its own rather than reusing one:

- the worker listener demands a client certificate (`WebPkiClientVerifier`); making it optional would turn every worker method into "reachable by anyone who completes TLS", and the role table would be the only guard;
- the REAPI listener is the public name for buck2 and CI, with a certificate the farm CA does not issue; enrollment behind it would tie the trust of the first contact to a public CA;
- the operator API is loopback by design.

Its certificate is the frontend's own `frontend-worker` certificate (issued by the CA, section 6.2) with the **root appended to the chain** it sends, so a node holding only the pin can verify it (section 5.4). After a rotation (6.4) the chain sent is `leaf, new root cross-signed by the old root, old root`, so a token minted before the rotation still verifies. The node also checks that the leaf carries the `frontend` role URI and `serverAuth`, so a storage or daemon certificate, which any enrolled node holds, cannot impersonate the enrollment server. Limits: connection and request rates per source address and in total, a request body of at most 16 KiB, a handshake deadline, and a concurrency cap. Behind an L7 load balancer that terminates TLS the pin cannot be checked on the wire; that is `tls.trust = "public"` (5.4). Behind Envoy during a frontend update, this listener is an L4 pass-through; its traffic is a few requests per node per half-life **[I]**. Behind an L4 proxy every connection arrives from the proxy, so per-source limits would see one source: `--enroll-proxy-protocol` accepts PROXY protocol v2 and takes the source address from it, only from peers listed in `--enroll-proxy-peer` (a connection from anyone else that sends the header is dropped), and a test proves a spoofed header from an untrusted peer changes nothing. **One active signer.** With two instances of the frontend alive (the Envoy handover) or several frontends, exactly one process signs: a row `enroll/signer` holds `{holder, epoch}` by compare-and-set, taken at start and handed over with the state writer baton of the handover; a process without it answers `Enroll` and `Renew` with `UNAVAILABLE` and a retry hint, and the node retries (its request is idempotent). Serials are random, so a split brain could not collide, but two signers would double the CA key's exposure and break the single audit order; the test kills the holder and checks the other takes over and no request is signed twice.

### 5.3 Messages

```proto
// crates/kbf-proto/proto/kbf/enroll/v1/enroll.proto
service Enroll {
  rpc Enroll(EnrollRequest) returns (EnrollResponse);
  rpc Renew(RenewRequest) returns (RenewResponse);
}

message RoleCsr { string role = 1; bytes csr_der = 2; }   // PKCS#10, at most 2 KiB

message EnrollRequest {
  string token_id = 1;
  bytes  token_secret = 2;          // 32 bytes
  string node_id = 3;
  string advertise = 4;             // storage only, "host:port"
  string zone = 5;
  repeated RoleCsr csrs = 6;        // one per requested role, each with its own key
  NodeFacts facts = 7;              // os, arch, running commit, protocol: informational
}

message NodeFacts { string os = 1; string arch = 2; string commit = 3; uint32 protocol = 4; string package_version = 5; }

message IssuedCert {
  string role = 1;
  bytes  leaf_der = 2;
  string serial = 3;                // lower-case hex, the deny list's spelling
  int64  not_before = 4;            // unix seconds
  int64  not_after = 5;
}

message EnrollResponse {
  repeated IssuedCert certs = 1;
  bytes  ca_bundle_pem = 2;         // every CA cert to trust, current first
  NodeConfig config = 3;
  int64  server_time = 4;           // unix seconds, for the skew check
  int64  renew_after = 5;           // unix seconds: when the node should first try
}

message RenewRequest {
  string node_id = 1;
  repeated RoleCsr csrs = 2;        // same keys as the registry holds; self-signed
  int64  issued_at = 3;             // unix seconds; also inside the CSR's signed attributes
  uint64 config_generation = 4;     // what the node holds
}

message RenewResponse {
  repeated IssuedCert certs = 1;
  bytes  ca_bundle_pem = 2;
  NodeConfig config = 3;            // present only when the generation differs
  int64  server_time = 4;
  int64  renew_after = 5;
}
```

Status codes: `INVALID_ARGUMENT` for a malformed request (named field); `UNAUTHENTICATED` for the token (5.1); `PERMISSION_DENIED` for a role the token lacks or a revoked node; `ALREADY_EXISTS` for a node id in use without `replace`; `RESOURCE_EXHAUSTED` for a rate limit (with a retry-after); `UNAVAILABLE` when the state store is down (the node retries). The same ones are used for `Renew`.

### 5.4 First-contact trust: the lean

The node knows two things at first contact: the URL, and the token, which contains the pin. The options:

| Option | Trust anchor | Costs |
|---|---|---|
| **A. Public certificate of the frontend name** | system roots | a rogue or compromised public CA, or a captive TLS-inspecting proxy, sees the token secret and can hand the node a bogus CA bundle; the node has nothing to compare it with; the farm's name today carries a publicly issued certificate |
| **B. Pinned CA hash in `kbf.toml`** | a value the operator must copy | a third thing to configure; breaks "URL plus token file" |
| **C. Pin carried inside the token (lean) [D]** | the operator's own copy-and-paste of one string | none for the operator; the pin is minted with the token by the frontend that holds the CA |

**Decided (owner): C, with A as a declared fallback.** The token is already a thing the operator moves out of band (it must be: it is the credential). Putting the CA fingerprint in it costs 43 characters and removes the whole class of first-contact attacks without adding a field to `kbf.toml`. The node then:

1. dials the URL; in `trust = "pin"` mode verifies the server's chain with a custom `rustls` verifier: some certificate in the presented chain has SHA-256 equal to the pin, and the leaf chains to it, validly, for the URL's host (hostname and validity checked as `webpki` does), key usage serverAuth;
2. sends the token secret only over that connection;
3. checks that every certificate the answer delivers chains to a CA whose hash is the pin or to a CA that the answer's bundle shows is **signed by the pinned one** (section 6.4, rotation), and that `ca_bundle_pem` contains the pinned root; otherwise it discards the answer and writes nothing.

In `trust = "public"` mode (a cloud frontend behind a load balancer) step 1 is the system roots; steps 2 and 3 are unchanged, which still defeats a forged CA bundle, but an active interceptor with a publicly trusted certificate for the name can read the token secret and enroll its own key. That residual is stated in the operator guide, and the token's `uses` and `expires_at` bound it. A farm that cares runs `pin`.

Decided: the online CA is a single self-signed root with `pathlen 0`; there is no intermediate tier in v1 (the signer process of 6.1 isolates the key instead, and a rotation replaces the root, 6.4). Rejected: trusting the server on first use (accepts whatever the first connection says), and a second out-of-band file for the pin.

### 5.5 The CSR and what the frontend builds

A CSR is the node's carrier for a public key and its proof of possession. The frontend takes **nothing but the public key** from it:

1. DER, at most 2 KiB; parses as PKCS#10 version 0; its self-signature verifies under its own public key (proof of possession);
2. the key is ECDSA on P-256 (the one algorithm, `ECDSA_P256_SHA256`; RSA, P-384, Ed25519 are refused so that the verifier set is one and the registry holds one key type);
3. subject, requested extensions and attributes are **ignored** (a CSR asking for a different role, `CA:TRUE`, a frontend name, a wildcard or another node's name gets none of it, and a test proves it);
4. the role of each CSR is the `role` field of `RoleCsr`, which must be in the token's `roles` and unique in the request.

The frontend then builds the certificate (via `kbf-ca`, section 6.2):

| Field | Value |
|---|---|
| version, signature | X.509 v3, ECDSA P-256 with SHA-256 |
| serial | 127 random bits from the CSPRNG, positive; the lower-case hex without leading zeros is the deny list's spelling **[F]** |
| subject | CN = node id (never read by kbf) |
| `notBefore` | now minus 5 minutes (clock slack) |
| `notAfter` | `notBefore` + lifetime; default **7 days** [owner], 1 hour to 30 days by flag |
| basicConstraints | `CA:FALSE`, critical |
| keyUsage | `digitalSignature` (critical) |
| extendedKeyUsage | `daemon`: clientAuth. `storage`: serverAuth and clientAuth |
| subjectAltName | URI `urn:kbf:role:<role>` (exactly one); `daemon`: DNS = node id (exactly one); `storage`: DNS = node id, and DNS or IP = the advertise host if different |
| authorityKeyIdentifier, subjectKeyIdentifier | set |

These are the rows `kbf-deploy linux verify-cert` checks today **[F]**; the CA's tests run `verify-cert`'s checks over every issued certificate so the two cannot drift.

**Names.** The `advertise` host for a storage node must satisfy the token's `names` policy if any, must not equal or sit under any name the frontend serves (`--frontend-name`), must not be `localhost` or an address that is loopback, unspecified, link-local or multicast, and must not already be bound to another node's registry row. Without these a storage-token holder could ask for the frontend's name and be a man in the middle for every storage client.

### 5.6 The enroll transaction

```text
validate request shape                          (no state read)
look up token, check secret                     (read)
check expiry, revocation, uses < max, roles, node_id / prefix / names, replace
validate CSRs; build and sign the certificates  (pure: no state)
txn (compare-and-set on the token row's version, the node row's absence or version):
    token.uses += 1
    enroll/node/<node_id> = { roles, per-role {spki_sha256, serial, not_after, cert_der},
                              advertise, zone, token_id, state: active, created_at, facts }
    enroll/serial/<hex> = { node_id, role }             (one per certificate)
    enroll/req/<token_id>/<sha256 over the request's CSRs, in role order> = node_id   (idempotency)
    if replace: the previous row's serials and key hashes are written as deny rows
commit -> only then return the response
```

Properties:

- **No use is consumed without a registered node, and no node is registered without a use.** One transaction.
- **Last use under a race.** Two requests on the last use both read `uses = max - 1`; the compare-and-set on the token row lets one commit and the other retries, reads `uses = max` and is refused. A test fires them concurrently.
- **Idempotent replay does not recheck token revocation, on purpose.** The row and the certificate already exist; revoking a token stops new enrollments, and revoking the node (5.9) is what ends a node. The replay returns only the stored certificates while the node is active and they are unexpired.
- **A crash between commit and response** leaves a registered node and a use consumed. The node retries the same request (the same CSRs): the idempotency row maps `(token_id, csr hash)` to the node, and the frontend returns the stored certificates **without a second use** if the node is active and the certificates are unexpired. A replayed `(token, CSR)` therefore returns the same certificate to anyone holding both, which is harmless because the certificate is useless without the private key.
- **Signing before the transaction** keeps the store out of the CPU-bound step and means a failed transaction wastes a signature, not a serial: the serial is recorded only on commit.
- **The registry is the frontend's, in the state store**, so a second frontend later sees it (Postgres era). It is not the member table: `member/<id>` is liveness and placement (TTL, map), `enroll/node/<id>` is identity and authorization (no TTL).

### 5.7 The node registry

`enroll/node/<node_id>` as above; `enroll/serial/<hex>` and `enroll/key/<spki hash>` give reverse lookups for the operator and the deny writer. `GET /v1/enroll/nodes` lists node id, roles, state, certificate expiry, token label, advertise, last renewal, the config generation the node reports, and a "restart pending" flag; the existing `GET /v1/nodes` (live sessions) and `GET /v1/members` (#858) are joined to it in the UI by node id.

### 5.8 Renewal

- **When.** The node renews when half the certificate's life has passed (the answer's `renew_after`, with ±10% jitter), retries every 5 minutes with doubling to 1 hour on failure, and logs a warning with the remaining time below 24 hours. The metric `kbf_enroll_cert_not_after_seconds{role}` (beside `kbf_tls_leaf_not_after_seconds` **[F]**) lets the frontend's UI alert before expiry.
- **How.** `Renew` on the enroll listener. The request is a CSR per role signed by **the key the registry holds** for that node and role (key continuity), plus `issued_at`. The frontend checks: the node row exists and is `active`; each CSR's key hash equals the row's; `issued_at` is within 5 minutes of the store clock; the pair `(node, csr hash)` was not seen in that window (a small in-memory set plus the rate limit); the node did not renew within the last minute. It then issues a new certificate with a new serial, records it and the old serial's `superseded_at`, and answers.
- **No client certificate is needed**, so a node whose certificate expired during a frontend outage, a sleep or a clock jump recovers by itself, for as long as it is not revoked. A bound is kept: a node whose last certificate expired more than 30 days ago (flag) must re-enroll with a token, so a long-abandoned key does not stay renewable forever. The same property means a **stolen node key stays renewable until the node is revoked**; revocation (5.9) is the only remedy, and the operator guide says so.
- **A replayed `Renew`** returns a certificate for the node's own key; an attacker without the key learns nothing and gains nothing, and the rate limit bounds the rows it can add.
- **Key rotation** is not part of renew in the first version: a new key means a re-enrollment with a `replace` token, which denies the old key. (A renew carrying a second CSR signed by both keys is the obvious extension and is listed as follow-up.)
- **On the node.** Files are replaced in one step each (write beside, `fsync`, rename), certificate last, so `kbf-tls`'s "a pair renewed in two writes is a mismatch" case does not arise on the server side **[F: tls-renewal.md]**; the 30-second look applies it with no restart and no ended connection. The client side (the daemon dialing the worker and storage listeners, storage dialing peers) rebuilds its channel with the new identity on the next reconnect, and B4's test proves a daemon continues after a renewal without a restart **[V]**.

### 5.9 Revocation

`POST /v1/enroll/nodes/{id}/revoke` (`kbf node revoke <id>`), one transaction:

1. registry row `state = revoked`, with the time and the operator's label;
2. deny rows `deny/node/<id>`, and `deny/serial/<hex>` and `deny/spki-sha256/<hash>` for every live certificate and key of the node, in the canonical spelling of `kbf_map::DenyEntry` **[F]**;
3. the token the node came from is untouched (revoke a token separately if it is still dangerous).

Effects, with the work each needs (section 8.3):

| Where | When it takes effect | Needs |
|---|---|---|
| `Renew` and `Enroll` for that node | immediately | the registry check (B3) |
| Worker listener: the node's session and blob calls | at the node's next `Hello`, `Heartbeat` or `Result`, as the file deny list behaves today **[F]** | the worker deny source reads the store's `deny/` rows beside the file (B3c) |
| Storage nodes: calls from the revoked node, and the revoked node's storage role | when the generation of the map turns, within one heartbeat of the frontend's refresh | `Members::refresh` writes `deny/` rows into `Map.denied`; `kbf-storage` admission applies `Map.denied` (B3c) |
| Everything else | when the certificate expires (at most the lifetime, 7 days) | nothing |

The last row is why the lifetime is short: revocation is eventually complete without a CRL or OCSP, and immediate where it matters (new work and storage access). An un-revoke is a new enrollment.

### 5.10 Abuse and failure cases

| Case | Behaviour |
|---|---|
| expired token | `UNAUTHENTICATED` `token expired` (after the secret is verified); no use consumed |
| used-up token, sequential or concurrent | refused; exactly `max_uses` nodes registered; a test races `max_uses + 4` requests |
| wrong role: CSR for a role the token lacks; role `frontend`; unknown role; duplicate role | `PERMISSION_DENIED` or `INVALID_ARGUMENT`; no row, no use |
| wrong secret, unknown id | one identical `UNAUTHENTICATED enrollment refused`; same work (dummy hash); rate-limited per source |
| replayed `(token, CSR)` | the original certificates, no second use, no second row |
| same CSR under a different node id | `INVALID_ARGUMENT`: the idempotency key includes the node id the row was written for |
| node id already enrolled | `ALREADY_EXISTS` unless the token has `replace`; with it, the old node's serials and keys are denied in the same transaction |
| CSR with a bad self-signature, RSA, a P-384 key, 5 KiB, trailing garbage | `INVALID_ARGUMENT` |
| CSR asking for SANs, a CA flag or another role | accepted as a key carrier; the issued certificate carries none of it |
| `advertise` equals the frontend's name, another node's, or loopback | `INVALID_ARGUMENT` / `ALREADY_EXISTS` |
| revoked node renews | `PERMISSION_DENIED`; counted |
| `Renew` signed by another key | `PERMISSION_DENIED` |
| stale or future `issued_at` | `INVALID_ARGUMENT` with the frontend's time (the skew shows) |
| node clock far off | the node refuses an answer whose `server_time` differs by more than 2 minutes, logging both times; it installs nothing it could not validate |
| pin mismatch, or a CA bundle that lacks the pinned root | the node discards the answer; exit code 2; the token is not marked used locally |
| token file readable by others, a symlink, too large, two lines | the node refuses to read it |
| frontend down during enroll | the node retries with backoff (the same CSR, so idempotent); during renewal it keeps its certificate until it expires, then keeps retrying |
| state store down | `UNAVAILABLE`; no certificate is returned without its row |
| flood on the enroll port | per-source and global rate limits, the concurrency cap, the handshake deadline; metrics `kbf_enroll_refused_total{reason}` (a reason name from a closed set) |
| CA key unreadable | the listener does not start; `/readyz` says why |

## 6. The CA at the frontend

### 6.1 Custody

- The CA is an ECDSA P-256 key and a self-signed certificate (basicConstraints `CA:TRUE, pathlen 0`, keyUsage `keyCertSign, cRLSign`, ten years), created by `kbf ca init` (or at the frontend's first start with `--ca-init`) in `<frontend state>/ca/` (`ca.key` `0600`, `ca.pem` `0644`), owned by the frontend account.
- The key is **never** written to the state store, never in the SQLite backups (`kbf-deploy host state-backup` copies the database only **[F]**), never in a release, never in a log; the type's `Debug` prints nothing.
- All signing goes through a `Signer` trait (`sign(tbs, algorithm) -> signature`). **Decided (owner): one CA, its key in a separate signing process; a cloud KMS is a later option.** Production uses a signer outside the frontend process: either a separate `kbf-signer` process, run as its own account, that owns `ca.key` and serves a Unix domain socket (`SO_PEERCRED` admits only the frontend's uid; it signs only a to-be-signed structure whose role, names and key usage it re-checks against the table of 5.5, so a compromised frontend can mint only well-formed node certificates, never a CA or a frontend-role certificate for a stranger's key), or a cloud KMS. The **in-process file signer is the development and test default**, and the frontend refuses it under `--production`. A PKCS#11 or KMS signer is another implementation of the trait; nothing in the issuance path may assume the key bytes are in the process [D].
- There is **no escrow** in v1. Losing `ca/` is a recovery, not a disaster: reversible by a new CA and a new set of tokens, because every node's identity is re-creatable (section 6.5). The operator guide says to back up `ca/` with the host's own backups.
- A frontend that restarts reloads the CA from files; `kbf-tls` reloading does not apply to the CA (it signs; it does not serve), so a CA file change takes a handover/restart.

### 6.2 `kbf-ca`

A new library crate with no network and no state-store code: key and certificate creation, the `Signer` trait, the file signer and the client of the signer socket, CSR validation (5.5), certificate building (the table), serial generation, the lifetime policy, bundles. Everything takes a clock and a CSPRNG as parameters so tests are deterministic. It depends on `rcgen`, `x509-parser`, `ring` (all already in the tree **[F]**: rcgen is "test certificates only" today, so the workspace comment and `deny.toml` stance on it change, which B3a states in its description).

### 6.3 Roles and the existing script's v2 table

| existing script row | Enrollment equivalent | Notes |
|---|---|---|
| `frontend-worker` (frontend, serverAuth + clientAuth) | issued by the frontend **to itself** at start and renewed in-process; never by token | SANs: the frontend's names. A second frontend (Postgres era) gets a certificate from the first through an operator command; no `frontend` role token exists [D] |
| `storage-<node>` (storage, both EKUs) | token role `storage`, `RoleCsr.role = "storage"` | DNS: node id and advertise host |
| `daemon-<node>`, `daemon-<mac>` (daemon, clientAuth) | token role `daemon` | the Mac id is `node.id`; use a `node_id`-bound one-use token |
| (a node that is both) | two certificates, **two keys**, from one request | one key per role, so a leak or a deny of one key does not silently imply the other; the registry row holds both |
| "client" in the owner's list | **not a role** | buck2 and CI present no certificate; the REAPI listener stays server TLS with a certificate for the farm's public name; client API tokens are later and enter through `kbf-auth` (`tlsClientCertificate`, `jwt` are the named "not built yet" variants **[F]**) |
| the REAPI certificate (`tailscale cert` today) | **not issued by this CA**, as now **[F]** | supplied as files; `kbf-tls` reloads on renewal. In-process ACME is out of scope |

### 6.4 Rotation

Rotation never needs a flag day because every trust surface is a **bundle** (`--worker-client-ca` and `--storage-client-ca` take a PEM bundle **[F]**) and every node receives the bundle at each renewal:

1. `kbf ca rotate --stage`: create `ca.next.{key,pem}`, signed by the **current** CA (a cross-signature, so the new root is verifiable by a node that holds only the old pin) and also self-signed; the bundle served is `[current, next]`; the frontend writes the bundle to the `ca.pem` its listeners read (reload).
2. Nodes pick up the bundle within one half-life (3.5 days) at their next renewal and write it to `pki/ca.pem`; storage and worker listeners now accept either.
3. `kbf ca rotate --activate`: new certificates are issued from `next`. Nodes switch their trust as they renew.
4. After one full lifetime plus the grace (default 8 days) `kbf ca rotate --retire` drops the old CA from the bundle. Any certificate still carrying the old chain is, at that moment, invalid, which is the intended end.

A pin in a token minted before rotation is the old root's hash: it still verifies, because the answer's bundle contains a certificate whose chain reaches the pin through the cross-signature (5.4 step 3). Tokens minted after `--activate` carry the new pin; the old one stays valid until retirement. The compromise path is `kbf ca rotate --emergency`: stage, activate and retire in one step, and it **revokes every outstanding token** in the same transaction (reason `ca-rotation`): a token minted under a compromised CA carries a pin an attacker may know, and an attacker holding the old key could stand up an enrollment server that its pin would accept. Operators mint new tokens after the rotation; the listener presents the cross-signed chain of 5.2 until the old root is retired. A node that renews before the old key is retired gets a certificate from the new CA and trusts the bundle; a node that was offline past that point re-enrolls with a fresh token. Whether the first group is large enough to call this hitless is **[V]**: B3a's test measures it, and until then the honest description is "a re-enrollment for the stragglers".

### 6.5 Losing the CA key, or the state store

- **CA key lost**: nodes keep working until their certificates expire (7 days at most), the frontend creates a new CA, and every node needs a new token and a restart of `kbf enroll` with the new pin. Documented as a drill and tested in the rehearsal (section 10).
- **State store lost, CA intact**: tokens and the registry are gone; certificates still verify (they are the CA's), so builds keep running; nodes renew by `Renew`, which fails because the registry has no row. B3 gives `Renew` a **re-adoption** path for this case, off by default (`--enroll-adopt-unknown`): a signed CSR for an unknown node is accepted and the row rebuilt only when the node also presents its live certificate, the CSR's public key **equals that certificate's SubjectPublicKeyInfo**, the certificate chains to the CA and is unexpired, and its serial is **not in a deny row**. Deny rows live in the same store: **a store loss erases every revocation**, so a node revoked before the loss is adoptable again if its certificate has not expired. That is why the flag is off by default and why, after an adoption, the operator rotates the CA if any revocation was outstanding. It widens trust after a loss, so it is the owner's call (section 12, item 6). The default is re-enrollment.

### 6.6 Migrating the current farm CA

1. `kbf ca import --cert ca.pem --key ca.key` on the frontend host (the existing CA directory the operator already holds; the command reads the files, checks they match and are a CA, and copies them into `ca/`). The pin of the imported CA is what tokens carry, so **the existing 15 certificates stay valid and keep working** while nodes migrate.
2. Enable `--enroll-listen` (not on a production frontend before B3c lands: see 8.3); the frontend issues itself a `frontend-worker` certificate from the CA and stops needing the build host's file.
3. Migrate nodes one at a time: install the package, write `kbf.toml`, place a token; the node enrolls and writes `pki/`. The old certificates in `/etc/kbf` are no longer read by `kbf node`.
4. When every node is enrolled, run `kbf ca rotate --emergency` (or the staged form) to move to a CA whose key has never been on the build host, and remove the existing CA directory. The old certificates stop being valid at that moment, which is the cut.

## 7. mTLS inside the farm, plain server TLS outside

Unchanged by this work, stated so the boundary is not ambiguous: worker and storage listeners keep mutual TLS and the role tables; the REAPI listener keeps server TLS and `kbf-auth`'s policy; the operator API stays loopback plus bearer. The enroll listener is a fourth listener whose only authentication is the token. The Mac in `trust = "pin"` mode needs no profile or keychain entry for the pin: the pin check is in `kbf-daemon`.

## 8. Fit with the existing code

### 8.1 Components touched

| Existing piece | Change | Unit |
|---|---|---|
| `kbf-peer` | none; `PeerRole` stays three | none |
| `kbf-tls` | none for servers; B4 adds a **pin verifier** for the enroll client (a `ServerCertVerifier`), kept in `kbf-enroll` | B4 |
| `kbf-server` `members_rpc` | bind the certificate's DNS name to `Registrant.instance_id` (the rule `identity.rs` applies to a daemon), and refuse a node the registry marks revoked; today any storage-role certificate registers as any instance **[F]** | B3c |
| `kbf-server` `identity.rs` / worker | a deny source that merges the file and the store's `deny/` rows; the file stays for tests and the infra path | B3c |
| `kbf-server` `members.rs` | `refresh` reads `deny/` rows and writes them into `Map.denied` (today copied forward only) | B3c |
| `kbf-storage` admission | apply `Map.denied` to every endpoint after the role check (the P7 item) | B3c |
| `kbf-state` | none; new key families only (below) | B2, B3 |
| `kbf-auth` | none now. `Policy` is REAPI client authentication; enrollment is not REAPI. Client API tokens later extend `LATER_AUTHENTICATION` | later |
| `kbf-deploy` | `package` kit (section 3.3); `Mode::Node`, a `kbf-node.service` template and `check-units` rules for it (the `exec` rule learns `kbf node`) | B5 |
| `kbf-cli` | `kbf node`, `kbf enroll`, `kbf token`, `kbf ca`, `kbf node revoke` | B2, B4 |
| `kbf-installer` (`feature/pull-updates`) | none; used as a library by `package postinst` | B5 |

### 8.2 State-store keys

| Key | Value | TTL |
|---|---|---|
| `enroll/token/<id>` | token row (5.1) | none (kept after use for audit; a purge command removes rows expired more than 90 days) |
| `enroll/node/<id>` | registry row (5.7) | none |
| `enroll/serial/<hex>` | `{node_id, role, not_after}` | none |
| `enroll/key/<spki-sha256>` | `{node_id, role}` | none |
| `enroll/req/<token_id>/<csr sha256>` | `{node_id}` | `expires_at` of the token plus 1 day |
| `enroll/renew/<node_id>/<csr sha256>` | seen marker | 10 minutes |
| `deny/<kind>/<value>` | `{reason, at, by}` | none |
| `enroll/ca/generation` | counter of bundle changes | none |

Counters use the store's C3 counters; the issued serial is random, not a counter, so a restored store cannot reissue one.

### 8.3 The deny path is the largest hidden dependency

Revocation as designed needs three things that do not exist: a writer of `Map.denied`, storage-side enforcement, and a store-backed source for the worker listener. They are one unit (B3c) so they land with a test that denies a node and watches **a storage node** refuse it. **Gate.** B3b registers `--enroll-listen` but the frontend refuses to start with it unless the build carries the marker `DENY_ENFORCED`, which B3c sets in the same change that makes storage apply `Map.denied`; B3b's CI test asserts the refusal. So an enrollment listener cannot be switched on in production, by mistake, while a revoked node would still be served by storage. Until B3c merges, revocation is "refuse renewal and wait for expiry" plus the existing file deny list on the worker listener, and the operator guide must say so.

### 8.4 Ports and URL

The frontend gains `--enroll-listen`, `--ca-dir`, `--frontend-name` (repeatable: the names the frontend serves and that no node may claim), `--enroll-cert-lifetime`, and the renew bounds. The node learns only the URL's port (the enroll port); the worker and storage ports reach it in `NodeConfig`.

## 9. B6: what is retired for product use, and what stays

| Item (all outside this repository) | Product use | Fate |
|---|---|---|
| the certificate script, v2 mode | issuing node and frontend certificates | **retired for product**. Stays only as the existing way to make the CA that `kbf ca import` reads, until the migration of 6.6 completes; then deleted |
| the same script, v1 mode (the first Mac) | the old server's client certificates | untouched until the previous system is retired |
| the existing installer and its setup, fetch and pin files | host install | **retired** when B5 lands: the package plus the helper replace it. Stays for the test farm until its nodes are reinstalled from the package |
| the frontend certificate renew helper | renewing the farm-CA worker certificate | **retired** (the frontend renews its own). Stays for the **REAPI** certificate (a `tailscale cert` pair) until the public certificate is renewed outside kbf another way (decided: it is renewed outside kbf for now, section 12, item 7) |
| host-prep script 1 (build host: CA, all certificates, copy to nodes) | prepare | **retired**. Replaced by `kbf ca import`, `kbf token create`, `apt install` |
| host-prep script 2, steps for the kit, the user, subordinate ids, lingering, certificates, `crun` and `uidmap`, the installer and key cleanup | per-node host prep | **retired**: `package postinst`. Its steps for stopping the old build-farm units, sudoers, the private storage network and the hosts line are our infrastructure and stay as one-shots |
| the per-node cold-store key file (`s3.env`) and its `EnvironmentFile=` line | storage node credentials | **retired** when the `ColdStore.GetKeys` RPC ships (section 4.3): the keys become frontend configuration. The existing storage-and-daemon units keep using it until each node is reinstalled |
| host-prep script 3 (worker label, object-store NodePort) | cluster writes | **stays** (our k3s) |
| host-prep script 4, steps for the frontend account, worker certificate, renew timer and installer | the frontend host | **retired**: the `kbf-frontend` package and `kbf ca init`. Its steps for the private-network address and the `tailscale cert` stay |
| the private-network, node-label, object-store, bucket, teardown, flip and Mac one-shots | our infra | **stay**, never product code |
| `kbf-deploy linux install-units`, `render-units`, `check-units`, `image-pull`, `swap` | host prep | `install-units` and `image-pull` stay for the five modes and the infra path; for product installs `package postinst` and the daemon's own pinned-image store (#936) replace them; `swap` is an optional host tuning, stays |
| `kbf-deploy host install|rollback|gc` | the manual release verbs | stay: the updater and the package call the same library |

"Retired" means: the product docs do not name it, no CI path runs it, and the next step after the last farm node is reinstalled is deleting it where it lives. The rule from the owner stands: a step is not rewritten in shell; each retired step above has a Rust verb and a test as its replacement.

## 10. Test plan

Each test names the defect it catches and the mutant that turns it red. The bar is the repository's: a test that cannot fail is not a test. Hermetic means no network beyond loopback, no real clock (the state store's `ManualClock` and an injected CSPRNG), no host paths (a scratch root).

### 10.1 Unit tests by crate

| Crate | Test | Catches | Mutant |
|---|---|---|---|
| `kbf-ca` | every certificate it issues passes the `verify-cert` checks of its row (role URI exactly one, EKU set exact, daemon DNS exactly the node id) | a certificate a listener would refuse, or drift from the row table | add a second URI SAN; add serverAuth to daemon |
| `kbf-ca` | a CSR carrying SANs, `CA:TRUE`, another node's name, a wildcard yields a certificate with none | copying CSR content into the certificate | copy requested extensions |
| `kbf-ca` | bad self-signature, RSA, P-384, Ed25519, 2 KiB+1, trailing bytes | accepting an unproved or odd key | skip the signature check |
| `kbf-ca` | serials: 1000 issues, all distinct, positive, ≤ 127 bits | a counter or a reused serial | fixed serial |
| `kbf-ca` | rotation: a leaf chains to either root in the bundle; the cross-signed new root verifies from the old pin | a rotation that strands nodes | omit the cross-signature |
| `kbf-enroll` | `kbf.toml`: every row of the 4.1 table accepted and refused (path, scheme, userinfo, node id pattern, advertise, unknown key, wrong type), all problems reported together | a typo silently ignored | allow unknown keys |
| `kbf-enroll` | token file: symlink, `0644`, two lines, 5 KiB, wrong owner | reading a token others can read | skip the mode check |
| `kbf-enroll` | token parse: part names reported, secret never in the message | echoing a secret in an error | include the input in the error |
| `kbf-enroll` | pin verifier: right pin, wrong pin, chain without the pinned certificate, hostname mismatch, expired leaf | trusting the first connection | accept any chain |
| `kbf-server` enroll | token store: hashed at rest (the stored bytes never contain the secret); shown once (no read path returns it); constant-work unknown vs wrong | a plaintext secret, a second reveal | store the secret |
| `kbf-server` enroll | the transaction: crash after commit and before response, retry returns the same certificates, one use | losing a node's use, or double-spending | write the idempotency row after the response |
| `kbf-server` enroll | race: `max_uses + 4` concurrent enrolls on one token, exactly `max_uses` rows | over-use | read-then-write without compare-and-set |
| `kbf-server` enroll | revoke writes registry, serials, key hash, node entries in one transaction (kill between: none or all) | a half-revoked node | write deny rows after the registry change |
| `kbf-server` rate limit | burst from one source, many sources, the global cap, the handshake deadline | an enroll port that is a free CPU burner | no limit |
| `kbf-deploy` package | `postinst` on a scratch root: user, subid block (planted neighbouring range, planted smaller existing range, planted exact collision), linger path, trust files, first release; run twice; run as upgrade | a re-run that duplicates, a range that overlaps | take the first block without reading `/etc/subuid` |
| `kbf-deploy` package | `postinst` with `current` present never moves `current` or `previous`; with it absent creates it | the package stealing the updater's authority | always flip |
| `kbf-deploy` package | `check-units --mode node` over the shipped template (the existing rules) and `kbf check-args` of the shipped binary | a unit the binary rejects | rename a flag in the template |
| repo lint | the package's maintainer scripts are ELF files and the budget gains no row | shell creeping back | ship a `#!/bin/sh` postinst |

### 10.2 The hermetic enrollment test (B3 and B4 together, in `kbf-it`)

In one process: SQLite state store on a temp file with a `ManualClock`; the real `Enroll` service on a loopback `--enroll-listen` with a `kbf-ca` CA; the real node client from `kbf-enroll` with a `kbf.toml` in a scratch root. It then:

1. enrolls a `storage-and-daemon` node from a 2-use token, and checks the files, modes (`0600` keys, `0700` directory), the registry row, the pin check, the token file removed;
2. starts a real worker listener and a real storage `Membership` service with the CA bundle and connects with the **issued** certificates: the `Hello` is admitted, the role table passes, the storage registers with `instance_id = node id`; a storage certificate registering as another instance is refused (the binding of 8.1);
3. advances the clock to half life, observes `Renew` and the new serial, and the **running** listener serving the new certificate with no restart (`kbf_tls_reloads_total{outcome="applied"}` moves);
4. revokes the node and observes the worker refusing it at the next `Heartbeat` and a **storage node's** admission refusing it after the map turns;
5. advances past expiry with the frontend down, brings it back, and the node renews without a token.

The abuse tests of 5.10 run in the same harness, one case each, plus the mutant list above applied one at a time to the production code; a case that does not go red under its mutant is a failing review item.

### 10.3 The package test in CI (B5)

A workflow `package.yml` (hosted runners, pinned actions, no shell logic in `run:` steps, per the repository's workflow lint) with:

- **S1, every pull request that touches packaging.** A container from a pinned Ubuntu image: `apt install ./kbf.deb` (the real maintainer scripts run; `podman` absent is asserted), check the files and owners of 3.2, `systemd-analyze verify` on the units, `crun --version`, `newuidmap` present, then (as the `kbf` user) `kbf node --config` against a **test frontend** (a second container from the frontend image, a fresh `kbf ca init`, a token made with `kbf token create` and passed by a `0600` file). Assert: the node enrolls, `GET /v1/enroll/nodes` lists it, `GET /v1/members` shows the storage registered, the certificate files exist with the right modes, and the node's role row reads `storage` (the S1 role is `storage` because a `daemon` needs cgroup delegation that an unprivileged container lacks **[V]**). Then `apt remove` and `purge`: the user, units and keys are gone, the data directories stay.
- **S2, nightly and on demand.** The same on a systemd-in-container image started with the privileges it needs, with the real `kbf-node.service` and a `daemon` role, a no-op action run through a frontend, and the `apt upgrade` path (package N to N+1 leaves `current` where the updater put it).
- **S3, the drill.** On the S2 setup: delete the frontend's state store, keep `ca/`, check nodes fail to renew and that `--enroll-adopt-unknown` recovers them; then delete `ca/` and check the documented recovery.

The `.pkg` and cask are built on the hosted macOS runner, signed ad hoc in CI (the farm identity is a secret held elsewhere) and installed by an S1-like job; the root LaunchDaemon start is checked with `launchctl print`, and the full signed path is part of the final farm cutover rehearsal, not CI **[V]**.

## 11. Units B2 to B5: independent PRs with disjoint file scopes

Dependency: `P0` first (it is tiny and unblocks the rest), then everything else in parallel. Conflicts at shared registration files (`kbf-cli/src/lib.rs`, `kbf-server/src/lib.rs`, `crates/kbf-proto/build.rs`, `docs/api.md`) are one-line additions; each unit adds its own line and rebases mechanically.

| Unit | Scope (files it may touch) | Does | Needs |
|---|---|---|---|
| **P0** | `crates/kbf-proto/proto/kbf/enroll/v1/enroll.proto`, the generated-code registration, `crates/kbf-types/src/enroll.rs` (token and node id newtypes, role set, the canonical formats of 5.1) | the wire and the shared types, with golden-byte tests; the closed set of refusal reasons | none |
| **B2a** frontend tokens | `crates/kbf-server/src/enroll/tokens.rs`, `.../tokens_tests.rs`, route hooks in `kbf-server/src/api.rs` (one `mod` line), `docs/api.md` section for the routes | token create, list, revoke, hashed storage, shown once, constant-work compare, expiry, uses | P0 |
| **B2b** token CLI | `crates/kbf-cli/src/token.rs` (new), one enum variant in `lib.rs` | `kbf token create|list|revoke`, `--out FILE` exclusive `0600` | P0, B2a's routes (contract fixed by P0) |
| **B2c** token UI page | `crates/kbf-ui/...` (its own files) | token and node list page | B2a, B3b |
| **B3a** CA | `crates/kbf-ca/` (new crate), `crates/kbf-signer/` (new: the signer process over a Unix socket), workspace `Cargo.toml` member lines, `deny.toml` note | 6.1 to 6.4: CA create and import, `Signer` (file, socket client), the `kbf-signer` process, CSR validation, issuance, rotation | P0 |
| **B3b** enroll service | `crates/kbf-server/src/enroll/{service.rs,registry.rs,renew.rs,limits.rs}`, `--enroll-listen` and `--ca-dir` flags in `kbf-server/src/config.rs`, the listener in `listen/` | 5.2 to 5.8: the transaction, registry, `Renew`, rate limits, the frontend's own certificate; the `--object-store-*` flags and the `ColdStore.GetKeys` service (4.3, with its row in `worker_roles.rs`) | P0, B3a, B2a (the token row type) |
| **B3c** revocation path | `crates/kbf-server/src/{members.rs,members_rpc.rs,identity.rs}`, `crates/kbf-map` (if a canonical write helper is needed), `crates/kbf-storage/src/service/admission.rs` and its tests, `POST /v1/enroll/nodes/{id}/revoke` | 5.9 and 8.3: `deny/` rows into `Map.denied`, storage enforcement, the instance-id binding, the worker store-backed deny source | P0 |
| **B4a** node library | `crates/kbf-enroll/` (new crate): `config.rs` (`kbf.toml`), `token.rs`, `pin.rs`, `client.rs`, `store.rs` (keys, atomic files, `node.json`), `renew.rs` | 4 and 5.4 to 5.8 on the node; the `GetKeys` client that keeps the keys in memory (4.3); no CLI | P0 |
| **B4b** node wiring | `crates/kbf-cli/src/{node.rs,enroll.rs}` (new), `names.rs` (`node`), one line each in `lib.rs`, `crates/kbf-daemon` (client identity rebuild on renewal, if [V] says it is needed), `crates/kbf-mac-provision` (plist runs `node`) | `kbf node`, `kbf enroll`, the mode from `NodeConfig.roles`, the daemon's identity refresh | B4a |
| **B5a** package verbs | `crates/kbf-deploy/src/package/` (new), `Mode::Node`, `templates/linux/modes/kbf-node.service`, `check-units` rules for it, including the refusal of a node unit that carries a key file or `AWS_*` | 3.3 as Rust with scratch-root tests | B4b's flag names (fixed by this document) |
| **B5b** Linux packaging | `packaging/linux/` (control, sysusers, tmpfiles, `kbf-package-hook`), `crates/xtask/src/deb.rs`, `.github/workflows/package.yml`, no interpreter-budget row (the hooks are executables) | `.deb` and `kbf-frontend.deb` built from the attested tarball; S1 to S3 | B5a |
| **B5c** macOS packaging | `packaging/macos/` (pkg scripts as Rust verbs, the plist, the cask), `crates/xtask/src/pkg.rs`, a macOS job in the workflow | `.pkg` and cask | B5a, B4b |
| **B5d** frontend image | `packaging/container/Containerfile`, an image job in the workflow | 3.4 | B3b |
| **B6** retirement | outside this repository only: those scripts' docs and run sheets; no code here | marks and later deletes the rows of section 9 | B5b done and one node reinstalled from the package |
| **Doc** | `docs/deploy/install.md`, `docs/deploy/enrollment.md` (operator guide, including the residuals of 5.1 and 5.4 and the 8.3 caveat) | written alongside B5b | |

Order by value: P0, then B3a, B2a and B4a in parallel, then B3b and B3c, then B4b, then B5a to B5d. B3c is independent of B3b except at test time and should start early because storage-side enforcement is the long pole.

## 12. Decisions

**Decided** items were settled by the owner in the review round and are applied throughout; the rest keep the lean the work proceeds with. **[owner]** marks a choice that is still the owner's.

| # | Decision | Status and where applied |
|---|---|---|
| 1 | apt updates the package's root-owned files (helper, units, maintainer executables, trust files); the pull updater updates the kbf release | **Decided.** 3.5 |
| 2 | One CA; its key lives in a separate signing process (`kbf-signer` over a Unix socket); a cloud KMS is a later option; a single root with `pathlen 0`, no intermediate | **Decided.** 6.1, 6.2 |
| 3 | Certificates last 7 days and renew at half their life | **Decided.** 5.5, 5.8 |
| 4 | First-contact trust is the CA fingerprint inside the enrollment token; `public` is an explicit fallback | **Decided.** 5.4 |
| 5 | Macs: a signed `.pkg` installed by a Homebrew cask | **Decided.** 3.1 |
| 6 | `--enroll-adopt-unknown` is off by default | **Decided.** 6.5 |
| 7 | The frontend's public (REAPI) certificate is renewed outside kbf for now | **Decided.** 6.3, 9 |
| 8 | The object store is frontend configuration; storage nodes get the keys over mTLS, in memory only, at each start; or `ambient` mode with no keys; scoped short-lived credentials are a later option | **Decided.** 4.3 |
| 9 | One key per role on a `storage-and-daemon` node | Lean: two keys; revocation by node id covers both |
| 10 | The frontend pushes a changed `frontend_url` to nodes | Lean: yes, guarded: adopted only under the same pin, `kbf.toml` kept as fallback |
| 11 | The frontend container image has no in-image updater | Lean: yes; the orchestrator replaces the image; the `.deb` and the Envoy handover remain the host path |
| 12 | A signed apt repository (versus the `.deb` as a release asset) | Lean: the release asset first; a repository when a second operator exists |
| 13 | Who may mint tokens: the operator API token as it is today, scoped operator roles later | Lean: as it is; documented as exactly as powerful as draining the frontend today |
| 14 | Key rotation on renew, and an `unattended-upgrades` default | Lean: neither in v1 |
| 15 | In-process ACME for the public certificate | Lean: not planned; item 7 stands |

## 13. Failure modes of the design

- **The enroll listener is the new front door.** Its blast radius is a CPU burner and a token oracle, both bounded (5.10); it serves two methods, issues certificates for exactly the fields in 5.5 and nothing from the request; it is off without a CA.
- **The frontend is a single point for issuance.** Running builds are unaffected by an enrollment outage; renewal has a 3.5-day margin before expiry and retries; a fresh node cannot join until the frontend is back.
- **The CA key in a file** is the farm's most valuable secret. It is mode `0600` under a dedicated account, outside the state store and the backups, behind a `Signer` seam; the cost of its theft is a rotation and re-enrollment, bounded by the lifetime of what it signed, with a drill to prove it.
- **Revocation is eventually complete** (section 5.9), immediate for renewal and new storage access once B3c is in, and bounded by 7 days otherwise.
- **A package upgrade changes root code** and is the one place that does so (3.5). It is an operator act, tested for the `current` link (S2).
- **Clock skew** is checked by the node on every answer and surfaced; a node with a wrong clock fails loudly instead of installing a certificate it cannot use.
- **Reversibility.** Enrollment is additive: the five modes, the flags and script-issued certificates keep working until the CA migration of 6.6 step 4, which is itself a single reversible rotation while the old CA is kept in the bundle.
