# REAPI client authentication

`kbf-server` checks a bearer token on every call to its REAPI listener (`--listen`)
when it is started with `--reapi-token-file <path>`. The file names the principals that
may call and holds the SHA-256 digest of each one's token, never a token. Without the
flag the listener checks no credential, as before: whoever reaches it can read and write
the CAS and Execute actions, so it is meant for a listener bound to loopback that only
trusted callers on the host reach.

TLS ends at the front (see the security model in
[ARCHITECTURE.md](../ARCHITECTURE.md#security-model)). The front passes the
`authorization` header through and checks nothing; `kbf-server` checks it behind the
front, in plain text on loopback or on the mesh interface. A peer address is not
trusted: behind a proxy on the same host every caller is loopback.

## What is checked

Every method of every service on the REAPI listener (Capabilities, the CAS, ByteStream,
the action cache and Execution), and any path the listener does not serve, goes through
one layer over the whole router. A call is served only when:

- it has exactly one `authorization` header;
- its value is `Bearer <token>` (the scheme in any case, one space, then the token with
  nothing after it);
- the token's SHA-256 digest is one of the file's entries as last read. It is compared in
  constant time with every entry.

Anything else is `UNAUTHENTICATED`, with a message that says how to configure the
client. The message never echoes the header.

A served call carries its principal in the request: Execute submits the work at the
principal's QoS (the third field of its line) instead of `ci`, and logs one `Execute`
event at INFO with `principal="<name>"` and the action digest. The token and its digest
are never logged.

## The token file

One line per token, `#` starts a comment:

```text
<principal> client <qos> sha256:<64 hex>
```

- `principal`: 1 to 64 characters from `A-Z a-z 0-9 . _ @ -`. A principal may have
  several lines, one per token.
- `client`: the only role. Every principal may Execute, read and write the CAS and read
  the action cache. No client writes the action cache (`UpdateActionResult` is
  `PERMISSION_DENIED` for everyone).
- `qos`: `interactive`, `ci` or `batch`, the level the principal's work is queued at.
- the digest of the token. Two lines with the same digest are refused.

`kbf-server hash-token <principal> [--qos <level>]` reads a token on stdin and prints
its line, so nobody hashes by hand:

```sh
head -c 32 /dev/urandom | base64 > dev.token            # the client keeps this
kbf-server hash-token dev --qos interactive < dev.token >> tokens.next
```

A token is at least 32 visible ASCII characters.

The file must be a regular file owned by the server's user, mode `0600` or `0400`, at
most 1 MiB, UTF-8, and every line must parse. Otherwise the server refuses to start
(exit 2, `--reapi-token-file: ...`).

## Changing the file while the server runs

The server looks at the file's metadata at most once a second and reads it again when
it changed. No restart is needed for any edit.

**Fail closed.** While the file is missing, breaks a rule above (for example, it was
`chmod`-ed to `0644`) or does not parse, every call is refused `UNAUTHENTICATED` with
a message that says the server's token file is unusable. The server logs the reason at
ERROR, once per change. No entry of an earlier read is kept. Fixing the file restores
service within a second.

**Write a new file and rename it over the old one**, in the same directory, with the
same owner and mode. An edit in place can be read half-written, and one that keeps the
size within the filesystem's timestamp granularity can go unseen until the next change.

```sh
install -m 0600 /dev/null tokens.next
cat tokens > tokens.next            # then edit tokens.next
mv tokens.next tokens               # rename(2): readers see the old file or the new one
```

**Rotating a token**, with no restart and no failed call:

1. Add a line for the new token (same principal, new digest) and rename the file in.
2. Move every client of that principal to the new token (for Buck2, restart its daemon,
   below).
3. Remove the old line and rename the file in. The old token is refused within a second.

Revoking a token is step 3 alone.

## Client configuration

**Buck2**, in `.buckconfig` or a `.buckconfig.local` that is not committed:

```ini
[buck2_re_client]
http_headers = Authorization: Bearer <token>
```

A running Buck2 daemon keeps the headers it started with: run `buck2 kill` after a
change.

**Bazel**:

```sh
bazel build --remote_executor=grpcs://<front> --remote_header=Authorization="Bearer <token>" //...
```

## Daemons

Today a daemon reads action inputs and writes outputs through the REAPI listener its
`--cas` names, and it sends no token. A server started with `--reapi-token-file`
refuses those calls, so daemons whose `--cas` points at it cannot run actions. The
planned fix moves daemon blob traffic to the mutual-TLS worker listener, which needs no
REAPI token. The worker listener itself is not affected by this flag.
