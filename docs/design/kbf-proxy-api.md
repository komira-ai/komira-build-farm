# kbf-proxy: public API, module layout and pool mode

This page says what `crates/kbf-proxy` is and does now. The design it serves is
[frontend-proxy.md](frontend-proxy.md). The code cites the section numbers below, so the sections
that described the removed machinery are gone and their numbers (6, 13 and 15 onward) are not
reused.

The proxy is a plain load balancer over frontends that run on their own (the `[[proxy.frontend]]`
tables, section 9). The two-slot supervision of frontend children was removed from the installer
and from this crate: the slots and the gate (`slot`), the hold queue (`hold`), the routing of a
`WaitExecution` by the old child's term (`term`), the append of the active child's `/metrics`
(`child_metrics`), the CI credential of the update API (`remote`) and the fenced re-dispatch.
`kbf-installer proxy` refuses a table that still has the `upstream` table.

## 1. What kbf-proxy is

`kbf-proxy` is a library; `kbf-installer proxy` (a verb) is its only binary. It is the public
port of a frontend host: one TLS terminator, per-stream routing to a frontend of the pool on
loopback, identity headers, open-stream accounting and the pool's admin API. It supervises
nothing, starts no process, writes nothing under `current`, `slots/`, `status.json` or
`update.json`, and has no control socket. It makes no call to a frontend except the readiness
poll (section 7).

Reusable from the repo: `kbf-front/src/proxy/bytestream.rs` (peek, keep the first frames, fail
over only before the first byte) is the pattern of the replay buffer. Nothing else in
`kbf-front/src/proxy` applies: that proxy is the frontend talking to storage nodes. `kbf-tls`
builds the proxy's configuration (`server_config_optional_client_sni`, over a `SniCerts`
resolver: optional client certificate, ALPN `h2` then `http/1.1`, the ring provider, early data
off).

## 2. Module map

| Module | Holds |
|---|---|
| `route` | `Route`, `Target`, `AuthClass`, `Idle`, the closed path table |
| `auth` | `decide`, `Buck2Auth`, `Token`, `Tokens`, `Refusal` |
| `identity` | `Identity`, `identity_of`, `ProxySecret`, `forward_headers`, `admin_headers`, `strip_reserved` |
| `replay` | `ReplayBuffer`, `Failure`, `may_retry` |
| `config` | `ProxyConfig`, `Frontend`, `UpstreamTable`, `Limits`, `SLOT_MODE_REMOVED` |
| `control` | `UpdateHandler`, `UpdateRequest`, `UpdateReply`, `NoUpdates` |
| `files` | the secret files, read the way the child reads them and written crash-safely |
| `tls` | `TlsReloader` (certificates, key and CA as one unit, SIGHUP), the handshake |
| `serve` | accept loop, hyper's h2 server (windows, concurrent streams), per-connection task, the proxy's own routes (`/healthz`, `/readyz`, `/metrics`, the update path), shutdown drain |
| `core` | the state every stream shares: the members, the pool, counters, the streams in flight |
| `stream` | one stream: placement on a member, dispatch, retry on another member, GOAWAY and reset mapping |
| `body` | the request `Source` (replay frames), the pump, the response body, idle activity |
| `pool` | the upstream h2c pool per (member address, target), on h2's own client |
| `security` | the `buck2_auth` mode, the tokens, the secret, the per-address limit on wrong tokens |
| `reply`, `metrics` | the proxy's own answers; counters |
| `probe` | the `GET /readyz` of a member's operator listener (connect, I/O error, timeout and non-200 each fail the poll) |
| `upstreams`, `pool_probe`, `pool_admin` | the members of the pool (up or draining, ready or not, streams open), the poll of each, and the admin API on `admin_listen` |
| `limits`, `keepalive` | the h2 builders and flood limits, `STREAM_WINDOW_BYTES` and `HEADER_BYTES`; PING keepalive both ways |
| `h1` | HTTP/1.1 for `/v1/*`, the probes and the update path only |
| `proxy` | `Proxy::bind`, `ProxyRunner`, `ProxyHandle` |

Dependencies: `hyper` (`http1`, `http2`, `server`, `client`), `hyper-util`, `h2` **directly**
(the window, flood-limit and GOAWAY settings are not reachable through hyper's builders; h2 0.4.20
has one initial window per connection, no per-stream setter and no connection-only credit
release), `tokio`, `tokio-rustls`, `rustls`, `kbf-tls`, `kbf-peer`, `futures`, `httparse`. The
upstream pool is built on h2's own client, not on hyper's `conn::http2` or `hyper-util`'s
`client-legacy` (which keeps one h2 connection per host and never dials more at
`MAX_CONCURRENT_STREAMS`).

