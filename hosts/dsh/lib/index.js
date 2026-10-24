// bas-remote — SAP Business Application Studio (BAS) remote dev spaces for
// DeepSeek Harness.
//
// Host half. This plugin turns a BAS landscape into usable remote SSH
// endpoints:
//
//   • sign in to a landscape (browser hand-off, the token is stored in the
//     harness credential store — never in configuration),
//   • list the landscape's dev spaces and their state,
//   • connect a dev space: fetch its SSH private key, open the dev-channel
//     tunnel (`wss://port33765-<host>:443` → the dev space's sshd on
//     127.0.0.1:2222) and publish it as a loopback SSH endpoint plus a
//     `~/.ssh/config` entry,
//   • hand that endpoint to the harness remote-workspace tools (rw_connect),
//     plain `ssh`, or any IDE.
//
// This file owns no BAS protocol logic. Every call that talks to a landscape,
// a dev channel or a dev space goes through `lib/native.js` → `libbas_api`
// (docs/abi.md). What stays here is host policy: where the token lives, which
// ports are assigned, which files may be written, and what a human is told.
//
// Plugin Config MUST be a schemastery schema.

import { existsSync, mkdirSync, readFileSync, renameSync, rmSync, writeFileSync } from 'node:fs'
import { homedir } from 'node:os'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'
import z from '@deepseek-ai/schemastery'

// Read version from package.json at load time
const __dirname_local = dirname(fileURLToPath(import.meta.url))
let PLUGIN_VERSION = 'unknown'
try {
  const pkg = JSON.parse(readFileSync(join(__dirname_local, '..', 'package.json'), 'utf8'))
  PLUGIN_VERSION = pkg.version || 'unknown'
} catch { /* ignore */ }
import { defineTool } from '@deepseek-ai/dsh-tools'
import { credentialKey } from '@deepseek-ai/dsh-credentials'

import { call, nativeStatus } from './native.js'

export const name = 'bas-remote'

// `commands` is deliberately absent: it is an optional service, and declaring
// it here would turn "this harness has no command registry" into "bas-remote
// does not load at all" — losing the tools, the web panel and the tunnels over
// a surface the harness may simply not have. The command is registered through
// ctx.reflect.get('commands') below, which skips it when the service is absent.
export const inject = ['tools', 'credentials', 'systemPrompt']

/** Status a dev space reports while its runtime is usable. */
const DEV_SPACE_RUNNING = 'RUNNING'
/** Status a dev space reports while it is down. */
const DEV_SPACE_STOPPED = 'STOPPED'
/** Default port the landscape login page posts the JWT back to. */
const DEFAULT_LOGIN_PORT = 55532
/** Default port the SFTP bridge listens on inside a dev space. */
const DEFAULT_BRIDGE_PORT = 2223
/** Default bridge directory inside a dev space (`~` when relative). */
const DEFAULT_BRIDGE_DIR = '.bas-remote/sftp-bridge'

export const Config = z.object({
  /** Interface the landscape login page posts its token back to. */
  loginHost: z.string().default('127.0.0.1'),
  /** Port for that hand-off. The BAS login page always posts to 55532; change
   * this only together with a port forward that keeps 55532 reachable. */
  loginPort: z.number().step(1).min(1).max(65535).default(DEFAULT_LOGIN_PORT),
  /** Default ceiling for one awaited sign-in. */
  loginTimeoutMs: z.number().step(1).min(1000).default(180000),
  /** Ceiling for waiting on a dev-space start/stop to reach a final status. */
  devSpaceTimeoutMs: z.number().step(1).min(1000).default(240000),
  /** SSH config file to publish dev-space endpoints into (`config` mode). */
  sshConfigPath: z.string().default(''),
  /** Directory holding the fetched dev-space private keys. */
  sshDir: z.string().default(''),
  /** SSH user presented to a dev space (BAS always uses `user`). */
  sshUser: z.string().default('user'),
  /**
   * Where dev-space SSH endpoints are published:
   * `off` (default) writes nothing and reports the block to paste instead,
   * `fragment` maintains `<sshDir>/bas-remote.conf` for an `Include`,
   * `config` adds a marked block to `sshConfigPath`/`~/.ssh/config`.
   * A nix/home-manager managed config is left untouched in every mode.
   */
  sshConfigMode: z.string().default('off'),
  /** Fragment file used in `fragment` mode (default `<sshDir>/bas-remote.conf`). */
  sshConfigFragmentPath: z.string().default(''),
  /** Deprecated alias for `sshConfigMode: 'config'`; leave false. */
  manageSshConfig: z.boolean().default(false),
  /** Delete a dev space's key file on disconnect. */
  removeKeyOnDisconnect: z.boolean().default(false),
  /** Loopback port for the tunnel (0 asks the kernel for a free one). */
  localPort: z.number().step(1).min(0).max(65535).default(0),
  /**
   * When true, connecting a dev space with a port that is already in use
   * fails instead of falling back to a random port.
   */
  forwardPortsStrict: z.boolean().default(true),
  /** How often a failed dev-channel handshake is retried before giving up. */
  connectAttempts: z.number().step(1).min(1).max(5).default(3),
  /**
   * Serve SFTP in connected dev spaces.
   *
   * A BAS dev space runs dropbear without `sftp-server`, so SFTP over the dev
   * channel fails with exit code 127 and every SFTP-based remote tool breaks.
   * When enabled, a small SSH server carrying exec + SFTP is uploaded into the
   * dev space and forwarded over the same dev channel; `rw_connect` then points
   * at that endpoint and mirror/sync work. Requires node and npm inside the dev
   * space (BAS images have both).
   */
  sftpBridge: z.boolean().default(true),
  /** Port that bridge listens on inside the dev space. */
  sftpBridgePort: z.number().step(1).min(1).max(65535).default(DEFAULT_BRIDGE_PORT),
  /** Bridge directory in the dev space (`~` when relative). */
  sftpBridgeDir: z.string().default(DEFAULT_BRIDGE_DIR),
  /** Allow the one-time `npm install ssh2` inside the dev space. */
  sftpBridgeInstall: z.boolean().default(true),
  /** Ceiling for the first-time bridge setup (upload + npm install). */
  sftpBridgeTimeoutMs: z.number().step(1).min(10000).default(300000),
  /** Inject an active-dev-space note into the system prompt. */
  promptSection: z.boolean().default(true),
  /** Landscape used when a tool call omits one, and pre-registered on load. */
  defaultLandscape: z.string().default(''),
  /** Verbose host-side diagnostics. */
  debug: z.boolean().default(false),
})

const OWNER = 'bas-remote'

function dshHome() {
  return process.env.DSH_HOME || join(homedir(), '.dsh')
}

function stateFile() {
  return join(dshHome(), OWNER, 'state.json')
}

function readState() {
  try {
    const parsed = JSON.parse(readFileSync(stateFile(), 'utf8'))
    // `removedForwardPorts` was written by 0.5.0+2 only and is deliberately
    // dropped here, so the next persist() cleans it out of the file.
    return {
      landscapes: Array.isArray(parsed?.landscapes) ? parsed.landscapes : [],
      lastDevSpace: typeof parsed?.lastDevSpace === 'string' ? parsed.lastDevSpace : '',
      forwardPorts: parsed?.forwardPorts || {},
      forwardPortsStrict: typeof parsed?.forwardPortsStrict === 'boolean' ? parsed.forwardPortsStrict : undefined,
    }
  } catch {
    return { landscapes: [], lastDevSpace: '', forwardPorts: {} }
  }
}

function writeState(state) {
  const file = stateFile()
  mkdirSync(dirname(file), { recursive: true, mode: 0o700 })
  const temp = `${file}.tmp`
  writeFileSync(temp, `${JSON.stringify(state, null, 2)}\n`, { mode: 0o600 })
  renameSync(temp, file)
}

/**
 * Whether an occupied fixed port is an error (strict) or falls back to random.
 * The live toggle wins over the configured default.
 * @param {object} config - plugin config.
 * @param {object} state - plugin state.
 * @returns {boolean} the effective strictness.
 */
function effectiveStrict(config, state) {
  return state.forwardPortsStrict !== undefined ? state.forwardPortsStrict : config.forwardPortsStrict
}

/**
 * Validate and normalise one port assignment coming from the UI or the CLI.
 *
 * The schema used to cover this while the assignments were also a config key;
 * with `state` as the only home they would otherwise be written unchecked.
 * `0` is a legitimate value: it means "let the kernel choose", and it is what
 * the settings page stores for a blank field.
 * @param {*} value - candidate assignment.
 * @param {string} wsId - dev space id, for the error message.
 * @returns {{dropbear: number, bridge: number}} the checked assignment.
 * @throws when the shape or a port is unusable.
 */
function checkForwardPorts(value, wsId) {
  if (value === null || typeof value !== 'object' || Array.isArray(value)) {
    throw new Error(`ports for ${wsId} must be an object like { dropbear: 44000, bridge: 44001 }`)
  }
  const ports = {}
  for (const kind of ['dropbear', 'bridge']) {
    const port = value[kind]
    if (!Number.isInteger(port) || port < 0 || port > 65535) {
      throw new Error(`${kind} port for ${wsId} must be an integer 0-65535 (0 = random), got ${JSON.stringify(port)}`)
    }
    ports[kind] = port
  }
  return ports
}

function formatDuration(ms) {
  if (!Number.isFinite(ms)) return 'unknown'
  const seconds = Math.max(0, Math.round(ms / 1000))
  if (seconds < 60) return `${seconds}s`
  const minutes = Math.floor(seconds / 60)
  if (minutes < 60) return `${minutes}m`
  return `${Math.floor(minutes / 60)}h${minutes % 60}m`
}

/**
 * Wait, without blocking the event loop.
 * @param {number} ms - delay.
 * @param {boolean} [unref] - do not hold the process open for this timer.
 * @returns {Promise<void>} resolves after the delay.
 */
function delay(ms, unref = false) {
  return new Promise((resolve) => {
    const timer = setTimeout(resolve, ms)
    if (unref) timer.unref?.()
  })
}

// ── SSH paths and key files ────────────────────────────────────────────────
//
// Paths and key files are host concerns, not protocol: the ABI's `ssh_config_*`
// ops take and return text, so the reading and writing stays here.

/** @returns {string} the default SSH config file. */
function defaultSshConfigPath() {
  return join(homedir(), '.ssh', 'config')
}

