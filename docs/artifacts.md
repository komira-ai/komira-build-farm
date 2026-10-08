# Build artifacts: what CI builds, and how a node verifies it

kbf's binaries are built by CI, never on a developer's machine, and a node runs only
bytes CI built and attested. The workflow is
[`.github/workflows/artifacts.yml`](../.github/workflows/artifacts.yml).

## What is built

| Asset | Runner | Target |
|---|---|---|
| `kbf-daemon-<commit>-darwin-arm64.tar.gz` | `macos-15` (Apple silicon) | `aarch64-apple-darwin`, `MACOSX_DEPLOYMENT_TARGET=14.0` |
| `kbf-daemon-<commit>-linux-x86_64.tar.gz`, `kbf-server-<commit>-linux-x86_64.tar.gz` | `ubuntu-24.04` | `x86_64-unknown-linux-gnu` |
| `kbf-daemon-<commit>-linux-arm64.tar.gz`, `kbf-server-<commit>-linux-arm64.tar.gz` | `ubuntu-24.04-arm` | `aarch64-unknown-linux-gnu` |

`<commit>` is the full SHA of the commit built (`GITHUB_SHA`: on `main`, the pushed
commit; on a pull request, the test merge commit). Each tarball holds one release binary (`cargo build
--locked --release`), mode 0755, owned by 0:0, and nothing else. Next to each tarball
is its CycloneDX SBOM, `<same stem>.cdx.json`, made from `Cargo.lock` for that binary
and target by `cargo-cyclonedx` (pinned by version and SHA-256 in the workflow). One
`SHA256SUMS` covers every tarball and SBOM of the commit.

The Linux binaries link glibc dynamically and need the glibc of Ubuntu 24.04 or
newer.

## When

| Event | What happens |
|---|---|
| Pull request, merge queue | A build check: every asset is built, signed, packaged and summed, and `SHA256SUMS` is checked. Nothing is attested, so nothing is published. |
| Push to `main` | The same, then the `attest` job signs build provenance for every file in `SHA256SUMS` (`actions/attest-build-provenance`) and an SBOM attestation for every tarball (`actions/attest-sbom`), and verifies them with the command below. |

Only the `attest` job holds `id-token: write` and `attestations: write`, and only
under the exact `if:` that limits it to a push to `main`. The workflow lint
(`crates/kbf-it/tests/workflows`) refuses those scopes in any other job, so no pull
request run holds a token that can sign an attestation. Every action is pinned by
commit SHA, and that lint refuses one that is not.

## Where the files are, and for how long

The assets are workflow artifacts of the run (`assets-darwin-arm64`,
`assets-linux-x86_64`, `assets-linux-arm64`, `sha256sums`). GitHub keeps workflow
artifacts for at most 90 days, so nothing may depend on them for longer. The plan for
releases (fleet-updates design) is that a release attaches the exact bytes the farm
ran after their soak, fetched from the deployment's own copy and checked against these
attestations; a release never rebuilds. The attestations themselves do not expire with
the artifacts.

## How a node's deployment verifies an asset

The deployment job verifies each asset before it touches a node, in CI (a node has no
`gh`), with every flag below:

```text
gh attestation verify kbf-daemon-<commit>-darwin-arm64.tar.gz \
  --repo komira-ai/komira-build-farm \
  --signer-workflow komira-ai/komira-build-farm/.github/workflows/artifacts.yml \
  --source-ref refs/heads/main \
  --deny-self-hosted-runners
```

- `--signer-workflow` and `--source-ref`: without them, an attestation signed by any
  workflow of the repository, on any branch, would verify.
- `--deny-self-hosted-runners`: the signing job ran on a GitHub-hosted runner.
- For the SBOM attestation, add `--predicate-type https://cyclonedx.org/bom`.

The node itself then checks the asset's SHA-256 against the value the job hands it,
before it installs anything.

The `attest` job runs exactly these checks on its own output
(`tools/ci/verify-assets.sh`), and two that must fail: the first tarball verified as
signed by `ci.yml`, and a copy with one flipped byte. If either verifies, the job fails.

## macOS signing: what ad hoc with the hardened runtime gives

`tools/ci/sign-darwin.sh` signs `kbf-daemon` with `codesign --force --sign -
--options runtime`: an ad-hoc signature (no certificate) with the hardened runtime.
That works without an Apple Developer account, and the job checks it every run:
`codesign --verify --strict` passes, the code directory's flags are exactly
`adhoc,runtime`, the signature has no entitlements, and the binary's minimum macOS is
14.0.

What it gives:

- **The kernel checks every page against the signature,** as for any signed code on
  Apple silicon. A binary modified after signing does not run (but one modified and
  signed again ad hoc does; see below).
- **The hardened runtime flag.** On a Mac with System Integrity Protection enabled
  (a node), dyld ignores `DYLD_*` variables for a hardened process, so
  `DYLD_INSERT_LIBRARIES` cannot inject code, and without the `get-task-allow`
  entitlement a debugger cannot attach. CI checks the flag in the signature, not the
  behaviour: GitHub's hosted macOS runners have SIP disabled (the job prints `csrutil
  status`), and there dyld loaded `DYLD_INSERT_LIBRARIES` into the hardened binary as
  well, so the behavioural check only warns on them. It fails the job on a runner with
  SIP enabled. Showing the behaviour on a real node is left to the node's
  provisioning checks.
- **A stable code identity (the cdhash)** that a requirement can pin, as the
  fleet-updates design's helpers do. The job prints it.

What it does not give:

- **No identity.** Anyone can sign anything ad hoc; the signature says nothing about
  who built the binary. Authenticity comes from the attestation, checked above.
- **No notarization and no Gatekeeper approval.** A quarantined copy (one a browser
  downloaded) is refused. Nodes fetch assets with command-line tools, which do not
  quarantine them.
- **Library validation by team.** With no Team ID, the hardened runtime's library
  validation admits only Apple-signed libraries; `kbf-daemon` loads only system
  libraries.

Developer ID signing and notarization need an Apple Developer Program membership; when
the project has one, it is one more step in the darwin job.