## 3. Public API

The crate's public API (abridged to the load-bearing parts; docs are in the code):

```rust
// limits (crate-private): the ONE h2 window of every stream, both directions, and the header cap.
const STREAM_WINDOW_BYTES: u32;              // 256 KiB
const HEADER_BYTES: u64;                     // 16 KiB, outside the window

// route and auth: pure.
pub fn route_of(path: &str) -> Route;        // Child(Target) | Ui | Update | Health | Ready | Metrics | Unknown
pub const fn auth_class(r: Route) -> Option<AuthClass>;
pub fn idle_of(path: &str) -> Idle;          // Never for Execute and WaitExecution, else 7200 s
pub fn decide(c: AuthClass, m: Buck2Auth, t: &Tokens, p: Presented<'_>) -> Result<(), Refusal>;

// identity: what the child sees.
pub fn identity_of(der: &[u8]) -> Result<Identity, IdentityError>; // role URI + the ONE plain DNS name
pub fn forward_headers(h: &mut HeaderMap, id: Option<&Identity>, client: Option<IpAddr>,
    s: &ProxySecret) -> Result<(), IdentityError>;  // strip x-kbf-*, then set node-id, role, ips, client-addr, proxy-auth
pub fn admin_headers(s: &ProxySecret) -> Result<HeaderMap, IdentityError>; // secret, no identity
pub fn strip_reserved(h: &mut HeaderMap);    // on every child answer

// replay
pub struct ReplayBuffer;  // NOT Copy. push(n) -> bool, end_request(), window_open(), replayable(), close()
pub enum Failure { Connect, RefusedStream, GoAway, Other }
pub fn may_retry(path: &str, f: Failure, b: &ReplayBuffer, attempts: u8) -> bool;

// control: the handler of `/v1/update/frontend*`. The built-in NoUpdates answers 503.
pub const MAX_UPDATE_BODY: usize;  pub const UPDATE_DEADLINE: Duration;   // 4096 bytes, 30 s
pub trait UpdateHandler: Send + Sync {       // commands idempotent; the proxy lets a handler finish after a 504
    fn handle(&self, r: UpdateRequest) -> impl Future<Output = UpdateReply> + Send; }
// config: ProxyConfig::from_toml_str, Frontend { name, base_port }, Limits (max_streams,
//         memory_budget_mib, Limits::stream_ceiling_bytes(), worst_case_bytes(): checked against
//         the budget at start). A table with the removed `upstream` table is read and written
//         back (`ProxyConfig::check`, `UpstreamTable`), and refused at the proxy's start
//         (`ProxyConfig::check_pool`) with SLOT_MODE_REMOVED.

pub struct Proxy;  impl Proxy {
    /// Takes the systemd-activated listener, builds the runtime, reads or creates the secret,
    /// binds the admin socket if `admin_listen` is set, and returns without serving. A config
    /// with no `[[frontend]]` table is refused.
    pub fn bind(cfg: ProxyConfig, listener: OwnedFd) -> io::Result<(ProxyHandle, ProxyRunner)>;
}
pub struct ProxyRunner;  impl ProxyRunner {
    /// Serves on the calling thread (a synchronous `main`) until `shutdown`.
    pub fn run<U: UpdateHandler + 'static>(self, updates: U) -> io::Result<()>;
}
pub struct ProxyHandle { /* Clone; no generics */ }
impl ProxyHandle {
    pub fn runtime(&self) -> tokio::runtime::Handle;   // the host's signal handlers run here, no second runtime
    pub fn open_streams(&self) -> u64;                 // restart-on-idle; a hint
    pub fn secret_origin(&self) -> SecretOrigin;       // Created | Existing
    pub fn admin_headers(&self) -> HeaderMap;          // the headers every call to a frontend's admin-call port carries
    pub fn reload_tls(&self) -> Result<(), TlsReloadError>; // what SIGHUP calls
    pub fn shutdown(&self);
}
```

There is no `set_ready`: readiness has one writer per member, the proxy's own poll of it.
`ProxySecret::create_or_read(path) -> io::Result<(ProxySecret, SecretOrigin)>` writes a temp file,
fsyncs and `link`s it into place, so a crash never leaves a 0-byte file; it adds no trailing
newline (the child requires exactly 32 bytes); an existing file that is not 32 bytes is replaced
the same way and reported `Created`.