/**
 * Resolve the effective SSH paths, honouring explicit overrides.
 * @param {object} [options] - path overrides.
 * @param {string} [options.sshConfigPath] - explicit config file path.
 * @param {string} [options.sshDir] - explicit key directory.
 * @returns {{configPath: string, dir: string}} resolved paths.
 */
function resolveSshPaths({ sshConfigPath = '', sshDir = '' } = {}) {
  const configPath = sshConfigPath || defaultSshConfigPath()
  return { configPath, dir: sshDir || dirname(configPath) }
}

/**
 * The `Host` alias used for one dev space, matching the SAP extension:
 * `<landscape host>.<dev space id>`.
 * @param {string} landscapeHost - `host[:port]` of the landscape.
 * @param {string} wsId - dev space id.
 * @returns {string} the SSH host alias.
 */
function sshHostAlias(landscapeHost, wsId) {
  return `${landscapeHost}.${wsId}`
}

/**
 * The private-key file name used for one dev space, matching the SAP
 * extension: `<dev space host>.key` inside the SSH directory.
 * @param {string} dir - SSH directory.
 * @param {string} wsUrl - dev-space runtime base URL.
 * @returns {string} absolute key path.
 */
function keyFilePath(dir, wsUrl) {
  const host = new URL(wsUrl).host
  return join(dir, `${host}.key`)
}

/**
 * Write a private key with owner-only permissions.
 * @param {string} file - destination path.
 * @param {string} key - PEM private key.
 * @returns {string} the path written.
 */
function writeKeyFile(file, key) {
  mkdirSync(dirname(file), { recursive: true, mode: 0o700 })
  const temp = `${file}.tmp`
  writeFileSync(temp, key, { mode: 0o600 })
  renameSync(temp, file)
  return file
}

/**
 * Delete a key file, ignoring absence.
 * @param {string} file - key path.
 * @returns {boolean} whether a file was removed.
 */
function removeKeyFile(file) {
  if (!existsSync(file)) return false
  rmSync(file, { force: true })
  return true
}

function textTool() {
  return {
    schema: {
      type: 'object',
      additionalProperties: false,
      properties: { text: { type: 'string', required: true } },
    },
    render: (_args, value) => [{ type: 'text', text: String(value?.text ?? '') }],
  }
}

function sendJson(res, status, body) {
  const payload = JSON.stringify(body)
  res.statusCode = status
  res.setHeader('content-type', 'application/json; charset=utf-8')
  res.setHeader('cache-control', 'no-store')
  res.end(payload)
}

async function readBody(req) {
  const chunks = []
  let size = 0
  for await (const chunk of req) {
    size += chunk.length
    if (size > 1024 * 1024) throw new Error('request body too large')
    chunks.push(Buffer.from(chunk))
  }
  const raw = Buffer.concat(chunks).toString('utf8').trim()
  if (!raw) return {}
  try {
    return JSON.parse(raw)
  } catch {
    return Object.fromEntries(new URLSearchParams(raw))
  }
}

/**
 * Register the same JSON routes on both carriers: the Web host's HTTP server
 * and, when present, the harness `connection` fetch channel used by Desktop.
 * @param {object} ctx - cordis context.
 * @param {Array} routes - `{path, handler(req, res)}` entries, all POST-or-GET.
 */
function registerTransports(ctx, routes) {
  ctx.inject(['webServer'], (inner) => {
    const disposers = routes.map((route) =>
      inner.get('webServer').register({ kind: 'exact', path: route.path, handler: route.handler }),
    )
    inner.effect(() => () => disposers.forEach((dispose) => dispose()), 'bas-remote.web-routes')
  })

  ctx.inject(['connection'], (inner) => {
    const connection = inner.get('connection')
    if (!connection || typeof connection.fetch?.register !== 'function') return
    const disposers = []
    for (const route of routes) {
      try {
        disposers.push(
          connection.fetch.register({
            path: `/api${route.path}`,
            methods: ['GET', 'POST'],
            requestBody: 'buffered',
            async fetch(request) {
              const url = new URL(request.url)
              let body = ''
              if (request.body) body = await request.text()
              const req = Object.assign(
                (function* () {
                  if (body) yield Buffer.from(body)
                })(),
                { method: request.method, url: `${route.path}${url.search}`, headers: request.headers },
              )
              const headers = new Headers()
              let status = 200
              let payload = ''
              const res = {
                setHeader(key, value) {
                  headers.set(key, value)
                },
                end(value) {
                  payload = value === undefined ? '' : String(value)
                },
                get statusCode() {
                  return status
                },
                set statusCode(value) {
                  status = value
                },
              }
              await route.handler(req, res)
              return new Response(payload, { status, headers })
            },
          }),
        )
      } catch (error) {
        console.warn(`[bas-remote] route ${route.path} was not registered: ${error.message}`)
      }
    }
    inner.effect(() => () => disposers.forEach((dispose) => dispose()), 'bas-remote.fetch-routes')
  })
}

/**
 * Host plugin entry point.
 * @param {object} ctx - cordis context.
 * @param {object} config - resolved plugin config.
 */
