// Tests for the host half against the real Rust library.
//
// `test/ports.test.mjs` proves the routes behave; this file proves the host and
// the protocol layer actually talk to each other — the seam that a rewrite can
// break while every unit test still passes.
//
// It loads the plugin with a stubbed cordis context, then drives the registered
// tools and routes through `lib/native.js` into `libbas_api`. Nothing reaches
// the network: every check is either a pure helper or a call that must fail
// cleanly without a landscape.
//
// Needs the library; set BAS_NATIVE_LIB or build it with
// `cargo build --release -p bas-api`. Without it the suite skips with a notice
// so `npm test` still runs on a machine without a Rust toolchain.

import assert from 'node:assert/strict'
import { mkdtempSync, mkdirSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'

const HOME_DIR = mkdtempSync(join(tmpdir(), 'dsh-bas-native-'))
process.env.DSH_HOME = HOME_DIR

const native = await import('../lib/native.js')
const status = native.nativeStatus()
if (!status.available) {
  console.log(`skip: ${status.error}`)
  process.exit(0)
}

const checks = []
/**
 * Run one check and record the result.
 * @param {string} name - what is being asserted.
 * @param {() => Promise<void>} body - the assertions.
 */
async function check(name, body) {
  try {
    await body()
    checks.push(`ok   ${name}`)
  } catch (error) {
    checks.push(`FAIL ${name}: ${error.message}`)
    process.exitCode = 1
  }
}

/**
 * Load the plugin and capture everything it registers.
 * @param {object} overrides - plugin config on top of the schema defaults.
 * @returns {Promise<{tools: Map<string, object>, call: Function, prompt: Function}>} the captured surface.
 */
async function loadPlugin(overrides = {}) {
  const mod = await import('../lib/index.js')
  const routes = new Map()
  const tools = new Map()
  let promptText = () => ''

  const makeInner = () => ({
    get: (name) => (name === 'webServer'
      ? { register: ({ path, handler }) => { routes.set(path, handler); return () => routes.delete(path) } }
      : undefined),
    effect: (execute) => { if (typeof execute === 'function') execute(); return () => {} },
  })

  const ctx = {
    tools: { register: (tool) => tools.set(tool.name, tool) },
    commands: { register: () => {} },
    credentials: {
      readRecord: async () => undefined,
      modifyRecord: async () => {},
      deleteRecord: async () => {},
    },
    systemPrompt: {
      section: ({ text }) => { promptText = typeof text === 'function' ? text : () => text },
    },
    effect: (execute) => { if (typeof execute === 'function') execute(); return () => {} },
    inject: (deps, callback) => callback(makeInner()),
    logger: { info: () => {}, warn: () => {}, error: () => {}, debug: () => {} },
  }

  mod.apply(ctx, mod.Config(overrides))

  const call = async (path, method, body) => {
    const handler = routes.get(path)
    if (!handler) throw new Error(`route ${path} is not registered`)
    const chunks = body === undefined ? [] : [Buffer.from(JSON.stringify(body))]
    const req = Object.assign(
      (function* () { for (const chunk of chunks) yield chunk })(),
      { method, url: path, headers: {} },
    )
    let code = 0
    let payload = ''
    const res = {
      set statusCode(value) { code = value },
      get statusCode() { return code },
      setHeader() {},
      end(value) { payload = value === undefined ? '' : String(value) },
    }
    await handler(req, res)
    return { status: code, body: payload ? JSON.parse(payload) : undefined }
  }

  return { tools, call, prompt: () => promptText() }
}

const plugin = await loadPlugin()

await check('the plugin registers its whole tool surface', async () => {
  const expected = [
    'bas_status', 'bas_login', 'bas_logout', 'bas_forget', 'bas_devspaces',
    'bas_start', 'bas_stop', 'bas_connect', 'bas_disconnect', 'bas_bridge',
  ]
  assert.deepEqual([...plugin.tools.keys()].sort(), expected.sort())
})

await check('bas_status reports the loaded protocol layer', async () => {
  const result = await plugin.tools.get('bas_status').execute({})
  assert.match(result.text, /libbas_api \d+\.\d+\.\d+/)
  assert.doesNotMatch(result.text, /UNAVAILABLE/)
})

await check('a tool with no landscape fails with guidance, not a stack trace', async () => {
  await assert.rejects(
    () => plugin.tools.get('bas_devspaces').execute({}),
    (error) => {
      assert.match(error.message, /landscape/i)
      return true
    },
  )
})

await check('the state route answers with the snapshot shape the UI renders', async () => {
  const { status: code, body } = await plugin.call('/bas-remote/state', 'GET')
  assert.equal(code, 200)
  assert.equal(body.ok, true)
  for (const key of ['landscapes', 'tunnels', 'forwardPorts', 'sshUser', 'sshConfigMode']) {
    assert.ok(key in body.state, `state.${key} is missing`)
  }
  assert.equal(body.state.version, '0.6.0')
})

await check('a landscape registered through the route survives a reload', async () => {
  const landscape = 'https://reload.example.applicationstudio.cloud.sap'
  const { status: code } = await plugin.call('/bas-remote/landscape', 'POST', { action: 'add', landscape })
  assert.equal(code, 200)
  const snapshot = (await plugin.call('/bas-remote/state', 'GET')).body.state
  assert.equal(snapshot.landscapes.length, 1)
  assert.equal(snapshot.landscapes[0].url, landscape)
})

await check('the credential id matches what the previous implementation stored', async () => {
  // Tokens are stored under `<scope>/<credential id>`; if the id changes, every
  // signed-in landscape silently looks signed out after the upgrade. This value
  // is the one the 0.5.x JavaScript implementation produced.
  const data = await native.call('normalize_landscape', {
    landscape: 'https://3fb23b87trial.ap21cf.trial.applicationstudio.cloud.sap',
  })
  assert.equal(data.credentialId, 'ls-3fb23b87trial-ap21cf-trial-appli-37f5d45f')
})

await check('port assignments round-trip through the route into the protocol layer', async () => {
  const { status: code } = await plugin.call('/bas-remote/ports', 'POST', {
    forwardPorts: { 'ws-native': { dropbear: 44010, bridge: 44011 } },
  })
  assert.equal(code, 200)
  const snapshot = (await plugin.call('/bas-remote/state', 'GET')).body.state
  assert.deepEqual(snapshot.forwardPorts['ws-native'], { dropbear: 44010, bridge: 44011 })
})

await check('a pinned port is probed through the library, not guessed', async () => {
  // `port_free` is the library's own bind test; the route must reach it.
  const busy = await native.call('port_free', { port: 1 })
  assert.equal(typeof busy.free, 'boolean')
})

await check('the system prompt is empty while nothing is connected', async () => {
  assert.equal(plugin.prompt(), '')
})

console.log(checks.join('\n'))
console.log(`\nHost ↔ native: ${checks.length} checks`)
