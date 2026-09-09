// The ESM entry for the slates Node SDK: the synchronous `Client` from the native addon, and the
// async-primary `AsyncClient` (from async.mjs, which loads the same addon). A refused operation
// crosses as a plain JS `Error` carrying the refusal's text (the Node SDK has no custom error class).
//   import { Client, AsyncClient } from 'slates'
import { createRequire } from 'node:module'
import { AsyncClient } from './async.mjs'

const require = createRequire(import.meta.url)
const addon = require('./index.js')

export const Client = addon.Client
export { AsyncClient }
export default addon