export function apply(ctx, config) {
  // `forwardPorts` was removed from the schema in 0.5.0+3: port assignments are
  // machine-local runtime data and live in state.json, edited from the settings
  // page or `/bas ports`. Schemastery passes unknown keys through untouched, so
  // a leftover profile key would otherwise be ignored silently and the pinned
  // ports would quietly go back to random — fail loudly instead.
  if (config.forwardPorts && Object.keys(config.forwardPorts).length > 0) {
    throw new Error(
      'forwardPorts is no longer a plugin config key (removed in 0.5.0+3). '
      + 'Port assignments now live in ~/.dsh/bas-remote/state.json: set them in '
      + 'Settings > BAS > Port settings, or with `/bas ports set <devSpace> <dropbear> <bridge>`. '
      + 'Remove `forwardPorts` from the profile config to continue.',
    )
  }

  const paths = resolveSshPaths({ sshConfigPath: config.sshConfigPath, sshDir: config.sshDir })
  const log = (level, message) => {
    if (config.debug || level === 'warn') console.warn(`[bas-remote] ${message}`)
  }

  let state = readState()
  /** @type {Map<string, object>} active tunnels keyed by `<landscape>/<devSpace id>`. */
  const tunnels = new Map()
  /** @type {object|null} the most recent sign-in attempt. */
  let pendingLogin = null

  function persist() {
    writeState(state)
  }

  // ── landscape registry ────────────────────────────────────────────────────

  /**
   * Normalize one landscape reference through the protocol layer.
   * @param {string} input - landscape URL or host.
   * @returns {Promise<{landscape: string, host: string, label: string, credentialId: string}>} its canonical parts.
   */
  async function describeLandscape(input) {
    return call('normalize_landscape', { landscape: String(input ?? '') })
  }

  async function registerLandscape(input) {
    const described = await describeLandscape(input)
    if (!state.landscapes.some((entry) => entry.url === described.landscape)) {
      state.landscapes.push({
        url: described.landscape,
        label: described.label,
        credentialId: described.credentialId,
        addedAt: Date.now(),
      })
      persist()
    }
    return described.landscape
  }

  async function findLandscape(input) {
    const described = await describeLandscape(input)
    return state.landscapes.find((entry) => entry.url === described.landscape) ?? null
  }

  async function forgetLandscape(input) {
    const described = await describeLandscape(input)
    state.landscapes = state.landscapes.filter((entry) => entry.url !== described.landscape)
    persist()
    return described.landscape
  }

  async function resolveLandscape(input) {
    const raw = String(input ?? '').trim()
    if (raw) return (await findLandscape(raw))?.url ?? (await registerLandscape(raw))
    if (state.landscapes.length === 1) return state.landscapes[0].url
    throw new Error(
      state.landscapes.length === 0
        ? 'no BAS landscape is configured yet; call bas_login with the landscape URL first'
        : `several landscapes are configured (${state.landscapes.map((entry) => entry.url).join(', ')}); name one`,
    )
  }

  /** @param {string} landscape - normalized landscape origin. @returns {object|null} its state entry. */
  function landscapeEntry(landscape) {
    return state.landscapes.find((entry) => entry.url === landscape) ?? null
  }

  /**
   * The credential-store key of one landscape.
   * @param {string} landscape - normalized landscape origin.
   * @returns {Promise<string>} the `<owner>/<id>` key.
   */
  async function credentialFor(landscape) {
    const known = landscapeEntry(landscape)?.credentialId
    const id = known || (await describeLandscape(landscape)).credentialId
    return credentialKey(OWNER, id)
  }

  async function readCredential(landscape) {
    try {
      const record = await ctx.credentials.readRecord(await credentialFor(landscape))
      if (!record || record.kind !== 'grant') return null
      const payload = record.payload ?? {}
      if (!payload.jwt) return null
      return {
        jwt: String(payload.jwt),
        iasjwt: payload.iasjwt ? String(payload.iasjwt) : '',
        obtainedAt: Number(payload.obtainedAt ?? 0),
      }
    } catch (error) {
      log('warn', `cannot read stored credential: ${error.message}`)
      return null
    }
  }

  async function storeCredential(landscape, payload) {
    await ctx.credentials.modifyRecord(await credentialFor(landscape), async () => ({
      kind: 'grant',
      payload: {
        landscape,
        jwt: payload.jwt,
        iasjwt: payload.iasjwt ?? '',
        obtainedAt: Date.now(),
      },
    }))
  }

  async function dropCredential(landscape) {
    try {
      await ctx.credentials.deleteRecord(await credentialFor(landscape))
    } catch (error) {
      log('warn', `cannot delete stored credential: ${error.message}`)
    }
  }

  async function requireCredential(landscape) {
    const credential = await readCredential(landscape)
    if (!credential) {
      const host = (await describeLandscape(landscape)).host
      throw new Error(`${host} is not signed in; run /bas login ${landscape} (or the bas_login tool) first`)
    }
    const info = await call('jwt_describe', { jwt: credential.jwt, skewMs: 60000 })
    if (!info?.valid) {
      const host = (await describeLandscape(landscape)).host
      throw new Error(`the stored token for ${host} expired; sign in again with /bas login ${landscape}`)
    }
    return credential
  }

  // ── sign-in hand-off ──────────────────────────────────────────────────────

  function closeLogin() {
    const entry = pendingLogin
    if (!entry || entry.settled) return
    entry.cancelled = true
    entry.settled = true
    entry.error = entry.error || 'sign-in cancelled'
    entry.finishedAt = Date.now()
    if (entry.loginId) void call('login_cancel', { loginId: entry.loginId }).catch(() => {})
  }

  async function startLogin(input) {
    const landscape = await registerLandscape(input)
    if (pendingLogin && pendingLogin.landscape === landscape && !pendingLogin.settled) return pendingLogin
    closeLogin()
    const begun = await call('login_begin', {
      landscape,
      host: config.loginHost,
      port: config.loginPort,
    })
    const entry = {
      landscape,
      loginId: begun.loginId,
      url: begun.url,
      boundPort: begun.boundPort,
      startedAt: Date.now(),
      settled: false,
      cancelled: false,
      error: '',
      finishedAt: 0,
    }
    entry.promise = (async () => {
      // `login_poll` never blocks; the hand-off is awaited by polling it, so a
      // browser that never comes back cannot pin a worker thread.
      for (;;) {
        if (entry.cancelled) return
        let result
        try {
          result = await call('login_poll', { loginId: entry.loginId })
        } catch (error) {
          entry.settled = true
          entry.error = error.message
          entry.finishedAt = Date.now()
          return
        }
        if (result?.state === 'done') {
          try {
            await storeCredential(landscape, result)
            log('info', `signed in to ${(await describeLandscape(landscape)).host}`)
          } catch (error) {
            entry.error = `the token arrived but could not be stored: ${error.message}`
          }
          entry.settled = true
          entry.finishedAt = Date.now()
          return
        }
        if (result?.state === 'failed') {
          entry.settled = true
          entry.error = result.error || 'the landscape rejected the sign-in'
          entry.finishedAt = Date.now()
          return
        }
        await delay(750, true)
      }
    })()
    pendingLogin = entry
    log('info', `waiting for the ${(await describeLandscape(landscape)).host} sign-in hand-off on ${config.loginHost}:${begun.boundPort}`)
    return entry
  }

  async function awaitLogin(entry, timeoutMs) {
    if (entry.settled) return entry
    const limit = Number.isFinite(timeoutMs) && timeoutMs > 0 ? timeoutMs : config.loginTimeoutMs
    let timer
    try {
      await Promise.race([
        entry.promise,
        new Promise((resolve) => { timer = setTimeout(resolve, limit) }),
      ])
    } finally {
      clearTimeout(timer)
    }
    return entry
  }

  // ── dev spaces ────────────────────────────────────────────────────────────

  /**
   * Cached `/key` probe results, keyed by `<landscape>/<devSpace id>`.
   *
   * `devspaces` can probe SSH availability itself, but that is one HTTP request
   * per RUNNING dev space and the settings page polls every few seconds, so the
   * answer is reused for a minute.
   * @type {Map<string, {value: boolean, at: number}>}
   */
  const sshProbeCache = new Map()
  const SSH_PROBE_TTL_MS = 60000

  /** @param {string} landscape - normalized landscape origin. @param {string} wsId - dev space id. */
  function forgetSshProbe(landscape, wsId) {
    sshProbeCache.delete(`${landscape}/${wsId}`)
  }

  /**
   * List the dev spaces of a landscape, filling in `sshEnabled` from a probe.
   * @param {string} landscape - normalized landscape origin.
   * @param {string} jwt - landscape JSON Web Token.
   * @returns {Promise<object[]>} flattened dev spaces.
   */
  async function listDevSpaces(landscape, jwt) {
    const cached = await call('devspaces', { landscape, jwt })
    const spaces = cached?.devSpaces ?? []
    // Probing costs one HTTP request per RUNNING dev space, so the listing is
    // only asked to probe when a cached answer has gone stale.
    const stale = spaces.some((space) => {
      if (space.status !== DEV_SPACE_RUNNING) return false
      const entry = sshProbeCache.get(`${landscape}/${space.id}`)
      return !entry || Date.now() - entry.at >= SSH_PROBE_TTL_MS
    })
    const probed = stale ? (await call('devspaces', { landscape, jwt, probeSsh: true }))?.devSpaces ?? spaces : spaces
    for (const space of probed) {
      if (space.status !== DEV_SPACE_RUNNING) {
        space.sshEnabled = null
        continue
      }
      if (stale) sshProbeCache.set(`${landscape}/${space.id}`, { value: Boolean(space.sshEnabled), at: Date.now() })
      else space.sshEnabled = sshProbeCache.get(`${landscape}/${space.id}`)?.value ?? null
    }
    return probed
  }

  async function devSpacesOf(landscape) {
    const credential = await requireCredential(landscape)
    return listDevSpaces(landscape, credential.jwt)
  }

  function pickDevSpace(spaces, wanted) {
    const raw = String(wanted ?? '').trim()
    if (!raw) throw new Error('a dev space name or id is required')
    const exact = spaces.filter((space) => space.id === raw)
    if (exact.length === 1) return exact[0]
    const byLabel = spaces.filter(
      (space) => space.label.toLowerCase() === raw.toLowerCase() || space.id.startsWith(raw),
    )
    if (byLabel.length === 1) return byLabel[0]
    if (byLabel.length === 0) {
      throw new Error(
        `no dev space matches "${raw}"; available: ${spaces.map((space) => `${space.label || '(unnamed)'} [${space.id}]`).join(', ') || '(none)'}`,
      )
    }
    throw new Error(`"${raw}" matches several dev spaces: ${byLabel.map((space) => space.id).join(', ')}`)
  }

  function devSpaceWaitMs(waitMs) {
    return Number.isFinite(waitMs) && waitMs > 0 ? waitMs : config.devSpaceTimeoutMs
  }

  /**
   * Start a dev space and (by default) wait until it reports RUNNING.
   *
   * The two-active-dev-space limit, the STOPPING-then-start ordering and the
   * status polling all live in the protocol layer: `devspace_start` refuses a
   * third space, waits out a space that is still stopping, and reports
   * `alreadyRunning` when there was nothing to do.
   * @returns {Promise<{landscape: string, space: object, started: boolean, alreadyRunning: boolean}>}
   */
  async function startDevSpaceFlow({ landscape: landscapeInput, devSpace, wait = true, waitMs }) {
    const landscape = await resolveLandscape(landscapeInput)
    const credential = await requireCredential(landscape)
    const spaces = await listDevSpaces(landscape, credential.jwt)
    const space = pickDevSpace(spaces, devSpace)
    if (space.status === DEV_SPACE_RUNNING) {
      return { landscape, space, started: false, alreadyRunning: true }
    }
    if (!wait && space.status !== DEV_SPACE_STOPPED) {
      return { landscape, space, started: false, alreadyRunning: false }
    }
    const started = await call('devspace_start', {
      landscape,
      jwt: credential.jwt,
      id: space.id,
      label: space.label || space.id,
      // `waitMs: 0` asks for the request to be sent without waiting for RUNNING;
      // the Web UI polls the listing itself, so it must not block.
      waitMs: wait ? devSpaceWaitMs(waitMs) : 0,
    })
    forgetSshProbe(landscape, space.id)
    log('info', `starting dev space ${space.label || space.id} [${space.id}]`)
    return {
      landscape,
      space: started?.devSpace ?? space,
      started: Boolean(started?.started),
      alreadyRunning: Boolean(started?.alreadyRunning),
    }
  }

  /**
   * Stop a dev space and (by default) wait until it reports STOPPED. Any tunnel
   * into it is closed first, because the dev channel dies with the runtime.
   */
  async function stopDevSpaceFlow({ landscape: landscapeInput, devSpace, wait = true, waitMs }) {
    const landscape = await resolveLandscape(landscapeInput)
    const credential = await requireCredential(landscape)
    const spaces = await listDevSpaces(landscape, credential.jwt)
    const space = pickDevSpace(spaces, devSpace)
    if (space.status === DEV_SPACE_STOPPED) {
      return { landscape, space, stopped: false, alreadyStopped: true }
    }
    const key = tunnelKey(landscape, space.id)
    if (tunnels.has(key)) await disconnectDevSpace(key).catch(() => {})
    const stopped = await call('devspace_stop', {
      landscape,
      jwt: credential.jwt,
      id: space.id,
      waitMs: wait ? devSpaceWaitMs(waitMs) : 0,
    })
    forgetSshProbe(landscape, space.id)
    log('info', `stopping dev space ${space.label || space.id} [${space.id}]`)
    return {
      landscape,
      space: stopped?.devSpace ?? space,
      stopped: Boolean(stopped?.stopped),
      alreadyStopped: Boolean(stopped?.alreadyStopped),
    }
  }

  // ── tunnels ───────────────────────────────────────────────────────────────

  function tunnelKey(landscape, wsId) {
    return `${landscape}/${wsId}`
  }

  function findTunnel(input) {
    const raw = String(input ?? '').trim()
    const entries = [...tunnels.entries()]
    if (!raw) {
      if (entries.length === 0) throw new Error('no dev space is connected')
      if (entries.length === 1) return entries[0]
      throw new Error(`several dev spaces are connected (${entries.map(([key]) => key).join(', ')}); name one`)
    }
    const exact = entries.find(([key]) => key === raw)
    if (exact) return exact
    // Tunnel keys are `<landscape>/<dev-space id>` and a landscape URL contains
    // slashes, so match the stored id/label instead of slicing the key.
    const matches = entries.filter(([, entry]) => entry.wsId === raw || entry.label === raw || entry.wsId.startsWith(raw))
    if (matches.length === 1) return matches[0]
    if (matches.length === 0) throw new Error(`"${raw}" is not a connected dev space`)
    throw new Error(`"${raw}" matches several dev spaces: ${matches.map(([key]) => key).join(', ')}`)
  }

  /**
   * Refresh the cached status of one tunnel from the protocol layer.
   *
   * The status is cached because the system-prompt section is synchronous; a
   * tunnel that dropped on its own also has to leave the config file behind.
   * @param {object} entry - tunnel entry.
   * @returns {Promise<object|null>} the fresh status, or null when it is gone.
   */
  async function refreshTunnel(entry) {
    try {
      const status = await call('tunnel_status', { tunnelId: entry.tunnelId })
      const fresh = status?.status ?? null
      if (fresh) entry.status = fresh
      return fresh
    } catch (error) {
      log('debug', `cannot read the tunnel status of ${entry.label || entry.wsId}: ${error.message}`)
      return null
    }
  }

  // ── SSH endpoint publishing ───────────────────────────────────────────────

  /**
   * Where dev-space endpoints go: `off` writes nothing, `fragment` maintains a
   * file this plugin owns, `config` edits `~/.ssh/config` (legacy). A
   * nix/home-manager managed config is usually not writable and would be
   * reverted on the next switch, so the default writes nothing at all.
   * @returns {'off'|'fragment'|'config'} the effective mode.
   */
  function sshConfigMode() {
    if (config.manageSshConfig === true) return 'config'
    const mode = String(config.sshConfigMode ?? 'off').toLowerCase()
    return mode === 'fragment' || mode === 'config' ? mode : 'off'
  }

  /** @returns {string} the fragment file this plugin owns in `fragment` mode. */
  function sshFragmentPath() {
    // Defaults next to the key directory, which follows sshDir/sshConfigPath.
    return config.sshConfigFragmentPath || join(paths.dir, 'bas-remote.conf')
  }

  /** Header written when the fragment file is created. */
  const SSH_FRAGMENT_HEADER = [
    '# bas-remote — dev-space SSH endpoints maintained by the harness plugin.',
    '# Reference it from your own SSH config, e.g. in home-manager:',
    '#   Include ~/.ssh/bas-remote.conf',
    '',
    '',
  ].join('\n')

  /**
   * Publish one dev-space endpoint according to the configured mode. Never
   * writes when the mode is `off`, and never writes a file that is a symlink or
   * not writable — that decision is the protocol layer's `ssh_config_publish`.
   * @param {object} options - entry options.
   * @param {string} options.section - `Host` alias.
   * @param {string} options.identityFile - private-key path.
   * @param {number} options.port - loopback port serving the dev space sshd.
   * @returns {Promise<{section: string, mode: string, written: boolean, target: string, reason: string, block: string}>}
   */
  async function publishSshEntry({ section, identityFile, port }) {
    const mode = sshConfigMode()
    if (mode === 'off') {
      const rendered = await call('ssh_config_block', { section, identityFile, port, user: config.sshUser })
      return { section, mode, written: false, target: '', reason: 'sshConfigMode is off', block: rendered?.block ?? '' }
    }
    const target = mode === 'fragment' ? sshFragmentPath() : paths.configPath
    const published = await call('ssh_config_publish', {
      target,
      section,
      identityFile,
      port,
      user: config.sshUser,
      header: mode === 'fragment' ? SSH_FRAGMENT_HEADER : '',
    })
    if (!published?.written) log('warn', `left ${target} untouched: ${published?.reason ?? 'unknown reason'}`)
    // The ABI's publish result carries no `section`: the host already knows the
    // alias it asked for, and the UI prints it back.
    return { ...published, section, mode }
  }

  /**
   * Remove the endpoint of one dev space from whichever file holds it.
   * @param {object} entry - tunnel entry with its publish record.
   * @returns {Promise<boolean>} whether a file changed.
   */
  async function unpublishSshEntry(entry) {
    const mode = entry?.ssh?.mode ?? 'off'
    if (mode === 'off') return false
    const target = mode === 'fragment' ? sshFragmentPath() : paths.configPath
    try {
      const result = await call('ssh_config_unpublish', { target, section: entry.section })
      return Boolean(result?.changed)
    } catch (error) {
      log('warn', `cannot clean ${target}: ${error.message}`)
      return false
    }
  }

  /**
   * Human-facing lines describing where a dev-space endpoint ended up.
   * @param {object} publish - result of {@link publishSshEntry}.
   * @returns {string[]} lines for a tool result.
   */
  function sshPublishLines(publish) {
    if (publish.written && publish.mode === 'fragment') {
      return [
        `SSH alias: ${publish.section}`,
        `Written to ${publish.target}; add \`Include ${publish.target}\` to your SSH config to activate it.`,
      ]
    }
    if (publish.written) {
      return [`SSH alias: ${publish.section} (marked block in ${publish.target})`]
    }
    return [
      `SSH config left untouched (${publish.reason}).`,
      `Add this to your SSH config to get the alias ${publish.section}:`,
      ...String(publish.block ?? '').split('\n').map((line) => `  ${line}`),
    ]
  }

  // ── SFTP bridge ───────────────────────────────────────────────────────────

  /**
   * Bring up the SFTP bridge of a freshly connected dev space.
   *
   * `bridge_ensure` uploads and starts the server inside the dev space and
   * proves it answers on the dev space's own loopback; the forward is added
   * here, then probed once more through the forward with `ssh_stat` so the
   * endpoint this plugin advertises is known to carry traffic.
   * @param {object} options - bridge options.
   * @param {string} options.tunnelId - dev-channel tunnel handle.
   * @param {number} options.tunnelPort - loopback port of the dev space sshd.
   * @param {string} options.key - key contents.
   * @param {object} options.space - flattened dev space.
   * @returns {Promise<object|null>} the bridge record, or null.
   */
  async function startSftpBridge({ tunnelId, tunnelPort, key, space }) {
    const name = space.label || space.id
    let forwarded = 0
    try {
      // The local port is resolved first: a strict-mode refusal must not leave
      // a bridge daemon running inside the dev space with nothing forwarding it.
      const bridgeLocalPort = await resolveForwardPort(space.id, 'bridge', config, state)
      const bridge = await call('bridge_ensure', {
        port: tunnelPort,
        key,
        username: config.sshUser,
        remotePort: config.sftpBridgePort,
        dir: config.sftpBridgeDir || DEFAULT_BRIDGE_DIR,
        install: config.sftpBridgeInstall,
        timeoutMs: config.sftpBridgeTimeoutMs,
      })
      forwarded = (await call('forward_add', {
        tunnelId,
        remotePort: bridge.remotePort,
        localPort: bridgeLocalPort,
      }))?.localPort ?? 0
      // Prove the whole path — forward, handshake — before advertising it.
      await call('ssh_stat', { port: forwarded, key, username: config.sshUser, path: '/' })
      log('info', `SFTP bridge for ${name} ready on 127.0.0.1:${forwarded}`)
      return { ...bridge, localPort: forwarded }
    } catch (error) {
      log('warn', `SFTP bridge for ${name} unavailable: ${error.message}`)
      if (forwarded) await call('forward_remove', { tunnelId, remotePort: config.sftpBridgePort }).catch(() => {})
      return null
    }
  }

  // ── port assignment ───────────────────────────────────────────────────────

  /**
   * Resolve the port for a dev space, checking availability.
   * @param {string} wsId - dev space id.
   * @param {'dropbear'|'bridge'} kind - port kind.
   * @param {object} config - plugin config.
   * @param {object} state - plugin state.
   * @returns {Promise<number>} resolved port (0 for random).
   * @throws if strict mode and port is occupied.
   */
  async function resolveForwardPort(wsId, kind, config, state) {
    const port = state.forwardPorts?.[wsId]?.[kind] || 0
    if (port === 0) return 0
    const free = Boolean((await call('port_free', { port }))?.free)
    if (free) return port
    const strict = effectiveStrict(config, state)
    if (strict) {
      throw new Error(`port ${port} for ${kind} of ${wsId} is already in use (forwardPortsStrict is on)`)
    }
    log('warn', `port ${port} for ${kind} of ${wsId} is occupied, falling back to random`)
    return 0
  }

  // ── connect / disconnect ──────────────────────────────────────────────────

  async function connectDevSpace({ landscape: landscapeInput, devSpace, localPort }) {
    const landscape = await resolveLandscape(landscapeInput)
    const credential = await requireCredential(landscape)
    const spaces = await listDevSpaces(landscape, credential.jwt)
    const picked = pickDevSpace(spaces, devSpace)
    // Mirrors the SAP extension, which starts a stopped dev space when you ask
    // to open it rather than failing on "no startup URL".
    let space = picked
    try {
      const started = await startDevSpaceFlow({
        landscape,
        devSpace: picked.id,
        waitMs: config.devSpaceTimeoutMs,
      })
      if (started.space?.status === DEV_SPACE_RUNNING) space = started.space
    } catch (error) {
      // A space that was still STARTING when the start was issued can come up
      // anyway; only that case is tolerated, and only when the listing proves
      // it, so a real failure is never hidden.
      const now = (await listDevSpaces(landscape, credential.jwt)).find((entry) => entry.id === picked.id)
      if (now?.status !== DEV_SPACE_RUNNING) throw error
      space = now
    }

    const existing = tunnels.get(tunnelKey(landscape, space.id))
    if (existing) {
      const status = await refreshTunnel(existing)
      if (status?.active) return existing
      tunnels.delete(tunnelKey(landscape, space.id))
      await call('tunnel_close', { tunnelId: existing.tunnelId }).catch(() => {})
      await unpublishSshEntry(existing)
    }

    let material
    try {
      material = await call('devspace_key', { landscape, jwt: credential.jwt, id: space.id })
    } catch (error) {
      // The dev space is running but served no usable key: this is the only
      // case in which the "Remote Access" extension can really be the cause.
      throw new Error(
        `cannot fetch the SSH key of ${space.label || space.id}: ${error.message}. ` +
          'Dev spaces normally serve a key from their runtime; if it stays missing, re-open the dev space in BAS.',
      )
    }
    const key = String(material?.key ?? '').trimEnd() + '\n'
    const wsUrl = String(material?.wsUrl ?? space.url ?? '')
    sshProbeCache.set(`${landscape}/${space.id}`, { value: true, at: Date.now() })
    const keyFile = writeKeyFile(keyFilePath(paths.dir, wsUrl), key)

    // Resolve the dropbear port: an explicit `localPort` wins, then the
    // per-dev-space assignment, then a kernel-chosen free port. `0` means "no
    // explicit port" — testing it with `Number.isFinite` would treat the
    // caller's default of 0 as a real request and silently skip the assignment.
    const dropbearPort = Number(localPort) > 0
      ? Number(localPort)
      : await resolveForwardPort(space.id, 'dropbear', config, state)

    let opened
    try {
      opened = await call('tunnel_open', {
        landscape,
        jwt: credential.jwt,
        wsUrl,
        localPort: dropbearPort,
        username: config.sshUser,
        connectAttempts: config.connectAttempts,
      })
    } catch (error) {
      throw new Error(`cannot open the dev channel to ${space.label || space.id}: ${error.message}`)
    }
    const tunnelId = opened.tunnelId
    const status = opened.status ?? { localPort: dropbearPort, active: true }

    const section = sshHostAlias((await describeLandscape(landscape)).host, space.id)
    const ssh = await publishSshEntry({ section, identityFile: keyFile, port: status.localPort })
    const bridge = config.sftpBridge
      ? await startSftpBridge({ tunnelId, tunnelPort: status.localPort, key, space })
      : null

    const entry = {
      key: tunnelKey(landscape, space.id),
      landscape,
      wsId: space.id,
      label: space.label,
      wsUrl,
      keyFile,
      section,
      ssh,
      bridge,
      sshConfigWritten: ssh.written,
      tunnelId,
      status,
    }
    tunnels.set(entry.key, entry)
    state.lastDevSpace = space.id
    persist()

    // A dropped dev channel must not leave a stale config entry behind. The
    // protocol layer reports the close on the next status read, so one poll is
    // armed here and re-armed while the tunnel is still up.
    void watchTunnel(entry)

    return entry
  }

  /**
   * Watch one tunnel until it drops, then clean up its published endpoint.
   * @param {object} entry - tunnel entry.
   * @returns {Promise<void>} resolves once the entry was cleaned up.
   */
  async function watchTunnel(entry) {
    for (;;) {
      await delay(5000, true)
      const live = tunnels.get(entry.key)
      if (live !== entry) return
      const status = await refreshTunnel(entry)
      if (status && status.active) continue
      tunnels.delete(entry.key)
      await unpublishSshEntry(entry)
      log('info', `dev channel for ${entry.label || entry.wsId} closed${status?.closedReason ? ` (${status.closedReason})` : ''}`)
      return
    }
  }

  async function disconnectDevSpace(input) {
    const [key, entry] = findTunnel(input)
    tunnels.delete(key)
    // The bridge lives inside the dev space and survives a dropped tunnel, so
    // stop it while the dropbear endpoint is still reachable.
    if (entry.bridge) {
      try {
        const stopped = await call('bridge_stop', {
          port: entry.status?.localPort ?? 0,
          key: readFileSync(entry.keyFile, 'utf8'),
          username: config.sshUser,
          dir: entry.bridge.dir,
          remotePort: entry.bridge.remotePort,
        })
        if (stopped?.stopped) log('debug', `stopped the SFTP bridge of ${entry.label || entry.wsId}`)
      } catch (error) {
        log('warn', `could not stop the SFTP bridge of ${entry.label || entry.wsId}: ${error.message}`)
      }
    }
    await call('tunnel_close', { tunnelId: entry.tunnelId }).catch(() => {})
    await unpublishSshEntry(entry)
    if (config.removeKeyOnDisconnect) removeKeyFile(entry.keyFile)
    return entry
  }

  // ── status ────────────────────────────────────────────────────────────────

  async function statusReport() {
    const lines = []
    lines.push(`BAS landscapes: ${state.landscapes.length}`)
    for (const entry of state.landscapes) {
      const credential = await readCredential(entry.url)
      let auth = 'not signed in'
      if (credential) {
        try {
          const info = await call('jwt_describe', { jwt: credential.jwt, skewMs: 60000 })
          const remaining = info?.remainingMs ?? null
          auth = info?.valid
            ? `signed in${remaining === null ? '' : `, token valid for ${formatDuration(remaining)}`}`
            : 'token expired'
        } catch (error) {
          auth = `cannot read the stored token (${error.message})`
        }
      }
      lines.push(`  - ${entry.url} (${auth})`)
    }
    if (state.landscapes.length === 0) {
      lines.push('  (none — add one with /bas login <landscape-url>)')
    }

    if (pendingLogin) {
      const state_ = pendingLogin.settled
        ? pendingLogin.error
          ? `failed: ${pendingLogin.error}`
          : 'completed'
        : `waiting for the browser hand-off on ${config.loginHost}:${pendingLogin.boundPort ?? ''}`
      lines.push(`Sign-in in progress: ${pendingLogin.landscape} — ${state_}`)
      if (!pendingLogin.settled) lines.push(`  Open: ${pendingLogin.url}`)
    }

    lines.push(`Connected dev spaces: ${tunnels.size}`)
    for (const entry of tunnels.values()) {
      const description = (await refreshTunnel(entry)) ?? entry.status ?? {}
      const alive = description.active ? 'active' : `down${description.closedReason ? ` (${description.closedReason})` : ''}`
      lines.push(
        `  - ${entry.label || entry.wsId} [${entry.wsId}] on ${entry.landscape} — ${alive}`,
        `    ssh: ssh ${config.sshUser}@127.0.0.1 -p ${description.localPort} -i ${entry.keyFile}`,
        entry.bridge
          ? `    sftp: 127.0.0.1:${entry.bridge.localPort} (bridge pid ${entry.bridge.pid}, exec + SFTP)`
          : '    sftp: not bridged (dropbear ships no sftp-server)',
        `    alias: ${entry.section}${
          entry.ssh?.written ? ` (in ${entry.ssh.target})` : entry.ssh ? ` (not published: ${entry.ssh.reason})` : ''
        }`,
        `    harness: rw_connect host=127.0.0.1 port=${entry.bridge ? entry.bridge.localPort : description.localPort} username=${config.sshUser} privateKeyPath=${entry.keyFile}`,
      )
    }
    if (tunnels.size === 0) {
      lines.push('  (none — /bas connect <dev space> after /bas login)')
    }
    lines.push(`SSH config: ${paths.configPath}${existsSync(paths.configPath) ? '' : ' (missing)'}`)
    const native = nativeStatus()
    lines.push(
      native.available
        ? `Protocol layer: libbas_api ${native.version} (${native.path})`
        : `Protocol layer: UNAVAILABLE — ${native.error}`,
    )
    return lines.join('\n')
  }

  /**
   * What actually works over a BAS dev-channel tunnel.
   *
   * The dev space's sshd is dropbear and the image ships no `sftp-server`
   * (`/usr/lib/sftp-server` does not exist), so the SFTP subsystem fails with
   * exit code 127 while exec channels work. Every SFTP-based remote tool
   * therefore reports "not a directory (or unreachable)" or a failed session.
   * @param {object} entry - tunnel entry.
   * @returns {string[]} guidance lines.
   */
  function remoteWorkspaceLines(entry) {
    const port = entry.status?.localPort ?? 0
    if (entry.bridge) {
      return [
        'SFTP cannot be served by this dev space\'s own sshd (dropbear ships no sftp-server), so this',
        'plugin runs a small SSH server inside the dev space and forwards it over the same dev channel.',
        `Full endpoint (exec + SFTP): 127.0.0.1:${entry.bridge.localPort}`,
        'To work inside it, adopt it as this session\'s remote workspace:',
        `rw_connect host=127.0.0.1 port=${entry.bridge.localPort} username=${config.sshUser} privateKeyPath=${entry.keyFile}`,
        'Commands then run with rw_exec; rw_stat/rw_read_file/rw_write_file/rw_sync/rw_push and',
        'dsh-remote\'s mirror/pick work over SFTP on that same endpoint.',
        `Plain SSH/exec endpoint of the dev space itself (no SFTP): 127.0.0.1:${port}`,
      ]
    }
    return [
      'To work inside it, adopt it as this session\'s remote workspace:',
      `rw_connect host=127.0.0.1 port=${port} username=${config.sshUser} privateKeyPath=${entry.keyFile}`,
      'Commands then run with rw_exec. BAS dev spaces run dropbear without an sftp-server, so the',
      'SFTP-based tools (rw_stat, rw_read_file, rw_write_file, rw_sync, rw_push, and dsh-remote\'s',
      'mirror/pick → "not a directory (or unreachable)") cannot work here. Either enable',
      '`sftpBridge` (default on) so this plugin provides SFTP, or move files over exec instead:',
      `  scp -O -P ${port} -i ${entry.keyFile} -r ./dir ${config.sshUser}@127.0.0.1:/home/user/projects/`,
      `  tar c -C ./dir . | ssh -p ${port} -i ${entry.keyFile} ${config.sshUser}@127.0.0.1 'tar x -C /home/user/projects/dir'`,
    ]
  }

  /** @param {object} space - flattened dev space. @returns {string} its SSH key state. */
  function sshStateLabel(space) {
    if (space.sshEnabled === true) return 'key available'
    if (space.sshEnabled === false) return 'no key from the runtime'
    return 'not probed (start it first)'
  }

  /** @returns {string} a one-line status label for a dev space. */
  function devSpaceName(space) {
    return `${space.label || space.id} [${space.id}]`
  }

  // ── model tools ───────────────────────────────────────────────────────────

  const output = textTool()
  const tools = [
    defineTool({
      name: 'bas_status',
      description:
        'Show the SAP Business Application Studio (BAS) state of this harness: configured landscapes and whether their tokens are still valid, a pending browser sign-in, every connected dev space with its loopback SSH endpoint, and where SSH endpoints are published. Call this first to orient, or after bas_login to see whether the hand-off completed.',
      parameters: {},
      output,
      async execute() {
        return { text: await statusReport() }
      },
    }),

    defineTool({
      name: 'bas_login',
      description:
        'Sign in to a BAS landscape. Registers the landscape URL if new and starts the browser hand-off: the landscape login page posts the token back to this host on 127.0.0.1:55532. Returns the URL to open. Call it again with the same landscape and a waitMs to block until the hand-off lands (or use bas_status to poll). The token is kept in the harness credential store, not in configuration.',
      parameters: {
        landscape: {
          type: 'string',
          required: true,
          description: 'Landscape URL or host, e.g. https://my-tenant.eu10.applicationstudio.cloud.sap',
        },
        waitMs: {
          type: 'integer',
          description: 'Block up to this many milliseconds for the browser hand-off (0 returns immediately with the URL)',
        },
      },
      output,
      async execute(args) {
        const entry = await startLogin(args.landscape)
        const waitMs = Number(args.waitMs ?? 0)
        if (waitMs > 0) await awaitLogin(entry, waitMs)
        const lines = [
          `Landscape: ${entry.landscape}`,
          `Open this URL in a browser and sign in: ${entry.url}`,
          'The landscape posts the token back to this host, so the browser must reach ' +
            `${config.loginHost}:${entry.boundPort ?? config.loginPort}` +
            (config.loginHost === '127.0.0.1' ? ' (forward that port when the harness runs on another machine).' : '.'),
        ]
        if (entry.settled) {
          lines.push(entry.error ? `Sign-in failed: ${entry.error}` : 'Sign-in completed; the token is stored.')
        } else {
          lines.push(
            `Waiting for the hand-off (call bas_login with waitMs, or bas_status, to check).`,
          )
        }
        return { text: lines.join('\n') }
      },
    }),

    defineTool({
      name: 'bas_logout',
      description: 'Forget the stored BAS landscape token for one landscape (the landscape stays configured).',
      parameters: { landscape: { type: 'string', required: true, description: 'Landscape URL or host' } },
      output,
      async execute(args) {
        const landscape = await resolveLandscape(args.landscape)
        await dropCredential(landscape)
        return { text: `Removed the stored token for ${(await describeLandscape(landscape)).host}.` }
      },
    }),

    defineTool({
      name: 'bas_forget',
      description: 'Disconnect every dev space of a landscape, forget its token and remove the landscape.',
      parameters: { landscape: { type: 'string', required: true, description: 'Landscape URL or host' } },
      output,
      async execute(args) {
        const landscape = await resolveLandscape(args.landscape)
        for (const [key, entry] of [...tunnels.entries()]) {
          if (entry.landscape !== landscape) continue
          await disconnectDevSpace(key).catch(() => {})
        }
        await dropCredential(landscape)
        await forgetLandscape(landscape)
        return { text: `Removed ${landscape} and its stored token.` }
      },
    }),

    defineTool({
      name: 'bas_devspaces',
      description:
        'List the dev spaces of a BAS landscape: display name, id, running status (RUNNING | STARTING | STOPPED | STOPPING | ERROR), extension pack, and whether the dev space exposes SSH (the "Remote Access" extension). A STOPPED dev space is started with bas_start (or implicitly by bas_connect). Requires a completed bas_login.',
      parameters: {
        landscape: { type: 'string', description: 'Landscape URL or host (optional when exactly one is configured)' },
      },
      output,
      async execute(args) {
        const landscape = await resolveLandscape(args.landscape)
        const spaces = await devSpacesOf(landscape)
        if (spaces.length === 0) return { text: `${landscape} has no dev spaces.` }
        const lines = [`Dev spaces on ${landscape}:`]
        for (const space of spaces) {
          lines.push(
            `- ${space.label || '(unnamed)'} [${space.id}]`,
            `  status: ${space.status}, ssh: ${sshStateLabel(space)}`,
            `  pack: ${space.packDisplayName || space.pack || '(unknown)'}${space.url ? `, url: ${space.url}` : ''}`,
          )
        }
        return { text: lines.join('\n') }
      },
    }),

    defineTool({
      name: 'bas_start',
      description:
        'Start (resume) a BAS dev space and wait until it reports RUNNING, mirroring the SAP extension\'s start action (`PUT <landscape>/ws-manager/api/v1/workspace/<id>` with `Suspended: false`). A landscape runs at most two dev spaces at a time; when both slots are taken the call fails and names the dev spaces holding them. Requires a completed bas_login.',
      parameters: {
        devSpace: { type: 'string', required: true, description: 'Dev space display name or id (see bas_devspaces)' },
        landscape: { type: 'string', description: 'Landscape URL or host (optional when exactly one is configured)' },
        waitMs: { type: 'integer', description: 'Give up waiting for RUNNING after this long (defaults to the devSpaceTimeoutMs config)' },
      },
      output,
      async execute(args) {
        const result = await startDevSpaceFlow({
          landscape: args.landscape,
          devSpace: args.devSpace,
          waitMs: Number(args.waitMs ?? 0) || 0,
        })
        const spaceName = devSpaceName(result.space)
        const where = (await describeLandscape(result.landscape)).host
        if (result.alreadyRunning) return { text: `Dev space "${spaceName}" is already RUNNING on ${where}.` }
        return {
          text: [
            `Dev space "${spaceName}" is ${result.space.status} on ${where}.`,
            `Connect it with bas_connect devSpace=${result.space.id}.`,
          ].join('\n'),
        }
      },
    }),

    defineTool({
      name: 'bas_stop',
      description:
        'Stop (suspend) a BAS dev space and wait until it reports STOPPED, mirroring the SAP extension\'s stop action (`PUT <landscape>/ws-manager/api/v1/workspace/<id>` with `Suspended: true`). Any dev-channel tunnel into it is closed first, because the runtime disappears with the dev space. Requires a completed bas_login.',
      parameters: {
        devSpace: { type: 'string', required: true, description: 'Dev space display name or id (see bas_devspaces)' },
        landscape: { type: 'string', description: 'Landscape URL or host (optional when exactly one is configured)' },
        waitMs: { type: 'integer', description: 'Give up waiting for STOPPED after this long (defaults to the devSpaceTimeoutMs config)' },
      },
      output,
      async execute(args) {
        const result = await stopDevSpaceFlow({
          landscape: args.landscape,
          devSpace: args.devSpace,
          waitMs: Number(args.waitMs ?? 0) || 0,
        })
        const spaceName = devSpaceName(result.space)
        const where = (await describeLandscape(result.landscape)).host
        if (result.alreadyStopped) return { text: `Dev space "${spaceName}" is already STOPPED on ${where}.` }
        return { text: `Dev space "${spaceName}" is ${result.space.status} on ${where}.` }
      },
    }),

    defineTool({
      name: 'bas_connect',
      description:
        'Connect a BAS dev space and expose it as a local SSH endpoint: fetches the dev-space private key, opens the dev-channel tunnel and reports the SSH `Host` block (only written to disk when sshConfigMode is set). A stopped dev space is started first (as the SAP extension does), then waited for. The result names the loopback host/port, the key file, the `rw_connect` call that adopts the dev space as a remote workspace, and which remote tools the dev space can actually serve: BAS sshd\'s serve exec but no SFTP, so when `sftpBridge` is on (default) a small SSH server carrying exec + SFTP is installed inside the dev space and the reported `rw_connect` endpoint serves both; if that fails the result says so and only exec works.',
      parameters: {
        devSpace: { type: 'string', required: true, description: 'Dev space display name or id (see bas_devspaces)' },
        landscape: { type: 'string', description: 'Landscape URL or host (optional when exactly one is configured)' },
        localPort: { type: 'integer', description: 'Loopback port to listen on (0 or omitted picks a free port)' },
      },
      output,
      async execute(args) {
        const entry = await connectDevSpace({
          landscape: args.landscape,
          devSpace: args.devSpace,
          localPort: Number(args.localPort ?? 0) || 0,
        })
        const description = entry.status ?? {}
        return {
          text: [
            `Connected dev space "${entry.label || entry.wsId}" [${entry.wsId}] on ${entry.landscape}.`,
            `SSH endpoint: 127.0.0.1:${description.localPort} (user ${config.sshUser})${
              entry.bridge ? ` — exec only; SFTP: 127.0.0.1:${entry.bridge.localPort}` : ''
            }`,
            `Private key: ${entry.keyFile}`,
            ...sshPublishLines(entry.ssh ?? { written: false, reason: 'nothing published', block: '', section: entry.section }),
            ...remoteWorkspaceLines(entry),
            'Use bas_disconnect when the dev space is no longer needed.',
          ].join('\n'),
        }
      },
    }),

    defineTool({
      name: 'bas_disconnect',
      description:
        'Close the dev-channel tunnel of a connected dev space, remove its published SSH endpoint and stop forwarding. Without an argument the only connected dev space is closed; with several connected, name one.',
      parameters: {
        devSpace: { type: 'string', description: 'Dev space display name or id (optional when exactly one is connected)' },
      },
      output,
      async execute(args) {
        const entry = await disconnectDevSpace(args.devSpace)
        return { text: `Disconnected dev space "${entry.label || entry.wsId}" [${entry.wsId}].` }
      },
    }),
    defineTool({
      name: 'bas_bridge',
      description:
        'Inspect or stop the SFTP bridge of a connected dev space. BAS dev spaces run dropbear without an sftp-server, so this plugin installs a small SSH server (exec + SFTP) inside the dev space and forwards it over the dev channel; this reports whether it is installed, running, listening and what its log says, or stops it (which turns the endpoint back into exec-only).',
      parameters: {
        devSpace: { type: 'string', description: 'Dev space display name or id (optional when exactly one is connected)' },
        action: { type: 'string', enum: ['status', 'stop'], description: 'status reports the bridge, stop removes it (default: status)' },
      },
      output,
      async execute(args) {
        const [, entry] = findTunnel(args.devSpace)
        const dir = entry.bridge?.dir || config.sftpBridgeDir || DEFAULT_BRIDGE_DIR
        const key = readFileSync(entry.keyFile, 'utf8')
        const bridge = await call('bridge_state', {
          port: entry.status?.localPort ?? 0,
          key,
          username: config.sshUser,
          dir,
          remotePort: config.sftpBridgePort,
        })
        if (args.action === 'stop') {
          const stopped = await call('bridge_stop', {
            port: entry.status?.localPort ?? 0,
            key,
            username: config.sshUser,
            dir: bridge?.dir || dir,
            remotePort: config.sftpBridgePort,
          })
          if (entry.tunnelId) await call('forward_remove', { tunnelId: entry.tunnelId, remotePort: config.sftpBridgePort }).catch(() => {})
          entry.bridge = null
          return {
            text: `SFTP bridge of ${entry.label || entry.wsId}: ${stopped?.stopped ? 'stopped' : 'was not running'}. The endpoint now serves exec only; reconnect to bring SFTP back.`,
          }
        }
        return {
          text: [
            `SFTP bridge of ${entry.label || entry.wsId} [${entry.wsId}]`,
            `directory: ${bridge?.dir ?? dir}`,
            `ssh2 installed: ${bridge?.installed ? 'yes' : 'no'}`,
            `daemon: ${bridge?.running ? `running (pid ${bridge.pid})` : 'not running'}`,
            `listening inside the dev space on port ${config.sftpBridgePort}: ${bridge?.listening ? 'yes' : 'no'}`,
            entry.bridge ? `forwarded here as 127.0.0.1:${entry.bridge.localPort}` : 'not forwarded in this session',
            bridge?.logTail ? `log:\n${bridge.logTail}` : '',
          ]
            .filter(Boolean)
            .join('\n'),
        }
      },
    }),
  ]

  for (const tool of tools) ctx.tools.register(tool)

  // ── human command ─────────────────────────────────────────────────────────

  const basCommand = {
    name: 'bas',
    description:
      'SAP Business Application Studio remote dev spaces (status | login | logout | add | forget | devspaces | start | stop | connect | disconnect | bridge | ports | strict)',
    input: {
      hint: 'status | login <landscape> | logout <landscape> | add <landscape> | forget <landscape> | devspaces [landscape] | start <dev space> | stop <dev space> | connect <dev space> | disconnect [dev space] | bridge [dev space] | ports [set <wsId> <drop> <bridge>|clear <wsId>] | strict [on|off]',
    },
    async handler(invocation) {
      const raw = String(invocation.rawInput ?? '').trim()
      const [subcommand = 'status', ...rest] = raw.split(/\s+/).filter(Boolean)
      const argument = rest.join(' ').trim()
      try {
        switch (subcommand) {
          case 'status':
            return { kind: 'success', text: await statusReport() }
          case 'login': {
            const entry = await startLogin(argument)
            const done = await awaitLogin(entry, config.loginTimeoutMs)
            if (!done.settled) {
              return {
                kind: 'success',
                text: [
                  `Sign in to ${done.landscape} at: ${done.url}`,
                  `Still waiting for the browser hand-off on ${config.loginHost}:${done.boundPort ?? config.loginPort}.`,
                  'Run /bas status when you are done.',
                ].join('\n'),
              }
            }
            if (done.error) return { kind: 'error', text: `Sign-in failed: ${done.error}` }
            return { kind: 'success', text: `Signed in to ${done.landscape}; the token is stored.` }
          }
          case 'devspaces': {
            const landscape = await resolveLandscape(argument)
            const spaces = await devSpacesOf(landscape)
            if (spaces.length === 0) return { kind: 'success', text: `${landscape} has no dev spaces.` }
            return {
              kind: 'success',
              text: spaces
                .map(
                  (space) =>
                    `${space.sshEnabled === true ? '[ssh]' : space.sshEnabled === false ? '[ ! ]' : '[ ? ]'} ${space.status.padEnd(8)} ${space.label || '(unnamed)'} [${space.id}]`,
                )
                .join('\n'),
            }
          }
          case 'start': {
            const result = await startDevSpaceFlow({ devSpace: argument })
            const spaceName = devSpaceName(result.space)
            return {
              kind: 'success',
              text: result.alreadyRunning
                ? `${spaceName} is already RUNNING.`
                : `${spaceName} is ${result.space.status}. Connect it with /bas connect ${result.space.id}.`,
            }
          }
          case 'stop': {
            const result = await stopDevSpaceFlow({ devSpace: argument })
            const spaceName = devSpaceName(result.space)
            return {
              kind: 'success',
              text: result.alreadyStopped ? `${spaceName} is already STOPPED.` : `${spaceName} is ${result.space.status}.`,
            }
          }
          case 'connect': {
            const entry = await connectDevSpace({ devSpace: argument, localPort: config.localPort })
            const description = entry.status ?? {}
            return {
              kind: 'success',
              text: [
                `Connected ${entry.label || entry.wsId} [${entry.wsId}].`,
                `ssh ${config.sshUser}@127.0.0.1 -p ${description.localPort} -i ${entry.keyFile}`,
                entry.bridge
                  ? `sftp + exec: rw_connect host=127.0.0.1 port=${entry.bridge.localPort} username=${config.sshUser} privateKeyPath=${entry.keyFile}`
                  : 'sftp: unavailable (the dev space sshd has no sftp-server and the bridge is off or failed)',
                ...sshPublishLines(entry.ssh ?? { written: false, reason: 'nothing published', block: '', section: entry.section }),
              ].join('\n'),
            }
          }
          case 'disconnect': {
            const entry = await disconnectDevSpace(argument)
            return { kind: 'success', text: `Disconnected ${entry.label || entry.wsId} [${entry.wsId}].` }
          }
          case 'logout': {
            const landscape = await resolveLandscape(argument)
            await dropCredential(landscape)
            return { kind: 'success', text: `Removed the stored token for ${(await describeLandscape(landscape)).host}.` }
          }
          case 'bridge': {
            const [, entry] = findTunnel(argument)
            const dir = entry.bridge?.dir || config.sftpBridgeDir || DEFAULT_BRIDGE_DIR
            const bridge = await call('bridge_state', {
              port: entry.status?.localPort ?? 0,
              key: readFileSync(entry.keyFile, 'utf8'),
              username: config.sshUser,
              dir,
              remotePort: config.sftpBridgePort,
            })
            return {
              kind: 'success',
              text: [
                `SFTP bridge of ${entry.label || entry.wsId} [${entry.wsId}]`,
                `directory: ${bridge?.dir ?? dir}`,
                `ssh2 installed: ${bridge?.installed ? 'yes' : 'no'}`,
                `daemon: ${bridge?.running ? `running (pid ${bridge.pid})` : 'not running'}`,
                `listening inside the dev space on port ${config.sftpBridgePort}: ${bridge?.listening ? 'yes' : 'no'}`,
                entry.bridge ? `forwarded here as 127.0.0.1:${entry.bridge.localPort}` : 'not forwarded in this session',
                bridge?.logTail ? `log:\n${bridge.logTail}` : '',
              ]
                .filter(Boolean)
                .join('\n'),
            }
          }
          case 'ports': {
            // Parse: /bas ports [set|clear] [wsId] [dropbear] [bridge]
            const parts = argument.split(/\s+/).filter(Boolean)
            if (parts[0] === 'set' && parts.length >= 4) {
              const wsId = parts[1]
              const dropbear = parseInt(parts[2]) || 0
              const bridge = parseInt(parts[3]) || 0
              try {
                state.forwardPorts[wsId] = checkForwardPorts({ dropbear, bridge }, wsId)
              } catch (error) {
                return { kind: 'error', text: error.message }
              }
              persist()
              return { kind: 'success', text: `Set ports for ${wsId}: dropbear=${dropbear}, bridge=${bridge}` }
            }
            if (parts[0] === 'clear' && parts.length >= 2) {
              const wsId = parts[1]
              delete state.forwardPorts[wsId]
              persist()
              return { kind: 'success', text: `Cleared port assignment for ${wsId}` }
            }
            // Show all ports
            const entries = Object.entries(state.forwardPorts)
            if (entries.length === 0) {
              return { kind: 'success', text: 'No fixed port assignments. All dev spaces use random ports.\n\nUsage: /bas ports set <wsId> <dropbearPort> <bridgePort>' }
            }
            const lines = entries.map(([wsId, ports]) => {
              const dropbear = ports.dropbear || 'random'
              const bridge = ports.bridge || 'random'
              return `  ${wsId}: dropbear=${dropbear}, bridge=${bridge}`
            })
            return {
              kind: 'success',
              text: [
                'Fixed port assignments:',
                ...lines,
                '',
                `Strict mode: ${effectiveStrict(config, state) ? 'on (error if port occupied)' : 'off (fallback to random)'}`,
                '',
                'Usage: /bas ports set <wsId> <dropbearPort> <bridgePort>',
                '       /bas ports clear <wsId>',
              ].join('\n'),
            }
          }
          case 'forget': {
            const landscape = await resolveLandscape(argument)
            for (const [key, entry] of [...tunnels.entries()]) {
              if (entry.landscape !== landscape) continue
              await disconnectDevSpace(key).catch(() => {})
            }
            await dropCredential(landscape)
            await forgetLandscape(landscape)
            return { kind: 'success', text: `Removed ${landscape}.` }
          }
          case 'add': {
            // The web panel can register a landscape without signing in to it
            // (POST /bas-remote/landscape with action "add"); /bas login would
            // also register it, but only as a side effect of signing in. Make
            // the intent explicit so a TUI user can stage a landscape first.
            if (!argument) {
              return { kind: 'error', text: 'usage: /bas add <landscape-url>' }
            }
            const landscape = await registerLandscape(argument)
            return {
              kind: 'success',
              text: `Added ${landscape}. Sign in with /bas login ${landscape}.`,
            }
          }
          case 'strict': {
            // Parity with the web panel's POST /bas-remote/strict, which is the
            // only UI that could flip this before. Empty = toggle, like the web
            // panel's button; on/off = set it explicitly.
            const wants = String(argument || '').trim().toLowerCase()
            if (wants === '') state.forwardPortsStrict = !effectiveStrict(config, state)
            else if (wants === 'on' || wants === 'true') state.forwardPortsStrict = true
            else if (wants === 'off' || wants === 'false') state.forwardPortsStrict = false
            else {
              return {
                kind: 'error',
                text: `unknown strict value "${argument}"; use on, off, or nothing to toggle`,
              }
            }
            persist()
            return {
              kind: 'success',
              text: effectiveStrict(config, state)
                ? 'Port strictness: on (a fixed port that is occupied is an error)'
                : 'Port strictness: off (fall back to a random port)',
            }
          }
          default:
            return {
              kind: 'error',
              text: `unknown subcommand "${subcommand}"; use status, login, logout, add, forget, devspaces, start, stop, connect, disconnect, ports or strict`,
            }
        }
      } catch (error) {
        return { kind: 'error', text: error.message }
      }
    },
  }

  const commandRegistry = ctx.reflect.get('commands')
  if (commandRegistry) commandRegistry.register(basCommand)

  // ── system prompt: active dev spaces ──────────────────────────────────────

  if (config.promptSection) {
    ctx.systemPrompt.section({
      name: 'bas-remote',
      order: 89,
      text: () => {
        // Synchronous by contract, so it reads the cached tunnel status that
        // connect and the status routes refresh; a native call here would block.
        const entries = [...tunnels.values()].filter((entry) => entry.status?.active)
        if (entries.length === 0) return ''
        const lines = [
          'SAP Business Application Studio dev spaces are connected over local SSH tunnels:',
        ]
        for (const entry of entries) {
          const description = entry.status ?? {}
          lines.push(
            `- ${entry.label || entry.wsId} [${entry.wsId}]: ssh 127.0.0.1 port ${description.localPort}, user ${config.sshUser}, key ${entry.keyFile}` +
              ` (alias ${entry.section}); adopt it with rw_connect host=127.0.0.1 port=${description.localPort} username=${config.sshUser} privateKeyPath=${entry.keyFile}.`,
          )
        }
        lines.push('These tunnels are managed by bas-remote (tools: bas_status, bas_connect, bas_disconnect).')
        return lines.join('\n')
      },
    })
  }

  // ── Web/Desktop JSON routes ───────────────────────────────────────────────

  async function snapshot() {
    const tunnelPayload = []
    for (const entry of tunnels.values()) {
      const status = (await refreshTunnel(entry)) ?? entry.status ?? {}
      tunnelPayload.push({
        key: entry.key,
        landscape: entry.landscape,
        wsId: entry.wsId,
        label: entry.label,
        keyFile: entry.keyFile,
        section: entry.section,
        sshConfigWritten: entry.sshConfigWritten,
        ssh: entry.ssh ?? null,
        sftp: entry.bridge
          ? { localPort: entry.bridge.localPort, remotePort: entry.bridge.remotePort, pid: entry.bridge.pid, dir: entry.bridge.dir }
          : null,
        serverUri: status.serverUri ?? '',
        localPort: status.localPort ?? 0,
        username: status.username ?? config.sshUser,
        sshHost: status.sshHost ?? '127.0.0.1',
        sshPort: status.sshPort ?? 0,
        active: Boolean(status.active),
        startedAt: status.startedAt ?? 0,
        closedReason: status.closedReason ?? '',
      })
    }
    return {
      version: PLUGIN_VERSION,
      landscapes: state.landscapes.map((entry) => entry),
      pendingLogin: pendingLogin
        ? {
            landscape: pendingLogin.landscape,
            url: pendingLogin.url,
            settled: pendingLogin.settled,
            error: pendingLogin.error,
            startedAt: pendingLogin.startedAt,
          }
        : null,
      tunnels: tunnelPayload,
      forwardPorts: state.forwardPorts,
      forwardPortsStrict: effectiveStrict(config, state),
      sshConfigPath: paths.configPath,
      sshConfigMode: sshConfigMode(),
      sshFragmentPath: sshFragmentPath(),
      sshUser: config.sshUser,
      loginHost: config.loginHost,
      loginPort: config.loginPort,
    }
  }

  async function authStates() {
    const result = {}
    for (const entry of state.landscapes) {
      const credential = await readCredential(entry.url)
      if (!credential) {
        result[entry.url] = { signedIn: false, remainingMs: null }
        continue
      }
      try {
        const info = await call('jwt_describe', { jwt: credential.jwt, skewMs: 60000 })
        result[entry.url] = { signedIn: Boolean(info?.valid), remainingMs: info?.remainingMs ?? null }
      } catch {
        result[entry.url] = { signedIn: false, remainingMs: null }
      }
    }
    return result
  }

  const routes = [
    {
      path: '/bas-remote/state',
      handler: async (req, res) => {
        if (req.method !== 'GET') return sendJson(res, 405, { ok: false, error: 'method not allowed' })
        try {
          sendJson(res, 200, { ok: true, state: await snapshot(), auth: await authStates() })
        } catch (error) {
          sendJson(res, 500, { ok: false, error: error.message })
        }
      },
    },
    {
      path: '/bas-remote/landscape',
      handler: async (req, res) => {
        if (req.method !== 'POST') return sendJson(res, 405, { ok: false, error: 'method not allowed' })
        try {
          const body = await readBody(req)
          const action = String(body.action ?? 'add')
          if (action === 'add') {
            const landscape = await registerLandscape(body.landscape)
            return sendJson(res, 200, { ok: true, landscape })
          }
          if (action === 'forget') {
            const landscape = await resolveLandscape(body.landscape)
            for (const [key, entry] of [...tunnels.entries()]) {
              if (entry.landscape !== landscape) continue
              await disconnectDevSpace(key).catch(() => {})
            }
            await dropCredential(landscape)
            await forgetLandscape(landscape)
            return sendJson(res, 200, { ok: true })
          }
          return sendJson(res, 400, { ok: false, error: `unknown action "${action}"` })
        } catch (error) {
          sendJson(res, 400, { ok: false, error: error.message })
        }
      },
    },
    {
      path: '/bas-remote/login',
      handler: async (req, res) => {
        if (req.method !== 'POST') return sendJson(res, 405, { ok: false, error: 'method not allowed' })
        try {
          const body = await readBody(req)
          const entry = await startLogin(body.landscape)
          const waitMs = Number(body.waitMs ?? 0)
          if (waitMs > 0) await awaitLogin(entry, waitMs)
          sendJson(res, 200, {
            ok: true,
            url: entry.url,
            landscape: entry.landscape,
            settled: entry.settled,
            error: entry.error,
            loginHost: config.loginHost,
            loginPort: entry.boundPort ?? config.loginPort,
          })
        } catch (error) {
          sendJson(res, 400, { ok: false, error: error.message })
        }
      },
    },
    {
      path: '/bas-remote/logout',
      handler: async (req, res) => {
        if (req.method !== 'POST') return sendJson(res, 405, { ok: false, error: 'method not allowed' })
        try {
          const body = await readBody(req)
          const landscape = await resolveLandscape(body.landscape)
          await dropCredential(landscape)
          sendJson(res, 200, { ok: true })
        } catch (error) {
          sendJson(res, 400, { ok: false, error: error.message })
        }
      },
    },
    {
      path: '/bas-remote/devspaces',
      handler: async (req, res) => {
        if (req.method !== 'POST') return sendJson(res, 405, { ok: false, error: 'method not allowed' })
        try {
          const body = await readBody(req)
          const landscape = await resolveLandscape(body.landscape)
          const spaces = await devSpacesOf(landscape)
          sendJson(res, 200, { ok: true, devSpaces: spaces })
        } catch (error) {
          sendJson(res, 400, { ok: false, error: error.message })
        }
      },
    },
    {
      path: '/bas-remote/devspace',
      handler: async (req, res) => {
        if (req.method !== 'POST') return sendJson(res, 405, { ok: false, error: 'method not allowed' })
        try {
          const body = await readBody(req)
          const action = String(body.action ?? '').toLowerCase()
          // The Web UI keeps polling the dev-space list itself, so it does not
          // wait here; the tools and /bas do wait for a final status.
          if (action === 'start') {
            const result = await startDevSpaceFlow({
              landscape: body.landscape,
              devSpace: body.devSpace,
              wait: false,
            })
            return sendJson(res, 200, { ok: true, devSpace: result.space })
          }
          if (action === 'stop') {
            const result = await stopDevSpaceFlow({
              landscape: body.landscape,
              devSpace: body.devSpace,
              wait: false,
            })
            return sendJson(res, 200, { ok: true, devSpace: result.space })
          }
          sendJson(res, 400, { ok: false, error: `unknown action "${body.action}"; use start or stop` })
        } catch (error) {
          sendJson(res, 400, { ok: false, error: error.message })
        }
      },
    },
    {
      path: '/bas-remote/connect',
      handler: async (req, res) => {
        if (req.method !== 'POST') return sendJson(res, 405, { ok: false, error: 'method not allowed' })
        try {
          const body = await readBody(req)
          const entry = await connectDevSpace({
            landscape: body.landscape,
            devSpace: body.devSpace,
            localPort: Number(body.localPort ?? 0) || 0,
          })
          const description = entry.status ?? {}
          sendJson(res, 200, {
            ok: true,
            tunnel: {
              key: entry.key,
              label: entry.label,
              wsId: entry.wsId,
              section: entry.section,
              keyFile: entry.keyFile,
              sshConfigWritten: entry.sshConfigWritten,
              ssh: entry.ssh ?? null,
              sftp: entry.bridge
                ? { localPort: entry.bridge.localPort, remotePort: entry.bridge.remotePort, pid: entry.bridge.pid, dir: entry.bridge.dir }
                : null,
              serverUri: description.serverUri ?? '',
              localPort: description.localPort ?? 0,
              username: description.username ?? config.sshUser,
              sshHost: description.sshHost ?? '127.0.0.1',
              sshPort: description.sshPort ?? 0,
              active: Boolean(description.active),
              startedAt: description.startedAt ?? 0,
              closedReason: description.closedReason ?? '',
            },
          })
        } catch (error) {
          sendJson(res, 400, { ok: false, error: error.message })
        }
      },
    },
    {
      path: '/bas-remote/ports',
      handler: async (req, res) => {
        if (req.method !== 'POST') return sendJson(res, 405, { ok: false, error: 'method not allowed' })
        try {
          const body = await readBody(req)
          // The handler merges, so a deletion has to be an explicit `null`:
          // an absent key cannot express "remove this one". Validate the whole
          // payload before touching state, so a bad entry cannot leave half of
          // a batch applied in memory but unwritten on disk.
          if (body.forwardPorts && typeof body.forwardPorts === 'object') {
            const removals = []
            const additions = []
            for (const [wsId, value] of Object.entries(body.forwardPorts)) {
              if (value === null || value === undefined) removals.push(wsId)
              else additions.push([wsId, checkForwardPorts(value, wsId)])
            }
            for (const wsId of removals) delete state.forwardPorts[wsId]
            for (const [wsId, ports] of additions) state.forwardPorts[wsId] = ports
            persist()
          }
          sendJson(res, 200, {
            ok: true,
            forwardPorts: state.forwardPorts,
          })
        } catch (error) {
          sendJson(res, 400, { ok: false, error: error.message })
        }
      },
    },
    {
      path: '/bas-remote/strict',
      handler: async (req, res) => {
        if (req.method !== 'POST') return sendJson(res, 405, { ok: false, error: 'method not allowed' })
        try {
          const body = await readBody(req)
          if (typeof body.strict === 'boolean') {
            state.forwardPortsStrict = body.strict
          } else {
            // Toggle
            state.forwardPortsStrict = !effectiveStrict(config, state)
          }
          persist()
          sendJson(res, 200, {
            ok: true,
            forwardPortsStrict: effectiveStrict(config, state),
          })
        } catch (error) {
          sendJson(res, 400, { ok: false, error: error.message })
        }
      },
    },
    {
      path: '/bas-remote/disconnect',
      handler: async (req, res) => {
        if (req.method !== 'POST') return sendJson(res, 405, { ok: false, error: 'method not allowed' })
        try {
          const body = await readBody(req)
          const entry = await disconnectDevSpace(body.devSpace)
          sendJson(res, 200, { ok: true, key: entry.key })
        } catch (error) {
          sendJson(res, 400, { ok: false, error: error.message })
        }
      },
    },
  ]

  registerTransports(ctx, routes)

  ctx.effect(
    () => () => {
      closeLogin()
      for (const entry of tunnels.values()) {
        void call('tunnel_close', { tunnelId: entry.tunnelId }).catch(() => {})
        void unpublishSshEntry(entry).catch(() => {})
      }
      tunnels.clear()
    },
    'bas-remote.teardown',
  )

  if (config.defaultLandscape) {
    void registerLandscape(config.defaultLandscape).catch((error) => {
      log('warn', `ignoring invalid defaultLandscape: ${error.message}`)
    })
  }
}