## 4. Threading and async model

- One multi-thread tokio runtime built inside `Proxy::bind` (workers = min(cores, 8)), reachable
  as `ProxyHandle::runtime()`; `kbf-installer`'s `main` stays synchronous and calls
  `ProxyRunner::run`. One accept task; one task per connection; hyper spawns the per-stream
  service futures. **Panics:** each connection task is wrapped in `FutureExt::catch_unwind`, and
  each stream future is wrapped too (a stream panic is a tokio `JoinError`, not a connection
  close, so the connection wrap does not see it). Both increment `kbf_proxy_panics_total`; a
  panic closes that connection or resets that stream and nothing else. The wrap is the
  blast-radius limit, not error handling.
- The data path takes one short lock to place a stream (the members' state, `upstreams`: a pick,
  a drain and a count are never apart, and the lock is never held across an await); the counters
  are atomics. The other locks: the TLS config (`RwLock<Arc<ServerConfig>>`, read once per
  handshake) and the pool's connection list (per target, held for a pick, never across an await).
- Streams read the clock only through tokio's (paused time in tests).
- **Memory** is bounded by counts, and the counts are knobs. h2 0.4.20 has one initial window per
  connection and no per-stream setter, so every stream's window is `STREAM_WINDOW_BYTES`
  (256 KiB) in each direction, and the connection window is `1000 streams x window`: no set of
  unread bodies can exhaust it, so an update POST or `/healthz` on that connection never stalls on
  credit (a promise to the client; it costs memory only if data arrives, and arrival is capped by
  the stream counts). One stream pins at most `2 x window + replay buffer (64 KiB) + header list
  (16 KiB)` = 592 KiB (`Limits::stream_ceiling_bytes`); the replay term is held for a 3 hour
  Execute's whole life. `limits.max_streams` (default 8192; bound and dialing together; one more
  is refused `UNAVAILABLE` at header time) times that must fit `limits.memory_budget_mib`
  (default 8192, for a host of 16 GiB or more) or the config is refused at start: 4.75 GiB by
  default. The proxy runs with `OOMScoreAdjust=-900`, so the kernel kills the frontends before
  it, which is why the budget is checked, not hoped for. The price of one window: a single upload
  moves at most window/RTT (5 MB/s at 50 ms, 250 MB/s at 1 ms); parallel streams are unaffected.

## 5. Placement of a stream and its permit

A stream is placed on a member of the pool when its headers arrived (section 9, "Pool mode"), by
one pick under the members' lock, which counts the stream on the member in the same step. The
count is the stream's **permit** on the member (`MemberPermit`): it drops after both upstream
halves are finished, reset or dropped, never at the downstream END_STREAM, so a draining member's
count reaches 0 only when its last stream is over. A stream that waits for an upstream connection
keeps its permit from header time. `ProxyHandle::open_streams()` is the streams between accept and
the answer's head (`Pending`) plus the streams open on the members, read for `== 0`.

A request that no member takes (every member draining, or each that could take it has failed it)
is answered at once: trailers-only `UNAVAILABLE` for gRPC, 503 with `Retry-After: 1` for the
rest. Nothing waits for a member to appear.

## 7. The members' ports and the pool

**Ports.** A member's listeners are at fixed offsets from its `base_port`
(`[[proxy.frontend]]`, section 9): REAPI 0, worker 1, operator 2, admin calls 3 (reserved: a
frontend serves nothing there now), enrollment 4, all on `127.0.0.1` (the table cannot name
another host). The readiness poll is a `GET /readyz` over plain HTTP/1.1 to a member's
**operator** listener, the one every frontend already answers it on, once a second, with 3
seconds to answer; each member has its own, and it is the only writer of that member's readiness
(`/readyz` of the proxy is 200 while some member is up and ready).

