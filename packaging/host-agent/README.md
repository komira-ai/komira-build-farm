# The host agent: units and jobs

The files an operator places to run a host's update agent (`docs/design/updates.md`). Nothing
here is in a package yet: the binaries are release assets (`docs/deploy/host-install.md`,
"Getting the binary"), and these files go beside them.

| file | what |
|---|---|
| `linux/kbf-host-local.service` | `kbf-host-install --serve`, as root: the local socket (`/run/kbf-host-install/local.sock`, mode 0600, owned by `kbf-updater`) that stages a release, switches to it on trial, and goes back by itself at the deadline |
| `linux/kbf-updater-agent.service` | `kbf-updater agent`, as `kbf-updater`: the heartbeat to the coordinator and the host's A/B through that socket |
| `linux/kbf-updater.conf` | the sysusers.d entry of the `kbf-updater` account (no login, no home) |
| `enroll-example.txt` | the enrollment command, as the tests read it |
| `macos/com.komira.kbf-host-install-local.plist` | the same socket on a Mac, as root, in `/var/db/kbf-host-install` |
| `macos/com.komira.kbf-updater-agent.plist` | the agent on a Mac, as `_kbf-updater` |

The values in the files are examples (`coordinator.farm.example`, `frontend.farm.example`, the
state directory): edit them to the farm's. `crates/kbf-updater/tests/agent_packaging.rs`
reads these files and checks that every command line parses as the programs parse it, that
the two ends agree on the socket and on the account that may connect to it, and that the agent
is not root.

## Enrolling a host

A host gets its updater certificate from the farm CA with a one-time token. On the frontend,
the operator makes the token for this host (`POST /v1/enroll/tokens`, `docs/api.md`) with
`"roles": ["updater"]`, the host's `node_id` and its `host_role` (`storage`, `daemon` or
`mac-daemon`); it is good for one use. On the host, as the agent's account (`kbf-updater`, or
`_kbf-updater` on a Mac), with the token in a file of that account's, mode 0600:

```sh
install -d -o kbf-updater -m 0700 /var/lib/kbf-updater
sudo -u kbf-updater sh -c 'umask 077; cat > /var/lib/kbf-updater/enroll.token'   # paste the token, end with ^D
sudo -u kbf-updater /usr/libexec/kbf/kbf-updater enroll --frontend frontend.farm.example:8444 --node-id host-1 --token-file /var/lib/kbf-updater/enroll.token --state-dir /var/lib/kbf-updater
```

The key is made on the host and stays there (mode 0600, the account's); only a request signed
by it is sent. The command checks the frontend against the CA pin in the token, checks what it
gets back, writes `pki/updater.key`, `pki/updater.pem` and `pki/ca.pem` into the state
directory, and removes the token file. The agent then starts (or is restarted, if it was
waiting) and renews the certificate itself at half its life. `enroll-example.txt` is that
command, which `crates/kbf-updater/tests/agent_packaging.rs` checks against the unit's
`--state-dir` and the program's flags.

## What the agent needs

- **Its certificate**: from the enrollment above, in the state directory. The certificate says
  which node this is and what the host runs; the agent's flags say neither. (Placed files still
  work, with `--client-cert`, `--client-key` and `--ca` in place of `--state-dir`; nothing
  renews them.)
- **A configuration for the helper's channels**: on Linux `/etc/kbf/host-install-releases.toml`,
  on a Mac the existing `host-install.toml`. See `docs/deploy/host-install.md`, "The local socket".
- **No operator token, as shipped.** The drain of the node, its undrain and its status (a storage
  host's: its own storage health) are calls to the frontend's worker listener (`--frontend`, the
  address daemons dial) made over mutual TLS with the host's own certificate. The frontend
  serves exactly those four, for the node in the certificate and no other, and nothing else from
  an updater's certificate; no token is involved (`docs/design/updates.md`, "Trust"). The
  worker listener must offer `http/1.1` beside `h2`, which `--worker-tls-*` does. An operator who
  wants the full token on a host adds `--operator-api` and `--token-file` to the agent's command
  line (the API is on the frontend's loopback), knowing that the agent's account can then read it;
  the agent uses it instead of the certificate.
- **Where the certificate is renewed**: `--enroll-frontend`, the frontend's enroll listener
  (`--enroll-listen`), which is the address the enrollment above was given.
- **A frontend host is not served**: its certificate is refused. A frontend is a container
  that `kbf-host-install` drains and recreates; the updater does not update one.
