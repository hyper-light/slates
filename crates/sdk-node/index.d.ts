// Type definitions for the slates Node SDK (§2.3, D-19). Hand-maintained against the napi surface in
// `crates/sdk-node/src/lib.rs` and the `AsyncClient` wrapper in `async.mjs` (the napi 2.x CLI would
// otherwise generate the class/interface types); the async verbs return the same shapes their sync
// counterparts do, wrapped in a Promise.

// A refused or failed slates operation crosses as a plain JS `Error` carrying the refusal's text
// (the Node SDK has no custom error subclass).

/** A volume's status (the fields `slates status` prints), with napi camelCase keys. */
export interface VolumeStatus {
  id: string
  name: string
  referencedBytes: number
  uniqueBytes: number
  leaseEpoch: number | null
  attachments: number
  head: number
  snapshots: number
  watcher: string
  drifted: string[]
  nfsPort: number | null
  placed: boolean
  mirrorAgeNs: number | null
  hostEpoch: number
}

/** A volume in a listing. */
export interface VolumeEntry {
  id: string
  name: string
  referencedBytes: number
  uniqueBytes: number
  overlay: boolean
}

/** A work volume created over a green: its id (hex) and the green base version its edits submit against. */
export interface WorkVolume {
  id: string
  base: number
}

/** A merge conflict window: the file and the base-coordinate range that met an intervening change. */
export interface ConflictWindow {
  path: string
  at: number
  len: number
  class: number
}

/** The outcome of a submit or rebase: `ok`, the green `version` produced (null on conflict), the `conflicts`. */
export interface MergeOutcome {
  ok: boolean
  version: number | null
  conflicts: ConflictWindow[]
}

/** A landing's result (§4.15) — the finished outcome, or a grant-required record for a human to authorize. */
export type LandingResult = Record<string, unknown>

/**
 * The synchronous client: one blocking call per verb over the daemon's rings. The thin facade over the
 * same daemon `AsyncClient` drives; prefer `AsyncClient` on an event loop.
 */
export class Client {
  static connect(instance: string, replyNs: number, reconnectNs: number): Client
  clientId(): number
  reconnects(): number
  create(name: string, sizeBytes: number, dynamic?: boolean, fold?: boolean, requireLocked?: boolean, base?: string): string
  snapshot(volume: string): number
  status(volume: string): VolumeStatus
  list(): VolumeEntry[]
  resize(volume: string, sizeBytes: number, dynamic?: boolean): void
  destroy(volume: string): void
  createGreen(name: string, requireEvidence?: boolean): string
  createWork(green: string, name: string): WorkVolume
  edit(work: string, path: string, at: number, deleteLen: number, data: Buffer): void
  submit(work: string): MergeOutcome
  rebase(work: string): MergeOutcome
  versions(green: string): number
  changedSince(green: string, version: number): string[]
  unlink(work: string, path: string): void
  rename(work: string, from: string, to: string): void
  mkdir(work: string, path: string): void
  rmdir(work: string, path: string): void
  chmod(work: string, path: string, mode: number): void
  symlink(work: string, path: string, target: string): void
  link(work: string, path: string, target: string): void
  setXattr(work: string, path: string, name: string, value: Buffer): void
  removeXattr(work: string, path: string, name: string): void
  land(volume: string, target: string, snapshot?: number, include?: string[], exclude?: string[], grant?: number): LandingResult
}

/**
 * The async-primary client (R6, D-19): every verb returns a Promise resolved by the completion fd's
 * readiness (a libuv-polled socket/pipe), never blocking the event loop. The fast path resolves within
 * the daemon's spin window without touching the loop.
 */
export class AsyncClient {
  static connect(instance: string, replyNs: number, reconnectNs: number): AsyncClient
  clientId(): number
  create(name: string, sizeBytes: number, dynamic?: boolean, fold?: boolean, requireLocked?: boolean, base?: string): Promise<string>
  snapshot(volume: string): Promise<number>
  status(volume: string): Promise<VolumeStatus>
  list(): Promise<VolumeEntry[]>
  resize(volume: string, sizeBytes: number, dynamic?: boolean): Promise<void>
  destroy(volume: string): Promise<void>
  createGreen(name: string, requireEvidence?: boolean): Promise<string>
  createWork(green: string, name: string): Promise<WorkVolume>
  edit(work: string, path: string, at: number, deleteLen: number, data: Buffer): Promise<void>
  submit(work: string): Promise<MergeOutcome>
  rebase(work: string): Promise<MergeOutcome>
  versions(green: string): Promise<number>
  changedSince(green: string, version: number): Promise<string[]>
  unlink(work: string, path: string): Promise<void>
  rename(work: string, from: string, to: string): Promise<void>
  mkdir(work: string, path: string): Promise<void>
  rmdir(work: string, path: string): Promise<void>
  chmod(work: string, path: string, mode: number): Promise<void>
  symlink(work: string, path: string, target: string): Promise<void>
  link(work: string, path: string, target: string): Promise<void>
  setXattr(work: string, path: string, name: string, value: Buffer): Promise<void>
  removeXattr(work: string, path: string, name: string): Promise<void>
  land(volume: string, target: string, snapshot?: number, include?: string[], exclude?: string[], grant?: number): Promise<LandingResult>
}
