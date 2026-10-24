# Running the frontend and the coordinator as containers

The frontend (`kbf --mode=frontend`) and the update coordinator (`kbf-coordinator`) come as
container images, for a farm that runs them under an orchestrator. There is no registry. Each
image is a release asset, an OCI image layout in one tar:

| image | asset | runs |
|---|---|---|
| frontend | `kbf-frontend-image-<commit>-<platform>.oci.tar` | `kbf --mode=frontend --config=/etc/kbf/kbf.toml` |
| coordinator | `kbf-coordinator-image-<commit>-<platform>.oci.tar` | `kbf-coordinator`, its files at `/etc/kbf-coordinator` |

`<platform>` is `linux-x86_64` or `linux-arm64`. What the files are, how they are built and
checked, and how they are attested is [artifacts.md](../artifacts.md#the-frontend-and-coordinator-images-oci). The
Dockerfiles are in [`packaging/container`](../../packaging/container).

## Verify and import

Verify the archive before it goes anywhere, offline, with its bundle
([artifacts.md](../artifacts.md#provenance-bundles-verifying-a-tarball-offline)), exactly as a
tarball is verified:

```text
gh attestation verify kbf-frontend-image-<commit>-linux-x86_64.oci.tar \
  --bundle kbf-frontend-image-<commit>-linux-x86_64.oci.tar.sigstore.json \
  --custom-trusted-root trusted_root.jsonl --repo komira-ai/kbf \
  --signer-workflow komira-ai/kbf/.github/workflows/artifacts.yml \
  --source-ref refs/heads/main --source-digest <commit> --deny-self-hosted-runners
```

Then, on each host that runs the image, as root (this changes the host; it is the operator's
step):

```text
sudo k3s ctr -n k8s.io images import kbf-frontend-image-<commit>-linux-x86_64.oci.tar
sudo k3s ctr -n k8s.io images ls | grep kbf-frontend
```

The image is known by the tag it was built under, `kbf-frontend:<commit>` (the runtime
normalises it; the listing shows the name to give the pod). With no registry there is nothing to
pull from: set the pod's `imagePullPolicy` to `Never` (or `IfNotPresent`), so a host that lacks
the image fails visibly instead of waiting. An update is a new tag: verify and import the next
archive on every host, then change the tag the pod names. The image does not update itself.

## The frontend

The image holds `/usr/bin/kbf` on a distroless base with the system CA roots, and runs as the user id 65532 with
`kbf --mode=frontend --config=/etc/kbf/kbf.toml`. It has no installer, no proxy and no Envoy: put
it behind the orchestrator's own proxy or load balancer. Nothing is baked in. The orchestrator
mounts:

| path in the container | what |
|---|---|
| `/etc/kbf/kbf.toml` | the settings ([kbf-modes.md](kbf-modes.md)); the process exits 2 when the file is missing, and it must not be writable by group or others |
| the files `kbf.toml` names | certificates, keys, the operator token (`api-token-file`); owned by uid 65532, a key or token with no group or other permission bit |
| `/var/lib/kbf-server` | a volume the frontend may write (uid 65532, mode 0750) |

The listeners of `kbf` are on loopback by default, which no other container reaches, so
`kbf.toml` sets them to the addresses the orchestrator publishes. The image exposes the three
ports of the mode: 8980 (REAPI), 8981 (the worker listener) and 8982 (the operator API). An
example `kbf.toml` (names are documentation names; use the farm's own):

```toml
[listener]
listen = "0.0.0.0:8980"
worker-listen = "0.0.0.0:8981"
api-listen = "0.0.0.0:8982"

[tls]
reapi-tls-cert = "/etc/kbf/tls/reapi.pem"
reapi-tls-key = "/etc/kbf/tls/reapi.key"
worker-tls-cert = "/etc/kbf/tls/worker.pem"
worker-tls-key = "/etc/kbf/tls/worker.key"
worker-client-ca = "/etc/kbf/tls/ca.pem"
api-token-file = "/etc/kbf/operator.token"

[storage-client]
storage = "https://storage.example.invalid:8983"

[scheduler]
state-store = "postgres://kbf@db.example.invalid/kbf"
```

`state-store` and `storage` are required: a frontend given none exits 2 naming the flag. The
state database is brought to the schema this `kbf` needs by `kbf boot-check --mode=frontend`
before the start (the units run it; [kbf-units.md](kbf-units.md)). In a container, run it once per
image tag as a job with the entrypoint replaced (`/usr/bin/kbf boot-check --mode=frontend
--installer-config <file>`); it reads the `database` key of that `installer.toml`, and refuses a
URL that carries a password, so the database it migrates is one a password is not needed for.

## The coordinator

The image holds `/usr/bin/kbf-coordinator` on a distroless base with the system CA roots (CI's OIDC
keys are fetched over https), and runs as the user id 65532. Its default command names these files and ports; every
flag may be replaced:

| path in the container | what |
|---|---|
| `/etc/kbf-coordinator/tls.pem`, `tls.key` | the coordinator's certificate and key (role `coordinator`) |
| `/etc/kbf-coordinator/ca.pem` | the farm CA certificate the updaters' certificates must chain to |
| `/etc/kbf-coordinator/oidc.toml` | the allow-list of CI's OIDC token; give it an audience no other verifier accepts |
| `/etc/kbf-coordinator/operator.token` | the operator token, owned by uid 65532, mode 0600 or 0400 (any other mode is refused) |
| `/var/lib/kbf-coordinator` | a volume the coordinator may write, for the desired state (uid 65532, mode 0750) |

Port 8443 is the heartbeat listener (mutual TLS: updaters dial it), 8444 the control listener (CI
sets the desired state; an operator reads the status).

**The coordinator's DNS name is a setting you give, and there is no default for it.** The image
has no name in it, and the coordinator takes none: the name is the one in the certificate the farm
CA issued for the role `coordinator`, and each host's agent is told it, with the port, on its own
command line (`kbf-updater agent --coordinator coordinator.example.invalid:8443`, required; see
[kbf-updater.md](kbf-updater.md#the-agent-mode)). An agent refuses a coordinator whose
certificate is not for that name.

## Checking what runs

Run an image with the argument `--version` (the pod's `args`, or the runtime's command): it prints
`kbf <version>+<first 12 digits of the commit>`, and the coordinator's prints the same with its
own name. The release workflow has built each image from the binary it had already checked for the
commit and refused it unless that binary is the one inside, and `beta.yml` loads each push's
x86_64 images, runs them with `--version`, and checks the binary taken out of the image.
