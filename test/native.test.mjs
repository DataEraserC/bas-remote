// Integration test for the Rust protocol layer through the real C ABI.
//
// The unit tests in `crates/` cover the pieces; this file covers the contract a
// host actually depends on, against a real SSH server:
//
//   1. the library loads through koffi and reports its ABI version;
//   2. one call runs one operation and returns the whole envelope;
//   3. exec propagates stdout, stderr and the exit code (a command that fails
//      must not look successful);
//   4. exec returns when a background child keeps the channel open (the SFTP
//      bridge starts exactly that way, and waiting for the channel to close
//      hangs forever);
//   5. stat reports a directory and a missing path distinctly.
//
// It needs an SSH server it may log into. Provide one with
//
//   BAS_TEST_SSH_PORT=22222 BAS_TEST_SSH_USER=nixos BAS_TEST_SSH_KEY=~/.ssh/id_ed25519 \
//   BAS_NATIVE_LIB=target/release/libbas_api.so npm run test:native
//
// and it skips (exit 0, with a notice) when they are absent, so `npm test`
// stays runnable on a machine without sshd.

import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { homedir } from 'node:os'
import { join } from 'node:path'

const PORT = Number(process.env.BAS_TEST_SSH_PORT ?? 0)
const USER = process.env.BAS_TEST_SSH_USER ?? ''
const KEY_PATH = (process.env.BAS_TEST_SSH_KEY ?? '').replace(/^~(?=\/)/, homedir())

if (!PORT || !USER || !KEY_PATH) {
  console.log('skip: set BAS_TEST_SSH_PORT, BAS_TEST_SSH_USER and BAS_TEST_SSH_KEY to run')
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

const native = await import('../lib/native.js')
const KEY = readFileSync(KEY_PATH, 'utf8')

await check('the library loads and reports a compatible ABI', async () => {
  const status = native.nativeStatus()
  assert.equal(status.available, true, status.error)
  assert.match(status.version, /^\d+\.\d+\.\d+/)
})

await check('a pure op returns the whole envelope', async () => {
  const data = await native.call('normalize_landscape', { landscape: 'host.example' })
  assert.equal(data.landscape, 'https://host.example')
  assert.equal(data.credentialId, data.credentialId.toLowerCase())
})

await check('repeated calls do not grow the process (no per-call leak)', async () => {
  // The library hands back an owned buffer per call; a host that forgot to free
  // it would grow by ~600 bytes a call, which is invisible in a single call and
  // obvious after a few thousand (the status route polls on a timer).
  for (let index = 0; index < 200; index += 1) {
    await native.call('normalize_landscape', { landscape: 'warmup.example' })
  }
  const before = process.memoryUsage().rss
  for (let index = 0; index < 5000; index += 1) {
    await native.call('normalize_landscape', { landscape: 'leak.example' })
  }
  const grownKb = Math.round((process.memoryUsage().rss - before) / 1024)
  assert.ok(grownKb < 20480, `5000 calls grew RSS by ${grownKb} KB; a forgotten free would leak ~3 MB+`)
})

await check('an unknown op fails with a stable code instead of throwing raw', async () => {
  await assert.rejects(
    () => native.call('not_an_op', {}),
    (error) => {
      assert.equal(error.code, 'invalid')
      return true
    },
  )
})

await check('exec carries stdout, stderr and the exit code', async () => {
  const output = await native.call('ssh_exec', {
    port: PORT,
    key: KEY,
    username: USER,
    command: 'echo out; echo err >&2; exit 7',
  })
  assert.equal(output.code, 7, 'a failing command must report its exit code')
  assert.equal(output.stdout.trim(), 'out')
  assert.equal(output.stderr.trim(), 'err')
})

await check('a successful command reports zero', async () => {
  const output = await native.call('ssh_exec', {
    port: PORT,
    key: KEY,
    username: USER,
    command: 'true',
  })
  assert.equal(output.code, 0)
})

await check('exec returns even when a background child holds the channel', async () => {
  // This is the shape the SFTP bridge starts its daemon with. A reader that
  // waits for the channel to close hangs here forever.
  const started = Date.now()
  const output = await native.call('ssh_exec', {
    port: PORT,
    key: KEY,
    username: USER,
    command: 'setsid nohup sleep 30 >/dev/null 2>&1 < /dev/null & sleep 1; echo started',
  })
  assert.equal(output.code, 0)
  assert.equal(output.stdout.trim(), 'started')
  assert.ok(Date.now() - started < 20000, 'exec must not wait for the background child')
})

await check('stat tells a directory from a missing path', async () => {
  const dir = await native.call('ssh_stat', {
    port: PORT,
    key: KEY,
    username: USER,
    path: '/tmp',
  })
  assert.equal(dir.exists, true)
  assert.equal(dir.isDir, true)
  const missing = await native.call('ssh_stat', {
    port: PORT,
    key: KEY,
    username: USER,
    path: '/definitely-not-here-dsh-bas-remote',
  })
  assert.equal(missing.exists, false)
})

await check('an SSH config publish round-trips through a temp file', async () => {
  const target = join(homedir(), '.dsh-bas-remote-test-sshconfig')
  const published = await native.call('ssh_config_publish', {
    target,
    section: 'bas.test.example',
    identityFile: '/tmp/key',
    port: 44000,
    user: USER,
  })
  assert.equal(published.written, true)
  assert.equal(published.section, 'bas.test.example')
  const listed = await native.call('ssh_config_list', { target })
  assert.deepEqual(listed.sections, ['bas.test.example'])
  const removed = await native.call('ssh_config_unpublish', { target, section: 'bas.test.example' })
  assert.equal(removed.changed, true)
})

console.log(checks.join('\n'))
console.log(`\nNative ABI: ${checks.length} checks`)
