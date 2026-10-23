# bas-remote

SAP Business Application Studio (BAS) remote dev spaces for
[DeepSeek Harness](https://github.com/deepseek-ai/deepseek-harness).

Reproduces the
["Remote Access for SAP Business Application Studio"](https://github.com/SAP/app-studio-toolkit)
workflow without VS Code: sign in to a BAS landscape, list its dev spaces, and
open an SSH dev-channel tunnel that any SSH client or the harness
remote-workspace tools (`rw_connect`) can use.

## Layout: a library, then a host

The BAS protocol does not depend on a harness. The repository is the library
first; each harness gets a host directory, and this one happens to be DeepSeek
Harness:

```
crates/bas-core/     protocol layer — landscape REST, dev-channel SSH-over-WebSocket,
                     port forwarding, SFTP bridge, SSH config text.
                     No filesystem, no terminal, no environment (enforced by
                     crates/bas-core/tests/purity.rs).
crates/bas-api/      the C ABI over bas-core: bas_call / bas_free / bas_version.
                     Owns the two things a protocol layer must not: effects
                     (files, listeners) and live handles (tunnels, sign-ins).
crates/bas-cli/      `bas` — the same operations from a shell, with no harness.
hosts/dsh/           the DeepSeek Harness host: its own package.json, the
                     cordis plugin (lib/index.js), the Web UI page (lib/client.js),
                     the FFI loader (lib/native.js), and its tests.
```

Only `hosts/*/` knows what a harness is. A second host (another harness, a
desktop app, a service) is a sibling directory that links the same C ABI and
ships none of this folder's code.

`docs/abi.md` is the whole contract between the halves. Any host — Node,
Python, Go, a shell script — can call the same library, which is the point of
the split:

| Host | Reaches the Rust part through |
|---|---|
| this DSH plugin | `koffi` FFI (`lib/native.js`) |
| Python | `ctypes.CDLL("libbas_api.so")` |
| Go | `cgo` |
| shell | `bas-cli` |

## Build

```sh
cargo build --release -p bas-api   # the library
cd hosts/dsh
npm install
npm test                           # host tests; the seam test skips without the library
npm run test:native                # ABI + SSH integration (needs BAS_TEST_SSH_*)
```

The plugin finds the library at `$BAS_NATIVE_LIB`, then `native/`, then `lib/`,
then the package root. A checkout can point `BAS_NATIVE_LIB` straight at
`target/release/libbas_api.so`.

Nothing is loaded at import time: a plugin whose library is missing still
registers its tools, and each call fails with `unavailable — <how to build it>`.

## Features

- **Landscape sign-in** — browser hand-off (`/ext-login.html`) with a loopback
  token listener that hands the token over exactly once.
- **Dev-space listing** — one card per space with running/starting/stopped
  state and whether its SSH key is being served (probed, not guessed).
- **Dev-space lifecycle** — start (resume) and stop (suspend) a dev space the
  same way the VS Code Dev Space Manager does; `bas_connect` starts a stopped
  space before opening the tunnel.
- **Dev-channel tunnel** — SSH over `wss://port33765-<host>:443`, written
  directly against the protocol (russh + tokio-tungstenite) instead of the
  `@microsoft/dev-tunnels-ssh` JavaScript stack.
- **SSH endpoint publishing** — off by default; opt in to a fragment file this
  plugin owns, or to editing `~/.ssh/config` itself. A nix/home-manager managed
  config is never written; the block is reported instead.
- **SFTP where the dev space has none** — see [SFTP bridge](#sftp-bridge).
- **Model tools** — `bas_status`, `bas_login`, `bas_logout`, `bas_devspaces`,
  `bas_start`, `bas_stop`, `bas_connect`, `bas_disconnect`, `bas_bridge`,
  `bas_forget`.
- **Human command** —
  `/bas status|login|devspaces|start|stop|connect|disconnect|logout|forget|bridge|ports`.
- **Web panel** — settings section with one-click start / stop / connect /
  disconnect and live polling, plus a floating chip showing live tunnels.

## What a connection actually is

Connecting a dev space does not open a proprietary API session — it ends in an
ordinary SSH endpoint. The dev channel is a standard SSH transport wrapped in a
WebSocket, because that is the only egress the landscape exposes:

1. the landscape hands out the dev space's runtime URL, and its SSH private key
   comes from `GET <runtime startup url>/key`,
2. the library opens `wss://port33765-<host>:443`, presents the landscape JWT as
   a bearer token, and speaks the SSH transport protocol over it — with no SSH
   authentication of its own, because the WebSocket bearer already authorised
   the caller,
3. `direct-tcpip` channels forward a loopback TCP port to the dev space's sshd
   on `127.0.0.1:2222`,
4. the key is written to the SSH directory and the `Host` block is published
   according to `sshConfigMode` (by default only reported, so a managed
   `~/.ssh/config` is never rewritten).

### What a BAS dev space can and cannot serve

The dev space's own sshd is **dropbear** and the image ships no `sftp-server`
(`/usr/lib/sftp-server` does not exist; the image has no root and `/usr/lib` is
read-only), so over the plain dev-channel endpoint:

| Capability | Works? | Evidence over a live tunnel |
|------------|--------|-----------------------------|
| exec channels (`ssh <cmd>`, `rw_exec`) | yes | `whoami` → `user`, `HOME=/home/user` |
| forwarding to another dev-space port | yes | dropbear answers on the forwarded port |
| SFTP subsystem | **no** | `sftp` → `bash: /usr/lib/sftp-server: No such file or directory` |
| legacy `scp -O` | yes | push verified by `md5sum` inside the dev space |
| `tar` over an exec channel | yes | `tar` is installed; no `rsync` |

That is why the plugin runs its own SSH server **inside** the dev space and
reports *its* endpoint for `rw_connect`.

## SFTP bridge

A BAS dev space has no SFTP server and no way to install one, so the library
brings its own:

1. a small Node SSH server (embedded in `bas-core`, `assets/bridge-server.cjs`)
   is uploaded to `~/.bas-remote/sftp-bridge/` in the dev space and `ssh2`
   is installed once with the dev space's own npm;
2. an ed25519 host key is derived from the dev-space key the landscape already
   issued, so setup needs no crypto tooling inside the dev space;
3. the daemon listens on `127.0.0.1:2223` *inside* the dev space (never on an
   external interface) as the dev-space user;
4. the dev channel forwards that loopback port to a second local port, and the
   whole path is verified (forward → handshake → `stat("/")`) before it is
   advertised.

| Port | Serves | Use for |
|------|--------|---------|
| dropbear (the dev space's own sshd) | exec, port forwarding | `ssh`, `scp -O`, `tar \| ssh` |
| SFTP bridge | exec **and** SFTP | `rw_connect` (all `rw_*` tools) |

| Config | Default | Meaning |
|--------|---------|---------|
| `sftpBridge` | `true` | provide SFTP in connected dev spaces |
| `sftpBridgePort` | `2223` | port the bridge listens on inside the dev space |
| `sftpBridgeDir` | `.bas-remote/sftp-bridge` | where it is installed (`~`-relative) |
| `sftpBridgeInstall` | `true` | allow the one-time `npm install ssh2` in the dev space |
| `sftpBridgeTimeoutMs` | `300000` | ceiling for the first-time setup |

If the bridge cannot start, the connect still succeeds, the result says why, and
the endpoint stays exec-only.

## SSH endpoint publishing

| `sshConfigMode` | Effect |
|-----------------|--------|
| `off` *(default)* | no file is touched; `bas_connect` reports the key path, the endpoint and the exact `Host` block to paste |
| `fragment` | maintains `<sshDir>/bas-remote.conf` (marked blocks, cleaned up on disconnect); add `Include ~/.ssh/bas-remote.conf` |
| `config` | edits `sshConfigPath` (default `~/.ssh/config`) between this plugin's markers |

In every mode the write is refused when the target is a symlink (nix store,
home-manager), not a regular file, or not writable; `bas_connect` then reports
the reason and the block instead of failing.

## Dev-space lifecycle

| Action | Call |
|--------|------|
| start (resume) | `PUT <landscape>/ws-manager/api/v1/workspace/<id>` with `{"Suspended": false, ...}` |
| stop (suspend) | the same `PUT` with `{"Suspended": true, ...}` |

BAS runs at most **two** dev spaces per landscape at a time — the toolkit
enforces the same limit — so a start is refused with the names of the spaces
holding the slots.

## Tests

| Suite | What it proves |
|-------|----------------|
| `cargo test --workspace` | core units, ABI envelope, purity of the protocol layer, and an end-to-end **SSH-over-WebSocket** server built in-process (`crates/bas-core/tests/devchannel_loopback.rs`) |
| `npm test` in `hosts/dsh/` | the Web UI, the port routes, and the **host ↔ native seam** against the real library |
| `npm run test:native` in `hosts/dsh/` | the ABI against a real SSH server: exec/exit codes, background-child commands, stat, SSH config round-trip |

## Upstream reference

- **SAP/app-studio-toolkit** — https://github.com/SAP/app-studio-toolkit
  - `packages/app-studio-remote-access` — the extension whose sign-in,
    dev-space and dev-channel behaviour this mirrors without VS Code;
  - `packages/app-studio-toolkit/src/devspace-manager` — start/stop.

## License

Apache-2.0 (matching the upstream SAP extension).