**Pool**, per (member address, target). Prior-knowledge HTTP/2, no ALPN, on h2's own client. A
pick **reserves** a connection by CAS on its in-flight counter (`< advertised max`) before
sending, so pick and send are atomic and `h2` never queues behind one connection. A new
connection waits one PING round trip (the child sends SETTINGS before it answers) before it is
used, because h2 assumes 100 streams until the SETTINGS arrive. When every connection is full the
stream dials: dials in flight count against the cap of 256 per (member, target), at most 16 dial
at once, and waiters wake on "stream freed or dial finished", so a release does not make
thousands of tasks dial together and trip the 3 s connect timeout. A stream beyond the cap waits
and keeps its permit. **Members:** a member is a process that runs on its own, so its pools are
keyed by where it listens, made when first asked for and kept; a connection of a process that died
is found dead by the next stream that picks it (a send on it fails, or its socket closed), is
dropped, and the stream dials again. A child GOAWAY marks the connection draining (including the
graceful form with `last_stream_id = 2^31-1`), is never forwarded, and a stream refused above
`last_stream_id` is `Failure::GoAway`, retried; only a remote GOAWAY counts (one h2 raised itself
for a protocol error is not retried). `GoAway` and `Connect` come from typed errors
(`is_go_away()` with `is_remote()`, `TrySendError::take_message` returning the request unsent),
never from error strings. A child that stops accepting connections fails new streams with
`UNAVAILABLE` after 3 s; open streams are untouched. **Upstream keepalive:** PING every 30 s; an
answer later than 20 s marks the connection suspect (no new stream, the streams on it untouched,
and a late answer clears it); death is the socket closing, with a 5 minute backstop (a PING
unanswered for 5 minutes closes the connection, streams on it included).

## 8. TLS and identity

**Terminator.** One public listener, the activated socket (fd 3; refuse to start unless
`LISTEN_FDS == 1` and `LISTEN_PID` is this process). Not tonic's `Incoming`: a raw accept loop
wraps `TlsAcceptor` (`tokio-rustls`) over `kbf-tls`'s optional-client config. ALPN `h2`,
`http/1.1` (no ALPN is HTTP/1.1); TLS 1.3 early data off; handshake, preface and request-head
timeouts 10 s each. The client certificate is optional; a presented certificate that fails
verification fails the handshake (`WebPkiClientVerifier` with `allow_unauthenticated`). After the
handshake, `identity_of(leaf_der)` runs once per connection; a verified certificate with no usable
identity closes the connection (`kbf_proxy_identity_refused_total`).

**Node id rule:** the node id is the certificate's one DNS name. Zero, two or more, or a name with
anything but letters, digits, `-`, `_` and `.` (a wildcard, a space, a control character) make
`identity_of` fail with `NodeId(n)`, and the proxy closes the connection: an identity that must
be guessed is not one. The role is the one `urn:kbf:role:<name>` URI, via `kbf_peer::role_of`.
In `buck2_auth = "mtls"` the identity rule wins over "any verified certificate": a Buck2 client
certificate must have one DNS name and a role URI, or the connection is closed at the handshake.

**Reload.** SIGHUP re-reads the server certificates, keys and client CA bundle as one unit and
builds a new `ServerConfig`; it replaces the old one only if the whole build succeeds (a reload
also refuses an expired leaf; the first load does not). On failure the old config stays,
`kbf_proxy_tls_reload_failed_total` ticks, the error is logged without any file content. Only new
handshakes see the new config. One reload runs at a time (a lock from the read of the files to
the swap), so a SIGHUP storm cannot swap in the order the reloads finish. The `[proxy]` table, the
operator token and the Buck2 key are read at start; only the TLS files reload.

**Path-class auth** (`auth::decide`; table in `route`):

| Class | Paths | Rule |
|---|---|---|
| Worker | `/kbf.worker.*`, `/kbf.map.v1.Membership/`, `/kbf.update.*`, `/kbf.enroll.v1.ColdStore/` | verified certificate with role Daemon or Storage; none: `UNAUTHENTICATED`; other role: `PERMISSION_DENIED` |
| Reapi | REAPI services, ByteStream, `google.longrunning.Operations` | `none`: open; `mtls`: any verified certificate but a host updater's; `bearer`: the right token |
| Operator | other `/v1/*`, `/metrics` | operator token; a certificate with the role `updater` is no credential here (the frontend serves an updater its own-node calls) |
| Update | `/v1/update/frontend*` | operator token; then the built-in handler answers 503 |
| Probe | `/healthz`, `/readyz` | open |
| Ui | `/ui`, `/ui/...` | open: no certificate, no token; `GET` and `HEAD` only |
| Enroll | `/kbf.enroll.v1.Enroll/` | no certificate and no token at the proxy, in every mode; the child checks the enrollment token |

The modes do not combine. In `bearer` mode a certificate is neither required nor sufficient, and a
wrong token next to a good certificate is a refusal: one mechanism per mode, no fallback to a
weaker one. In `none` mode a verified certificate is still turned into identity headers (the
child decides what a daemon may do). A certificate of the wrong role is `PERMISSION_DENIED` on the
worker class: the peer is known, and `kbf-peer` documents the same split.

