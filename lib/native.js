// dsh-bas-remote — native protocol layer (koffi FFI).
//
// This module is the DSH half's only contact with BAS. Everything that talks
// to a landscape, a dev channel or a dev space lives in `libbas_api` behind the
// C ABI frozen in docs/abi.md; this file loads that library and calls it.
//
// The ABI has one call and one rule:
//
//     char       *bas_call(const char *op, const char *args_json);  // caller owns
//     void        bas_free(char *response);                         // free it once
//
// The operation runs exactly once per call. (A two-call `probe → allocate →
// call` pattern looks friendlier, but "report the size" means running the
// operation to learn the size — so it would open two tunnels, or start two
// bridge daemons. The library returns an owned string for exactly this reason.)
//
// Nothing is loaded at import time. A plugin whose native library is missing
// still registers its tools, and every call fails with
// `NativeError('unavailable', …)` naming what to build or where to point
// `BAS_NATIVE_LIB` — never with a crash on import.

import { existsSync } from 'node:fs'
import { createRequire } from 'node:module'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'

const require = createRequire(import.meta.url)

/** `major.minor` of the ABI in docs/abi.md this host was written against. */
export const ABI_VERSION = '0.6'
const [ABI_MAJOR, ABI_MINOR] = ABI_VERSION.split('.').map((part) => Number.parseInt(part, 10))

/** Operations that are pure and cheap, so a synchronous call site may use them. */
const PURE_OPS = new Set(['normalize_landscape', 'jwt_describe', 'ext_login_url', 'ssh_config_block'])

const LIB_DIR = dirname(fileURLToPath(import.meta.url))
const PKG_DIR = dirname(LIB_DIR)

/**
 * One failure, either from the protocol layer or from this loader.
 *
 * `code` is the ABI's stable `error.code` (`upstream`, `transport`,
 * `token_expired`, `not_found`, `port_in_use`, `bad_body`, `status`, `other`,
 * `internal`) so callers can branch without parsing prose. This loader adds one
 * code of its own, `unavailable`, for "the library could not be loaded or does
 * not speak this ABI".
 */
export class NativeError extends Error {
  /**
   * @param {string} code - stable, branchable failure class.
   * @param {string} message - human-readable prose.
   * @param {object} [options] - standard `Error` options (`cause`).
   */
  constructor(code, message, options) {
    super(message, options)
    this.name = 'NativeError'
    /** @type {string} */
    this.code = code
  }
}

// koffi is loaded eagerly (it is a small native addon, not the BAS library) but
// a failure is remembered rather than thrown, so import stays harmless.
let koffi = null
let koffiError = null
try {
  koffi = require('koffi')
} catch (error) {
  koffiError = error
}

/** @type {{done: boolean, api: {path: string, version: string, call: Function}|null, error: Error|null}} */
const loader = { done: false, api: null, error: null }

/** @returns {string[]} candidate file names for the current platform. */
function libraryFileNames() {
  const own = process.platform === 'darwin'
    ? 'libbas_api.dylib'
    : process.platform === 'win32'
      ? 'bas_api.dll'
      : 'libbas_api.so'
  const all = [own, 'libbas_api.so', 'libbas_api.dylib', 'bas_api.dll', 'libbas_api.dll']
  return [...new Set(all)]
}

/**
 * Where the library may live, in the order the plugin documents:
 * `$BAS_NATIVE_LIB`, then the repository/packaged `native/` directory, then the
 * directory holding this file, then the package root.
 * @returns {string[]} absolute candidate paths.
 */
function bundledCandidates() {
  const paths = []
  for (const name of libraryFileNames()) {
    paths.push(join(PKG_DIR, 'native', name))
    paths.push(join(LIB_DIR, name))
    paths.push(join(PKG_DIR, name))
  }
  return paths
}

/**
 * Pick the library to load.
 * @returns {string} an absolute path, or a bare file name for the platform loader.
 * @throws {NativeError} when `BAS_NATIVE_LIB` names a file that does not exist.
 */
function pickLibrary() {
  const explicit = String(process.env.BAS_NATIVE_LIB ?? '').trim()
  if (explicit) {
    if (!existsSync(explicit)) {
      throw new NativeError('unavailable', `BAS_NATIVE_LIB points at ${explicit}, which does not exist`)
    }
    return explicit
  }
  for (const candidate of bundledCandidates()) {
    if (existsSync(candidate)) return candidate
  }
  // Last resort: a bare name lets dlopen/LoadLibrary search the platform paths
  // (LD_LIBRARY_PATH, DYLD_LIBRARY_PATH, the Windows DLL search order).
  return libraryFileNames()[0]
}

