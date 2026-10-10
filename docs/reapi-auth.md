# REAPI authentication and authorization

`kbf-server` checks REAPI callers in two layers, after
[Buildbarn](https://github.com/buildbarn/bb-storage)'s model, and reads both from one
policy file whose names follow Buildbarn's configuration. A Buildbarn operator can
port an `authenticationPolicy` and its authorizers by renaming the keys this page
lists.

- **Authentication** runs once per call, before the call reaches a service. It
  answers who is calling, as *authentication metadata*, or refuses the call
  UNAUTHENTICATED. For a streaming call (ByteStream Write, Execute) it runs once,
  when the call arrives, not once per message.
- **Authorization** runs inside the services, once per call, after the request's
  instance name is read and before any store work. It answers whether this caller may
  make this kind of call on this instance name, or refuses it PERMISSION_DENIED.

The two meet only through the metadata, which the server carries with the request.

The policy covers the REAPI listener (`--listen`) and nothing else. The worker
listener keeps its own mutual TLS and deny list, and the operator API its own token
([api.md](api.md)); neither reads the policy file. On the REAPI listener, the
`grpc.health.v1.Health` service is answered without the policy, so a proxy's health
check needs no credentials ([api.md](api.md#grpchealthv1-on-the-reapi-listener));
every other call, a path the listener does not serve included, runs it.

**Without a policy file, nothing changes:** every call is accepted with empty
metadata and every authorizer allows. A farm whose front already limits who can
reach the REAPI listener can run that way.

**What a front does.** A front (a load balancer, a mesh ingress, an HTTP/2 proxy)
terminates TLS and balances load. To `kbf-server` it is a client of the REAPI
listener, over plain text or over the listener's own TLS (`--reapi-tls-cert`,
`--reapi-tls-key`, a server certificate only); nothing in kbf names or trusts a
particular front. The policy runs the same way over TLS and over plain text. The policies built
so far read no header and no certificate, so a front passes nothing they need. The
planned credential policies (below) read the call's `authorization` header or its
TLS client certificate.

## The flag

```
kbf-server --reapi-auth-policy /etc/kbf/reapi-auth.json ...
```

The file is read once, at start. A file that cannot be read, is not JSON, or is not a
policy stops the server before anything is bound (exit 2), and the error names the
flag, the file, and where in the JSON the mistake is, as a path from the root `$`:

```
kbf-server: --reapi-auth-policy: /etc/kbf/reapi-auth.json: $.authenticationPolicy.jwt: `jwt` is not supported yet; the variants built are allow, deny, any, all
```

Changing the file takes a restart.

## The file

JSON, with Buildbarn's protojson field names (camelCase). Each choice of Buildbarn's
(a protobuf `oneof`) is an object with exactly one key, the variant; an object with
none or with two is refused. Every object refuses keys it does not know, so a typo is
an error, not an allow.

```json
{
  "authenticationPolicy": {
    "allow": { "public": { "user": "anonymous" } }
  },
  "capabilitiesAuthorizer": { "allow": {} },
  "contentAddressableStorage": {
    "getAuthorizer": { "allow": {} },
    "putAuthorizer": { "instanceNamePrefix": { "allowedInstanceNamePrefixes": ["ci"] } },
    "findMissingAuthorizer": { "allow": {} }
  },
  "actionCache": {
    "getAuthorizer": { "allow": {} }
  },
  "executeAuthorizer": { "instanceNamePrefix": { "allowedInstanceNamePrefixes": ["ci"] } }
}
```

`authenticationPolicy` is required (as in Buildbarn, where a missing policy is an
error). Every authorizer key is optional and defaults to `allow`.

## Authentication policies

| Variant | Built | What it does |
|---|---|---|
| `allow` | yes | Accepts every call, with the metadata given: `{"public": <any JSON>, "private": <any JSON>}`, either optional. |
| `deny` | yes | Refuses every call UNAUTHENTICATED, with the string given as the message: `{"deny": "no REAPI callers"}`. |
| `any` | yes | `{"policies": [...]}`. Asks each policy in order; the first that accepts answers, and the rest are not asked. See below for its refusal. |
| `all` | yes | `{"policies": [...]}`. Every policy must accept; they are asked in order and the first refusal is the call's. Their metadata is merged in order (below). |
| `jwt` | planned | A bearer JWT in the `authorization` header. |
| `tlsClientCertificate` | planned | A TLS client certificate. The REAPI listener can serve TLS, but asks clients for no certificate, so this also needs it to take a client CA. |
| `remote` | planned | Asks a remote authentication service, with a cache. |
| `peerCredentialsJmespathExpression` | not applicable | UNIX-socket peers; kbf has no UNIX-socket REAPI listener. |

The last four, and `tracingAttributes` inside `allow`, are refused as "not supported
yet", so a file written for a later `kbf-server` is never half-read.

**The metadata.** `public` may be shown: `kbf-server` logs it when it refuses a call
and puts it on the Execute trace. `private` is for authorizers only and is never
logged. Each is any JSON value.

**`any`'s refusal.** When every policy refuses, the call fails with the first error
whose code is not UNAUTHENTICATED, if there was one, so a policy that could not run
(a backend it needs is down) is not reported as "who are you". Otherwise it fails
UNAUTHENTICATED with every policy's message, in order, joined by `", "`. An `any`
with no policies refuses every call; an `any` of one policy is that policy.

**`all`'s merge.** For `public` and `private` separately: when the earlier value and
the later one are both JSON objects, their keys are merged and the later value wins
for a key both have (one level deep, not recursively); otherwise the later value
replaces the earlier one when it has one. An `all` needs at least one policy.

## Authorizers

| Key | Calls it covers |
|---|---|
| `capabilitiesAuthorizer` | GetCapabilities |
| `contentAddressableStorage.findMissingAuthorizer` | FindMissingBlobs |
| `contentAddressableStorage.getAuthorizer` | BatchReadBlobs, GetTree, ByteStream Read, SplitBlob, GetChunkMapping |
| `contentAddressableStorage.putAuthorizer` | BatchUpdateBlobs, ByteStream Write, ByteStream QueryWriteStatus, SpliceBlob, RegisterChunkMapping |
| `actionCache.getAuthorizer` | GetActionResult |
| `executeAuthorizer` | Execute, and WaitExecution against the instance name the operation was submitted under |
| none | UpdateActionResult: always PERMISSION_DENIED |

Each call is authorized against its request's instance name: `instance_name` in the
REAPI messages; for ByteStream, every segment of the resource name before `blobs/`
(Read) or `uploads/` (Write, QueryWriteStatus); for RegisterChunkMapping, the first
message's. WaitExecution looks the operation up first: an unknown name is NOT_FOUND,
whoever asks. The chunking calls are authorized, then answer UNIMPLEMENTED as before.

The reads Execute makes for itself (the action cache lookup, the action, its input
tree) are not authorized again, as in Buildbarn.

| Variant | Built | What it does |
|---|---|---|
| `allow` | yes | `{}`. Allows everything. |
| `deny` | yes | `{}`. Refuses everything. |
| `instanceNamePrefix` | yes | `{"allowedInstanceNamePrefixes": [...]}`. Allows an instance name under one of the prefixes, whole path components at a time: `a` allows `a` and `a/b`, not `ab`. The empty prefix allows every name, the empty name included. A prefix with an empty component (`/a`, `a/`, `a//b`) is refused. It does not look at the caller. |
| `jmespathExpression` | planned | Allows when a JMESPath expression over the caller's metadata and the instance name is true. |
| `remote` | planned | Asks a remote authorization service, with a cache. |

**Instance names gate calls; they do not separate data.** One cell has one cache, and
the CAS and the action cache ignore the instance name when they store or find a blob
or a result. An `instanceNamePrefix` authorizer on the CAS or the action cache decides
which calls are served, but a caller allowed on `x` can read a blob uploaded under
`y` if it knows the digest. Execute does use the instance name: identical actions
share one run only within one instance.

## Where kbf differs from Buildbarn

- **`actionCache.putAuthorizer` is refused.** Clients never write the action cache;
  UpdateActionResult is PERMISSION_DENIED whatever the policy says. Only the daemon
  that ran an action writes its result.
- **One policy file for one listener,** with every authorizer at its top level or
  under `contentAddressableStorage` and `actionCache`; Buildbarn spreads them over
  its per-store and scheduler configurations.
- **`capabilitiesAuthorizer` is its own key.**
- **QueryWriteStatus, SpliceBlob and RegisterChunkMapping count as writes**, and
  SplitBlob and GetChunkMapping as reads.
- **An `all` with no policies is refused** rather than accepted.

## Errors and logs

- An authentication refusal answers the call with the policy's own status
  (UNAUTHENTICATED for `deny`). It is logged at WARN, `REAPI call not authenticated`,
  with the call's gRPC path and the message.
- An authorization refusal keeps the authorizer's code (PERMISSION_DENIED for the
  built ones) with the message prefixed `Authorization: `, so a `deny` reads
  `Authorization: Permission denied`. It is logged at WARN, `REAPI call refused`,
  with the call's gRPC path, the instance name, and the caller's `public` metadata.
- No log line carries the `private` metadata.
- Every Execute runs in an INFO trace span `execute` with the fields `instance` and
  `caller` (the `public` metadata as one line of JSON, `-` when there is none); its
  DEBUG events say whether the action cache answered or which operation was
  submitted.

## Not built yet

- The credential policies (`jwt`, `tlsClientCertificate`, `remote`) and the
  `jmespathExpression` and `remote` authorizers, as above.
- Reading the policy file again when it changes.
- Tracing attributes from the metadata.
- Using the metadata to choose a QoS or for fair sharing between callers (in the
  manner of Buildbarn's invocation keys taken from authentication metadata). Every
  Execute is still QoS `ci`.