**Bearer.** Read only from `Authorization: Bearer`, never a query string, never logged
(`HeaderValue::set_sensitive`; the log layer drops the header). The limit on wrong tokens runs
before any hashing and counts wrong tokens, not calls (a Buck2 client sends its key on every call,
thousands a second): ten in a row per address, one earned back per second; an address past it is
refused (`RESOURCE_EXHAUSTED`, 429). `Token` compares SHA-256 digests in constant time
(`subtle`), so length does not leak. The limit is per source address: behind a shared address
(NAT, an overlay gateway) one client sending a wrong token a second keeps the address at its burst
and so blocks the operator and Buck2 keys from that address too. An IPv6 client is keyed by its
full address. Token files follow the child's rules (regular file, this user, mode 0600 or 0400, at
least 32 visible ASCII characters).

**Closed route table.** A path matching no prefix is `Unknown`, answered by the proxy (404, or
`UNIMPLEMENTED` for gRPC) and never forwarded. `/v1/update/frontend` matches on a path boundary,
so `/v1/update/frontendx` is an operator path. **Enrollment.** `Enroll` is served by the
frontend's enrollment listener (`--enroll-listen`), which authenticates the server only: the
caller proves itself with the enrollment token or, for `Renew`, the key the registry holds.
`ColdStore` is on the worker listener (mutual TLS, storage role only), so
`/kbf.enroll.v1.ColdStore/` is a **worker** path (the proxy checks a worker or storage
certificate; the child's role table narrows it to storage), and `/kbf.enroll.v1.Enroll/` is the
`Enroll` target at a member's `base_port + 4`, with its own auth class: **no certificate and no
token at the proxy** in every `buck2_auth` mode (a node enrolling has no certificate; `mtls` must
not lock the door it comes in by). The proto says `Enroll` and `Renew` come without a client
certificate, so a client that presents a certificate must present a valid one: a certificate that
fails verification is a handshake error here as everywhere. The proxy applies no limit to
`Enroll`, and a child sees only the proxy's loopback address, so the enrollment listener limits
globally or by token, never by peer address.

**Identity to the child** (`identity::forward_headers`, the exact contract of
`kbf-server/src/proxy_auth.rs`): strip every inbound `x-kbf-*`; with a verified certificate set
`x-kbf-node-id` and `x-kbf-role` (`PeerRole::name`) and, when the leaf names IP addresses,
`x-kbf-node-ips` (sorted canonical text, comma separated, at most 16;
[frontend-proxy.md](frontend-proxy.md) section 3); set `x-kbf-client-addr` to the connection's
address (canonical IP text, no port; the proxy's reading, never the client's); always set
`x-kbf-proxy-auth` to the 64-char lower-case hex of the secret. Each header ends with exactly one
value. Child answers go through `strip_reserved` before the client sees them. The upstream
connection is loopback, so the child's loopback check holds.

**Secret.** `ProxySecret` is 32 bytes. `ProxySecret::create_or_read` returns the secret and
`SecretOrigin::{Created, Existing}`: it reads `/run/kbf-proxy/proxy-auth` if present (so it
survives a proxy restart and frontends that run on keep working), else writes a temp file (0600,
no trailing newline), fsyncs, and `link`s it into place (a second proxy racing the creation fails
with `AlreadyExists` and is restarted by its unit). A file with another owner or a mode other than
0600 or 0400 is an error, not a repair, whatever its length. `Proxy::bind` creates the secret
last, after the tokens and TLS files loaded, so a start that exits has not made a secret that
frontends could read. The directory is owned by the unit (`RuntimeDirectory=kbf-proxy`,
`RuntimeDirectoryPreserve=yes`). Frontends read it by `--proxy-auth-file`; never argv or env.
Admin calls carry it too (`admin_headers`).

## 9. Config schema (`installer.toml`, table `[proxy]`)

```toml
[proxy]
operator_token_file = "/etc/kbf/api-token"   # read at start
client_ca_file   = "/etc/kbf/tls/ca.pem"   # verifies client certificates, whatever name they asked for
# Server certificates, one of two forms (not both).
# Single certificate, served for every name:
server_cert_file = "/etc/kbf/tls/server.pem"
server_key_file  = "/etc/kbf/tls/server.key"
# Or a list chosen by SNI (the ClientHello's server name), in [[proxy.server_cert]] tables below.
buck2_auth = "none"                  # none | bearer | mtls
buck2_token_file = "/etc/kbf/buck2.token"   # required for bearer
proxy_auth_file = "/run/kbf-proxy/proxy-auth"
# state_dir, hold_max_s and remote_operator_file belonged to the two-slot supervision and the CI
# credential of the update API: they are still accepted, so that a file written before keeps
# starting the proxy, and nothing reads them.
ui = true                            # the read-only web UI at /ui: open (no certificate, no token), GET and HEAD only; false makes /ui a 404 at the proxy
# The list form of the server certificates, in place of server_cert_file/server_key_file:
# [[proxy.server_cert]]
# names = ["reapi.example.com"]        # DNS names, each in the certificate's SANs
# cert_file = "/etc/kbf/tls/reapi.pem"
# key_file  = "/etc/kbf/tls/reapi.key"
# default = true                        # exactly one when there are two or more
# [[proxy.server_cert]]
# names = ["farm.internal.example"]
# cert_file = "/etc/kbf/tls/worker.pem"
# key_file  = "/etc/kbf/tls/worker.key"
[proxy.limits]                       # every key optional; defaults shown
pre_stream_timeout_s = 10   max_connections = 50000   max_connections_per_ip = 2000   max_streams = 8192
max_header_list_bytes = 16384   keepalive_interval_s = 30   keepalive_ack_s = 20
```

**Server certificates.** `server_cert_file` + `server_key_file` is the same as one
`[[proxy.server_cert]]` with no names and `default = true`, so a config written before SNI stays
valid. With a list, a ClientHello whose server name equals a `names` entry (case-insensitive,
trailing dot ignored) gets that entry's certificate; no server name, or a name that is in no
entry, gets the entry marked `default = true`. Two or more entries need exactly one default; an
entry without `names` must be the default. At start, and at every reload, each pair is checked:
the key is the certificate's, every configured name is a DNS subject alternative name of the leaf
(a `*.rest` SAN covers one label), no name appears twice, and the shape rules above hold; a reload
also refuses an expired leaf. Any failure refuses the whole list: a start does not proceed, and a
reload keeps the previous one. Client certificates are verified against `client_ca_file` and the
per-path rules apply, the same on every name.

**Pool mode.** The children are given as `[[proxy.frontend]]` tables, at least one: frontends that
run on their own, which the proxy balances over, with no supervisor, no swap and no hold. The
`[proxy.upstream]` table of the two-slot supervision is refused at the proxy's start
(`ProxyConfig::check_pool`, which `kbf-installer proxy` calls) with a message that says the
supervision was removed and what to give instead (`SLOT_MODE_REMOVED`). `ProxyConfig::check`,
which `kbf-installer setup` and the release fetcher read the file with, accepts the table and
writes it back, so a file written before the removal is not refused where the proxy is not
started.

```toml
[proxy]
admin_listen = "127.0.0.1:18090"     # optional, loopback only, plain HTTP, no credential
[[proxy.frontend]]
name = "a"                           # 1 to 64 of letters, digits, '-' and '_'; unique
base_port = 18000                    # 1024 to 65531; the offsets of a member's listeners (REAPI 0, worker 1, operator 2, admin 3, enroll 4)
[[proxy.frontend]]
name = "b"
base_port = 18010                    # at least 5 above or below any other frontend's
```

A stream goes to the frontend with the fewest streams open among those that are up (not draining)
and ready; frontends with as many are taken in turn. A frontend is ready while its last
`GET /readyz` on its operator listener, asked once a second, passed; a frontend that is up but not
ready is taken only when none is ready, and one that cannot be connected to is not ready until a
poll passes again. A draining frontend is never taken and its open streams run to their end. A
stream that fails before any answer (a refused connection, `REFUSED_STREAM`, a GOAWAY that proves
it unprocessed) is placed once more on a frontend that has not failed it, if the request is whole
(the rules of section 10). `/readyz` is 200 while some frontend is up and ready. With every
frontend draining or failed, a call is `UNAVAILABLE`. A `WaitExecution` is routed like any call,
because every frontend of the store answers an operation by name. `GET /metrics` is the proxy's
own series followed by `kbf_proxy_member_up`, `kbf_proxy_member_ready`,
`kbf_proxy_member_open_streams` (by `member` and `target`), `kbf_proxy_member_picked_total` and
`kbf_proxy_member_failed_total` for each frontend, and no series of a frontend's own.

The admin API is served on `admin_listen` and on no other socket: `GET /v1/proxy/upstreams` lists
`{"name", "state": "up"|"draining", "ready", "open_streams": {"reapi", "worker", "operator",
"enroll"}}` for each frontend; `POST /v1/proxy/upstreams/{name}:drain` and `...:undrain` change the
state, are idempotent and answer the same list; an unknown name is a 404 and changes nothing. The
API asks for no credential, and a request a web page could have sent is a 403: one with an
`Origin` header, or whose `Host` is not an IP literal on loopback (use the address of
`admin_listen`; a DNS name that was made to point at it does not pass). `admin_listen` must not be
a port of a frontend (`base_port` to `base_port + 4`). A frontend leaves the pool for an update by
being drained, waited for until its `open_streams` are 0, retired (`POST /v1/frontend/retire`),
restarted, waited for until it is ready, and undrained: `kbf-updater` does it
([../deploy/kbf-updater.md](../deploy/kbf-updater.md)).

Unknown keys are refused at start. The operator token is read by the proxy itself.

## 10. Keepalive, retry, GOAWAY and resets

- **No stream deadlines.** No route timeout, no max stream duration. Idle timeout: `idle_of(path)`
  is `Never` for `Execution/Execute` and `Execution/WaitExecution` (matched by full method path)
  and 7200 s for the rest.
- **Dead peers.** Downstream PING every 30 s, ack within 20 s; a failed ack closes that connection
  only. It closes dead peers, never a live stream, so it fits the no-timeout rule. Upstream PING
  marks a connection suspect and closes it only after 5 minutes without an answer (section 7).
- **Replay buffer:** request bytes are kept from the first byte, at most 64 KB, as `Bytes` clones,
  freed at `close()`. A 3 hour Execute keeps its buffer until the first response frame, since tonic
  sends headers lazily. The window closes for good at the upstream's response head, or when a frame
  would overflow it. The list holds at most 64 frames and no empty DATA frame. `may_retry` is true
  only for: not ByteStream, fewer than 2 dispatches, window open **and the client's END_STREAM
  seen** (`replayable`), failure in `{Connect, RefusedStream, GoAway}`. An open bidirectional
  request (`Worker.Session`) is never re-dispatched: no remainder is spliced onto a new stream.
- **A retry is a new placement**, not a reuse of the member: the old upstream stream is reset, the
  old permit dropped, and the stream is picked again with the members that failed it left out
  (`attempts + 1`). With no member left the stream is `UNAVAILABLE`. Two attempts at most, so no
  loop.
- **Resets, in the order applied.** (1) `REFUSED_STREAM` or a GOAWAY above `last_stream_id`:
  `may_retry` first, before any mapping. (2) `RST_STREAM(NO_ERROR)` after the child sent a complete
  response: not an error, forward the response. (3) A reset on a non-gRPC path (`/v1/*`,
  `/metrics`): 502 on h1 or `RST(INTERNAL_ERROR)` on h2, never a gRPC trailer. (4) A gRPC path,
  headers not yet sent: trailers-only `UNAVAILABLE`. (5) Headers sent: a trailers frame with
  `UNAVAILABLE` follows the data; a data frame cut mid-message is followed by the same non-OK
  trailer, which tonic reports as a status error, never as a short OK message. Client RST:
  propagated upstream as `RST_STREAM(CANCEL)`; the permit drops after it is sent. **Child
  GOAWAY:** never forwarded. **The proxy's own GOAWAY:** only on its shutdown and on a connection
  that trips a flood limit (a misbehaving connection only); a drain sends none.
- **Flow control.** One window for every stream, 256 KiB (`STREAM_WINDOW_BYTES`), in both
  directions: h2 0.4.20 cannot grow one stream's window after dispatch. A stream returns credit as
  the other side consumes (`release_capacity`), and a stream that is not read returns none. The
  connection window is 1000 x window, so no combination of unread bodies stalls a probe on the
  connection. Adaptive BDP is off. Raising the window to buy upload speed raises `max_streams x
  window` in step; the memory-budget check at start enforces it.
- **h2 flood limits.** Derived from the 1000-stream cap, not left at the defaults of about 20: a
  Buck2 build cancel resets hundreds of streams at once, and tripping the limit would GOAWAY the
  connection with its other streams on it. `max_pending_accept_reset_streams` is 2 x 1000; the
  header-list cap and the 16 KiB frame size set the CONTINUATION limits, which h2 0.4.20 derives
  from them (it has no setter of their own); concurrent streams per connection 1000. The
  rapid-reset limit counts resets read before the proxy accepts the stream, so it stops a burst; a
  slow-roll attacker whose resets are accepted promptly is not tripped (the 1000-stream cap and the
  per-IP connection cap bound it). The upstream client takes no flood setting: the children are
  our own processes and push is off.

## 11. Pre-stream limits and the verb

Handshake, h2 preface and h1 request-head timeouts: 10 s each (`pre_stream_timeout_s`);
`max_connections`, `max_connections_per_ip`; per-address limit on wrong tokens before any hashing.
The h2 preface timeout is not separate: nothing bounds the time from a finished handshake to the
first h2 frame beyond the keepalive. **HTTP/1.1** is served only for `/v1/*`, the update path and the
probes; hyper's HTTP/1.1 server parses the head (with `httparse`), at most
16 KB and 100 headers, origin-form targets only (`CONNECT`, absolute-form and `Upgrade` are 400),
and a gRPC path on h1 is 426.

**Verb.** `kbf-installer proxy [--config PATH] [--check]`: `--config` defaults to
`/etc/kbf/installer.toml`; `--check` loads `installer.toml` the way a run does, through
`InstallerToml::parse_tolerant` (the confirm windows, the fetch timeout, `database` and the
`[proxy]` table, not only the table) and exits 0 or 2 without binding. A key the verb does not
read is not checked, so `--check` (the `ExecStartPre` of `kbf-proxy.service`) does not catch a
misspelt key of the rest of the file; `setup` and the root run helper, which read the file closed,
refuse it. A malformed `[proxy]` table still makes the root run helper refuse `installer.toml`,
since both check the one table. No other flag: everything else is the `[proxy]` table, since
configuration is flags and files, not the environment. The two environment variables it reads
(`LISTEN_FDS`, `LISTEN_PID`) are the platform's socket-activation markers. `needs_root` is false.
The verb serves TLS: `server_cert_file` and `server_key_file` (or the `[[proxy.server_cert]]`
list), `client_ca_file` and `operator_token_file` are required, and a start without readable files
exits with the bind error.

**Layering.** `kbf-installer` links `kbf-proxy` and with it tokio, hyper and rustls in the same
binary as the root helper. A layering test keeps the root helper's modules (`helper/`, `run`) and
every file outside the dispatch from naming `kbf_proxy` or the verb's module `proxy_verb`.

## 12. Decisions that still hold

| # | Choice | Reason |
|---|---|---|
| C1 | The pool is h2c prior-knowledge per (member, target), up to 256 connections each, CAS reserve at pick, 16 dials at once, dial before queue; a queued stream keeps its permit | the child's per-connection stream limit would otherwise queue thousands of streams behind long ones |
| C4 | h1 only for `/v1/*`, the probes and the update path; head <= 16 KB, 100 headers, origin-form; gRPC on h1 is 426 | h1 cannot multiplex |
| C5 | One replay buffer for the retry; whole request only | one rule, no state to keep in step |
| C7 | Idle is chosen by the full method path of Execute and WaitExecution | generic routing needs no per-stream state |
| C8 | Modes do not combine; a wrong-role certificate is `PERMISSION_DENIED`; a wrong token never falls back to a certificate | no downgrade to a weaker mechanism |
| C9 | The proxy reads `operator_token_file` itself; the update path takes the same `Token` | one credential for the operator surface |
| C10 | Static table is `base_port` plus fixed offsets; one readiness writer per member (the poll, every 1 s, 3 s timeout); readiness only steers the next pick and never touches an open stream | a health failure does not touch open streams |
| C12 | SIGHUP reloads certificates, key and CA as one unit; a failed reload keeps the old one and counts | a half-applied reload would break handshakes |
| C13 | The proxy creates (temp + link) or reads the secret file and reports `Created` or `Existing`; the unit owns `/run/kbf-proxy/` | the proxy restarts on its own |

The node id is the certificate's one DNS name (section 8), and the route table is closed
(section 8).

## 14. Soak

`crates/kbf-integration-test/tests/proxy_soak.rs` runs the proxy in pool mode in front of two real
`kbf frontend` processes (each with a `kbf storage` node behind it, file stores under a scratch
directory; no Postgres, no object store), and drains one under load. 200 long-lived `Execute`
streams are open through one client connection, spread over the two frontends; one frontend is
drained through the admin API; every old stream stays open on its frontend and runs to its end
there (the client cancels them, and each cancel reaches the frontend that has the stream: its own
count of open executions falls), and every stream opened after the drain is served by the other.
The frontends' own counts (`open_executions` of `GET /v1/frontend/drain`) make every figure the
child's own. It proves that streams stay where they were placed and that a drained frontend takes
no new one; it does not prove a `Write` or a `Read` completing. The signal handling of the verb is
tested in `kbf-installer/tests/proxy_verb.rs`.
