# The `bas-api` C ABI

The DSH half never re-implements BAS behaviour. It loads `libbas_api` and calls
one function. This document is the whole contract between the two halves, so
either side can be replaced without reading the other's source.

## Why a C ABI and not a Node addon

The Rust part must not depend on a harness platform (the request that created
this split). A C ABI is the narrowest portable surface:

| Host | How it reaches the Rust part |
|---|---|
| DeepSeek Harness plugin (Node) | `koffi` FFI, `lib/native.js` |
| Python | `ctypes.CDLL("libbas_api.so")` |
| Go | `cgo` |
| shell / anything | `bas-cli` (a thin wrapper over the same ops) |

A Node-API addon would have made the Rust part a Node artifact, which is
exactly the coupling this design removes.

## The exported functions

```c
char       *bas_call(const char *op, const char *args_json);
void        bas_free(char *response);
const char *bas_version(void);
```

- `op` — NUL-terminated UTF-8 operation name (`"tunnel_open"`).
- `args_json` — NUL-terminated UTF-8 JSON object. `"{}"` when the op takes
  nothing. `NULL` is treated as `"{}"`.
- **`bas_call` returns a NUL-terminated JSON envelope the caller owns**, or an
  error envelope — never `NULL`.
- **`bas_free` releases exactly that pointer, once.** `NULL` is a no-op.

That is the whole ownership rule: whoever receives a pointer frees it once.

### Why not the usual two-call probe

The common C ABI pattern is `bas_call(op, args, NULL, 0)` to learn the size,
then a second call to fill a buffer. It is wrong here: to report a size the
implementation has to *run the operation*, so the operation runs twice. Two
`tunnel_open` calls open two tunnels; two `bridge_ensure` calls start two
daemons. An earlier draft of this file had that bug, and the second daemon was
observable in the dev space.

Returning an owned string costs one allocation and removes the trap. A host
that cannot allocate is out of memory anyway.

```c
const char *bas_version(void);   /* static, never freed, e.g. "0.7.0" */
```

A panic inside any op is caught and returned as an `internal` error; it never
unwinds across the boundary.

## The response envelope

Every response is one JSON object:

```json
{"ok": true,  "data": <op-specific>}
{"ok": false, "error": {"code": "<code>", "message": "<prose>"}}
```

`code` is stable and branchable: `invalid`, `upstream`, `transport`,
`token_expired`, `not_found`, `port_in_use`, `bad_body`, `status`, `other`,
`internal`. The message is for a human, never for control flow.

## Statefulness

The core (`bas-core`) is a pure protocol layer: every function takes its
inputs — token, URL, key, path, port — and returns data. It owns no token, no
file, no "current landscape", and no cache.

Two things are inherently live and therefore **handle-based** in `bas-api`:

| Resource | Create | Inspect | Destroy |
|---|---|---|---|
| dev-channel tunnel | `tunnel_open` | `tunnel_status`, `tunnel_list` | `tunnel_close` |
| browser sign-in listener | `login_begin` | `login_poll` | `login_cancel` |

Every handle is returned by its create call and is the only way to touch the
resource afterwards. Nothing is implicit: there is no "the connected tunnel"
and no default landscape, so a host with several sessions cannot collide with
itself. A handle from another process is meaningless — handles are not portable
identifiers, and the host keeps them in its own process.

Closing is idempotent: closing an unknown or already-closed handle reports
`ok` with `{"closed": false}`.

## Operations

Every op's args and data are camelCase JSON.

### Pure helpers (no state, no I/O)

| op | args | data |
|---|---|---|
| `normalize_landscape` | `{landscape}` | `{landscape, host, label, credentialId}` |
| `jwt_describe` | `{jwt, nowMs?, skewMs?}` | `JwtInfo` (`valid`, `remainingMs`, `expiresAt`, `subject`) |
| `ext_login_url` | `{landscape, cacheBuster?}` | `{url}` |
| `ssh_config_block` | `{section, identityFile, port, user?, hostName?}` | `{block}` |
| `ssh_config_publish` | `{target, section, identityFile, port, user?, header?}` | `{written, changed, section, target, reason, block}` |
| `ssh_config_unpublish` | `{target, section}` | `{changed}` |
| `ssh_config_list` | `{target}` | `{sections: [string]}` |
| `port_free` | `{port}` | `{free}` |

`ssh_config_publish` refuses to write a symlink, a non-file, or an unwritable
file — a nix/home-manager managed `~/.ssh/config` is left untouched, `written`
is `false` and `reason` says why, instead of failing the call. A missing target
(including a missing parent directory such as a fresh `~/.ssh`) is created, not
refused.

