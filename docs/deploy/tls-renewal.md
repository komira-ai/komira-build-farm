# Renewing a listener's certificate

`kbf frontend` and `kbf-server` serve TLS on two listeners, and `kbf storage` on one, from files:

| Listener | Files | Flags |
|---|---|---|
| REAPI (clients) | server certificate, its key | `--reapi-tls-cert`, `--reapi-tls-key` |
| worker (daemons, mutual TLS) | server certificate, its key, the CA daemon certificates chain to | `--worker-tls-cert`, `--worker-tls-key`, `--worker-client-ca` |
| storage node (frontends, daemons, other nodes; mutual TLS) | server certificate, its key, the CA callers' certificates chain to | `--storage-tls-cert`, `--storage-tls-key`, `--storage-client-ca` |

They look at those files every `--tls-reload-interval-secs` (default 30, 1 to 3600).
To renew, replace the files. Nothing is restarted, and no connection is ended:

- A TLS handshake that starts after the files were applied is served the new
  certificate, and checks a client certificate against the new CA bundle.
- A connection that was already open goes on under the certificate and trust roots it
  was accepted with, until it closes. A certificate or node refused after that is the
  deny list's job (`--worker-deny-list`, read again at every call), not the CA bundle's.

## What is applied

New files replace the old only if together they make a working configuration: every
file reads; the certificate chain and the key are PEM; the key is the certificate's; the
client CA bundle holds at least one certificate that can be a trust anchor; and the
leaf has not expired. Otherwise the previous files keep serving, the reason is logged
as a warning (`TLS files changed but are not usable`), and
`kbf_tls_reloads_total{outcome="rejected"}` counts it once. A bad state is reported
once however many times it is looked at.

A renewal tool that writes the certificate and then the key is caught between the two
writes as a mismatched pair. That is refused like any other bad state, and the next
look finds the pair whole and applies it. Replace each file in one step (write a
temporary file next to it, then rename it) so a look never reads half a file.

The files must be good when the server starts: a start with a bad file stops, as before.
(An expired certificate still starts, as before; it is not applied by a later change.)

## Watching it

A storage node has no operator API: its rejected looks are in its log only (the
warning above), and it has no `/metrics` to scrape. Watch the log for it.

On the operator API's `/metrics` ([api.md](../api.md#get-metrics)):

- `kbf_tls_reloads_total{listener,outcome}`: a rise in `rejected` after a renewal means
  the new files were not usable, and the old ones are still in use.
- `kbf_tls_leaf_not_after_seconds{listener}`: when the served certificate expires.
  After a renewal it should move; if it does not, the renewal was not applied.