/**
 * Load the library once and check the ABI version it reports.
 * @returns {{path: string, version: string, call: Function}} the loaded symbols.
 * @throws {NativeError} `unavailable` when the library is missing or mismatched.
 */
function ensureApi() {
  if (loader.done) {
    if (loader.error) throw loader.error
    return loader.api
  }
  loader.done = true
  try {
    if (!koffi) {
      throw new NativeError(
        'unavailable',
        `koffi could not be loaded (${koffiError?.message ?? 'not installed'}); run \`npm install\` in the plugin directory`,
      )
    }
    const path = pickLibrary()
    let library
    try {
      library = koffi.load(path)
    } catch (error) {
      throw new NativeError(
        'unavailable',
        `cannot load libbas_api from ${path} (${error.message}); build the Rust half (cargo build --release -p bas-api) or point BAS_NATIVE_LIB at the library`,
      )
    }
    let versionSymbol
    let callSymbol
    let freeSymbol
    try {
      versionSymbol = library.func('bas_version', 'const char *', [])
      // `void *`, not `char *`: with a `char *` return type koffi copies the C
      // string out and drops the pointer, so the library's buffer could never be
      // freed — ~600 bytes leaked per call, measured over 20 000 calls. Keeping
      // the pointer lets `bas_free` release it.
      callSymbol = library.func('bas_call', 'void *', ['const char *', 'const char *'])
      freeSymbol = library.func('bas_free', 'void', ['void *'])
    } catch (error) {
      throw new NativeError('unavailable', `${path} does not export the bas-api ABI (${error.message})`)
    }
    const version = String(versionSymbol() ?? '').trim()
    const [major, minor] = version.split('.').map((part) => Number.parseInt(part, 10))
    if (major !== ABI_MAJOR || minor !== ABI_MINOR) {
      throw new NativeError(
        'unavailable',
        `${path} reports bas-api ${version || '(no version)'}, but this plugin speaks ${ABI_VERSION}; install the matching library or set BAS_NATIVE_LIB`,
      )
    }
    loader.api = { path, version, call: callSymbol, free: freeSymbol }
  } catch (error) {
    loader.error = error instanceof NativeError
      ? error
      : new NativeError('unavailable', error.message, { cause: error })
    throw loader.error
  }
  return loader.api
}

/**
 * Unwrap one response envelope.
 * @param {string} text - the response body the ABI wrote.
 * @param {string} op - operation name, for error messages.
 * @returns {*} the `data` member of a successful envelope.
 * @throws {NativeError} with the envelope's own `error.code`.
 */
function decode(text, op) {
  let parsed
  try {
    parsed = JSON.parse(text)
  } catch (error) {
    throw new NativeError(
      'bad_body',
      `${op} returned something that is not JSON (${error.message}): ${text.slice(0, 200)}`,
      { cause: error },
    )
  }
  if (parsed === null || typeof parsed !== 'object' || Array.isArray(parsed)) {
    throw new NativeError('bad_body', `${op} returned a response that is not an envelope: ${text.slice(0, 200)}`)
  }
  if (parsed.ok === true) return parsed.data
  if (parsed.ok === false) {
    const error = parsed.error ?? {}
    throw new NativeError(
      String(error.code ?? 'other'),
      String(error.message ?? `${op} failed without a message`),
    )
  }
  throw new NativeError('bad_body', `${op} returned an envelope without \`ok\`: ${text.slice(0, 200)}`)
}

/**
 * Read a string `bas_call` returned and release it.
 *
 * The return is declared `void *` so the pointer stays ours: `bas_free` is
 * mandatory, or every call leaks the library's response buffer. A string is
 * still accepted defensively (a future declaration change must not turn into a
 * crash), but then there is no pointer left to free.
 * @param {Function} free - the `bas_free` symbol.
 * @param {string|bigint|number|null} pointer - what `bas_call` returned.
 * @param {string} op - operation name, for error messages.
 * @returns {string} the response text.
 */
