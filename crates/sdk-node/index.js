// Loads the platform-specific slates napi addon (the `Client` and `SlatesError` the sync surface and
// the AsyncClient wrapper use): the locally built `slates.<triple>.node` in a source checkout, else
// the per-platform optional dependency package (`slates-<triple>`) a published install pulls in. The
// async surface (`AsyncClient`) lives in `async.mjs` and is re-exported from `index.mjs`.
//
// Data-driven over slates's target triples (see package.json `napi.targets`), so there is one mapping
// to keep right rather than a per-arch branch. napi-rs would auto-generate a longer loader; this hand-
// written one is equivalent for these targets and needs no build step to exist.
'use strict'

const { existsSync } = require('node:fs')
const { join } = require('node:path')

// Whether this Linux is musl (the C library the prebuilt binary must match). Absent glibc-runtime
// version in the process report means musl; any error falls back to musl, the conservative choice for
// a static binary.
function isMusl() {
  if (process.platform !== 'linux') {
    return false
  }
  try {
    return !process.report.getReport().header.glibcVersionRuntime
  } catch {
    return true
  }
}

// The target triple for this platform/arch, or null if slates ships no binary for it.
function triple() {
  const { platform, arch } = process
  if (platform === 'darwin') {
    return arch === 'arm64' ? 'darwin-arm64' : arch === 'x64' ? 'darwin-x64' : null
  }
  if (platform === 'win32') {
    if (arch === 'x64') return 'win32-x64-msvc'
    if (arch === 'arm64') return 'win32-arm64-msvc'
    if (arch === 'ia32') return 'win32-ia32-msvc'
    return null
  }
  if (platform === 'linux') {
    const libc = isMusl() ? 'musl' : 'gnu'
    if (arch === 'x64') return `linux-x64-${libc}`
    if (arch === 'arm64') return `linux-arm64-${libc}`
    return null
  }
  return null
}

function load() {
  const target = triple()
  if (!target) {
    throw new Error(`slates: no prebuilt binary for ${process.platform}/${process.arch}`)
  }
  const local = join(__dirname, `slates.${target}.node`)
  if (existsSync(local)) {
    return require(local)
  }
  try {
    return require(`slates-${target}`)
  } catch (error) {
    throw new Error(
      `slates: could not load the native addon for ${target}. Install the optional dependency ` +
        `slates-${target}, or build from source (\`npm run build\`). Cause: ${error.message}`,
    )
  }
}

module.exports = load()