### Landscape REST (stateless: the caller passes the token)

| op | args | data |
|---|---|---|
| `devspaces` | `{landscape, jwt, probeSsh?}` | `{devSpaces: [DevSpace]}` |
| `devspace` | `{landscape, jwt, id}` | `{devSpace}` |
| `devspace_start` | `{landscape, jwt, id, label?, waitMs?, maxActive?}` | `{devSpace, started, alreadyRunning}` |
| `devspace_stop` | `{landscape, jwt, id, waitMs?}` | `{devSpace, stopped, alreadyStopped}` |
| `devspace_key` | `{landscape, jwt, id}` | `{key, wsUrl, startupUrl}` |

`devspace_start` waits for `RUNNING` (or `ERROR`) up to `waitMs`, defaulting to
240 s. **`waitMs: 0` means "send the request and report what BAS says now"** —
it does not mean "use the default", because a UI that polls the dev-space
listing itself must not be held for minutes. It refuses to start a third dev space when `maxActive` (default 2)
spaces are already running or starting, and names the spaces holding the slots
— BAS enforces the same limit.

### Dev channel

| op | args | data |
|---|---|---|
| `tunnel_open` | `{landscape, jwt, wsUrl, localPort?, username?, connectAttempts?, timeoutMs?}` | `{tunnelId, status}` |
| `tunnel_status` | `{tunnelId}` | `{status}` |
| `tunnel_list` | `{}` | `{tunnels: [TunnelStatus]}` |
| `tunnel_close` | `{tunnelId}` | `{closed}` |
| `forward_add` | `{tunnelId, remotePort, localPort?}` | `{localPort}` |
| `forward_remove` | `{tunnelId, remotePort}` | `{removed}` |

`TunnelStatus`: `{id, serverUri, localPort, username, sshHost, sshPort, active,
startedAt, closedReason, forwards: [{remotePort, localPort}]}`.

`tunnel_open` defaults: `localPort: 0` (kernel picks), `username: "user"`,
`connectAttempts: 3`, `timeoutMs: 30000` per handshake attempt.

`tunnel_open` retries the handshake `connectAttempts` times (default 3) with a
backoff, because the dev channel intermittently answers "Failed to read the
protocol version" on a fresh or busy dev space.

### SSH inside the dev space

These take a loopback port that a tunnel already forwards (or any SSH port):

| op | args | data |
|---|---|---|
| `ssh_exec` | `{port, key, username?, command, timeoutMs?}` | `ExecOutput` |
| `ssh_stat` | `{port, key, username?, path}` | `{exists, isDir, size, mode}` |

`key` is the PEM text, not a path: the protocol layer never reads files. A host
that has the key in a file reads it itself.

### SFTP bridge (a server the dev space has none of)

| op | args | data |
|---|---|---|
| `bridge_ensure` | `{port, key, username?, remotePort?, dir?, install?, timeoutMs?}` | `BridgeInfo` |
| `bridge_state` | `{port, key, username?, remotePort?, dir?}` | `BridgeState` |
| `bridge_stop` | `{port, key, username?, remotePort?, dir?}` | `{stopped}` |

A BAS dev space runs dropbear without an `sftp-server`, so SFTP fails with exit
code 127 and every SFTP-based remote tool breaks. `bridge_ensure` uploads a
small Node SSH server into the dev space, installs `ssh2` there once, starts it
on the dev space's loopback and proves it answers before reporting success.

### Browser sign-in hand-off

| op | args | data |
|---|---|---|
| `login_begin` | `{landscape, host?, port?}` | `{loginId, url, boundPort, landscape}` |
| `login_poll` | `{loginId}` | `{state, jwt?, iasjwt?, error?}` |
| `login_cancel` | `{loginId}` | `{cancelled}` |

`state` is `waiting`, `done`, `consumed`, or `failed`. The JWT is handed back
exactly once (`done` then `consumed`); the host stores it in the credential
store and drops it from memory. `login_poll` never blocks, so the host chooses
the cadence — `lib/native.js` polls every 750 ms. The listener gives up after
180 s and reports `failed` with a reason.

## Versioning

`bas_version()` returns the crate version. `lib/native.js` compares the
`major.minor` it was written against with the loaded library and refuses a
mismatch, so a stale `.so` next to a new plugin fails loudly instead of
misbehaving.

`bas_call` and `bas_free` are the only two symbols a host needs; everything
else is operation names in `op`.