function takeResponse(free, pointer, op) {
  if (pointer === null || pointer === undefined) {
    throw new NativeError('internal', `${op} returned no response`)
  }
  // Defensive: a string means the declaration already copied it.
  if (typeof pointer === 'string') return pointer
  if (pointer === 0n || pointer === 0) {
    throw new NativeError('internal', `${op} returned a null response`)
  }
  try {
    return String(koffi.decode(pointer, 'char', -1))
  } finally {
    try {
      free(pointer)
    } catch {
      // A failed free is a leak, not a wrong answer; the response is still valid.
    }
  }
}

/**
 * Perform one call on a worker thread (koffi's `.async` member takes a
 * node-style callback last), so the event loop is never blocked.
 * @param {Function} fn - the `bas_call` symbol.
 * @param {Function} free - the `bas_free` symbol.
 * @param {string} op - operation name.
 * @param {string} argsJson - args object as JSON text.
 * @returns {Promise<*>} the op's `data`.
 */
function invokeAsync(fn, free, op, argsJson) {
  return new Promise((resolve, reject) => {
    try {
      fn.async(op, argsJson, (error, pointer) => {
        if (error) {
          reject(new NativeError('internal', `bas_call(${op}) failed: ${error.message}`, { cause: error }))
          return
        }
        try {
          resolve(decode(takeResponse(free, pointer, op), op))
        } catch (failure) {
          reject(failure)
        }
      })
    } catch (error) {
      reject(new NativeError('internal', `bas_call(${op}) could not be started: ${error.message}`, { cause: error }))
    }
  })
}

/**
 * The same call, synchronously. Only used for the pure helpers in
 * {@link PURE_OPS}: they do no I/O and return in microseconds, and some of them
 * are needed from synchronous code paths (config resolution, the state file).
 * @param {Function} fn - the `bas_call` symbol.
 * @param {Function} free - the `bas_free` symbol.
 * @param {string} op - operation name.
 * @param {string} argsJson - args object as JSON text.
 * @returns {*} the op's `data`.
 */
function invokeSync(fn, free, op, argsJson) {
  let pointer
  try {
    pointer = fn(op, argsJson)
  } catch (error) {
    throw new NativeError('internal', `bas_call(${op}) failed: ${error.message}`, { cause: error })
  }
  return decode(takeResponse(free, pointer, op), op)
}

/**
 * Call one ABI operation and return its `data`.
 *
 * Runs on a koffi worker thread, so a slow op (a dev-space start waits minutes)
 * does not stall the host.
 * @param {string} op - operation name from docs/abi.md, e.g. `tunnel_open`.
 * @param {object} [args] - args object; omitted when the op takes nothing.
 * @returns {Promise<*>} the op's `data` member.
 * @throws {NativeError} with the envelope's `error.code`, or `unavailable`.
 */
export async function call(op, args) {
  const api = ensureApi()
  return invokeAsync(api.call, api.free, op, JSON.stringify(args ?? {}))
}

/**
 * Call one pure ABI operation synchronously.
 *
 * Restricted to {@link PURE_OPS} — no network, no filesystem, no waiting — so
 * it can be used where an `await` is impossible (plugin config at load time,
 * the state file, JWT expiry labels). Anything that can block uses {@link call}.
 * @param {string} op - operation name; must be one of the pure helpers.
 * @param {object} [args] - args object.
 * @returns {*} the op's `data` member.
 * @throws {NativeError} when the op is not pure, or the call fails.
 */
export function callSync(op, args) {
  if (!PURE_OPS.has(op)) {
    throw new NativeError('internal', `${op} is not a pure helper; use call() so the event loop stays free`)
  }
  const api = ensureApi()
  return invokeSync(api.call, api.free, op, JSON.stringify(args ?? {}))
}

/**
 * Whether the native library loaded, and which one.
 *
 * Never throws: the plugin's status report uses it to explain why a call would
 * fail instead of failing while describing itself.
 * @returns {{available: boolean, path: string, version: string, abi: string, error: string}}
 */
export function nativeStatus() {
  try {
    const api = ensureApi()
    return { available: true, path: api.path, version: api.version, abi: ABI_VERSION, error: '' }
  } catch (error) {
    return {
      available: false,
      path: '',
      version: '',
      abi: ABI_VERSION,
      error: error instanceof Error ? error.message : String(error),
    }
  }
}
