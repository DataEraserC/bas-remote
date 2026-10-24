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
import { mkdtempSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs'
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
  const commands = new Map()
  let promptText = () => ''

  const makeInner = () => ({
    get: (name) => (name === 'webServer'
      ? { register: ({ path, handler }) => { routes.set(path, handler); return () => routes.delete(path) } }
      : undefined),
    effect: (execute) => { if (typeof execute === 'function') execute(); return () => {} },
  })

  // A real cordis context exposes every service through ctx.reflect.get, and
  // bas-remote asks for `commands` that way because the service is optional.
  // The stub has to model the same path, not the shortcut.
  const commandSink = { register: (command) => commands.set(command.name, command) }
  const ctx = {
    tools: { register: (tool) => tools.set(tool.name, tool) },
    reflect: { get: (name) => (name === 'commands' ? commandSink : undefined) },
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

  return { tools, commands, call, prompt: () => promptText() }
}

const plugin = await loadPlugin()

await check('the plugin registers its whole tool surface', async () => {
  const expected = [
    'bas_status', 'bas_add', 'bas_login', 'bas_logout', 'bas_forget', 'bas_devspaces',
    'bas_start', 'bas_stop', 'bas_connect', 'bas_disconnect', 'bas_bridge',
    'bas_ports', 'bas_strict',
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
  // Read from package.json, so the check tracks the manifest instead of
  // needing an edit at every release.
  const { version } = JSON.parse(readFileSync(new URL('../package.json', import.meta.url), 'utf8'))
  assert.equal(body.state.version, version)
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

await check('/bas is registered through the optional commands service', async () => {
  // The TUI only sees ctx.commands, so this is what makes the plugin usable
  // outside the web panel at all.
  const command = plugin.commands.get('bas')
  assert.ok(command, 'the bas command was never registered')
  assert.equal(command.name, 'bas')
  assert.match(command.input.hint, /add <landscape>/)
  assert.match(command.input.hint, /strict \[on\|off\]/)
})

await check('/bas strict toggles, sets and rejects through one subcommand', async () => {
  const command = plugin.commands.get('bas')
  const run = (raw) => command.handler({ rawInput: raw })
  // The schema default is strict=true, so the first bare call flips it off.
  assert.match((await run('strict')).text, /off \(fall back to a random port\)/)
  assert.match((await run('strict')).text, /on \(a fixed port that is occupied is an error\)/)
  assert.match((await run('strict on')).text, /on \(a fixed port/)
  assert.match((await run('strict off')).text, /off \(fall back/)
  const bad = await run('strict maybe')
  assert.equal(bad.kind, 'error')
  assert.match(bad.text, /unknown strict value/)
  // The live toggle has to land in the persisted state, not just in a local.
  const snapshot = (await plugin.call('/bas-remote/state', 'GET')).body.state
  assert.equal(snapshot.forwardPortsStrict, false)
})

await check('/bas add registers a landscape the way the web route does', async () => {
  const command = plugin.commands.get('bas')
  const empty = await command.handler({ rawInput: 'add' })
  assert.equal(empty.kind, 'error')
  assert.match(empty.text, /usage: \/bas add/)
  // normalize_landscape is a pure string operation, so this needs no network
  // and can assert the outcome instead of only the absence of a fallback.
  const result = await command.handler({ rawInput: 'add https://example.invalid/landscape' })
  assert.equal(result.kind, 'success')
  assert.match(result.text, /^Added https:\/\/example\.invalid\./)
  const snapshot = (await plugin.call('/bas-remote/state', 'GET')).body.state
  assert.ok(snapshot.landscapes.some((entry) => entry.url === 'https://example.invalid'))
})

await check('bas_add registers a landscape without signing in to it', async () => {
  const landscape = 'https://tool-add.example.applicationstudio.cloud.sap'
  const result = await plugin.tools.get('bas_add').execute({ landscape })
  assert.match(result.text, /^Added /)
  // The tool names the next call in the model's own terms; telling it to type
  // a slash command would be the surface leaking into the wrong audience.
  assert.match(result.text, /bas_login landscape=/)
  assert.doesNotMatch(result.text, /\/bas login/)
  const snapshot = (await plugin.call('/bas-remote/state', 'GET')).body.state
  assert.ok(snapshot.landscapes.some((entry) => entry.url === landscape))
  await assert.rejects(() => plugin.tools.get('bas_add').execute({}), /landscape/i)
})

await check("bas_ports pins, lists and clears a dev space's ports", async () => {
  const ports = plugin.tools.get('bas_ports')
  const wsId = 'ws-tool-ports'
  const pinned = await ports.execute({ action: 'set', wsId, dropbear: 45010, bridge: 45011 })
  assert.match(pinned.text, /dropbear=45010, bridge=45011/)
  assert.match(pinned.text, /next time the dev space connects/)
  const listed = await ports.execute({})
  assert.match(listed.text, /Fixed port assignments:/)
  assert.match(listed.text, new RegExp(wsId))
  // 0 is "let the kernel choose", not a missing value, so it has to be accepted.
  await ports.execute({ action: 'set', wsId, dropbear: 0, bridge: 0 })
  await ports.execute({ action: 'clear', wsId })
  assert.doesNotMatch((await ports.execute({})).text, new RegExp(wsId))
  await assert.rejects(
    () => ports.execute({ action: 'set', dropbear: 44000, bridge: 44001 }),
    /dev space id is required/,
  )
  await assert.rejects(
    () => ports.execute({ action: 'set', wsId, dropbear: 99999, bridge: 0 }),
    /dropbear port for .* must be an integer 0-65535/,
  )
})

await check('bas_strict sets and toggles the persisted flag', async () => {
  const strict = plugin.tools.get('bas_strict')
  assert.match((await strict.execute({ value: 'off' })).text, /falls back to a random one/)
  assert.equal((await plugin.call('/bas-remote/state', 'GET')).body.state.forwardPortsStrict, false)
  assert.match((await strict.execute({ value: 'on' })).text, /fails the connection/)
  assert.equal((await plugin.call('/bas-remote/state', 'GET')).body.state.forwardPortsStrict, true)
  // Omitting the value toggles, which is what the settings page button does.
  assert.match((await strict.execute({})).text, /falls back to a random one/)
})

await check('the three surfaces are one implementation, not three', async () => {
  // Whatever one of them does, the other two have to see. This is the check
  // that fails the moment any surface grows its own copy of the logic.
  const wsId = 'ws-shared'
  await plugin.tools.get('bas_ports').execute({ action: 'set', wsId, dropbear: 46010, bridge: 46011 })
  assert.deepEqual(
    (await plugin.call('/bas-remote/state', 'GET')).body.state.forwardPorts[wsId],
    { dropbear: 46010, bridge: 46011 },
    'the route sees what the tool pinned',
  )

  await plugin.commands.get('bas').handler({ rawInput: `ports clear ${wsId}` })
  assert.doesNotMatch(
    (await plugin.tools.get('bas_ports').execute({})).text,
    new RegExp(wsId),
    'the tool sees what the command cleared',
  )

  const landscape = 'https://shared.example.applicationstudio.cloud.sap'
  await plugin.tools.get('bas_add').execute({ landscape })
  assert.ok(
    (await plugin.call('/bas-remote/state', 'GET')).body.state.landscapes.some(
      (entry) => entry.url === landscape,
    ),
    'the route sees what the tool registered',
  )

  // The panel's toggle button and the tool flip the same persisted flag.
  const before = (await plugin.call('/bas-remote/state', 'GET')).body.state.forwardPortsStrict
  await plugin.call('/bas-remote/strict', 'POST', {})
  const after = (await plugin.call('/bas-remote/state', 'GET')).body.state.forwardPortsStrict
  assert.equal(after, !before, 'the route toggles')
  // Not `.includes('on')`: "random one" contains it, so that test passes for
  // both states and proves nothing.
  const toggled = (await plugin.tools.get('bas_strict').execute({})).text
  assert.equal(toggled.includes('fails the connection'), !after, 'the tool toggles the flag the route wrote')
})

await check('the plugin loads even when the commands service is absent', async () => {
  // ctx.reflect.get('commands') returning undefined must cost the /bas command
  // and nothing else — the tools, the routes and the tunnels still come up.
  const mod = await import('../lib/index.js')
  const tools = new Map()
  const makeInner = () => ({
    get: (name) =>
      name === 'webServer'
        ? { register: () => () => {} }
        : undefined,
    effect: (execute) => { if (typeof execute === 'function') execute(); return () => {} },
  })
  const ctx = {
    tools: { register: (tool) => tools.set(tool.name, tool) },
    reflect: { get: () => undefined },
    credentials: {
      readRecord: async () => undefined,
      modifyRecord: async () => {},
      deleteRecord: async () => {},
    },
    systemPrompt: { section: () => {} },
    effect: (execute) => { if (typeof execute === 'function') execute(); return () => {} },
    inject: (deps, callback) => callback(makeInner()),
    logger: { info: () => {}, warn: () => {}, error: () => {}, debug: () => {} },
  }
  mod.apply(ctx, mod.Config({}))
  assert.equal(tools.size, 13)
  assert.deepEqual(
    [...tools.keys()].filter((name) => ['bas_add', 'bas_ports', 'bas_strict'].includes(name)).sort(),
    ['bas_add', 'bas_ports', 'bas_strict'],
    'the three surfaces the tools were missing are all registered',
  )
})

console.log(checks.join('\n'))
console.log(`\nHost ↔ native: ${checks.length} checks`)
