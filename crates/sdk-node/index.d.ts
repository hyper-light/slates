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
  /** Drifted paths the report did not carry; `slates status --drift` walks them (A-267). */
  driftedElided: number
  nfsPort: number | null
  placed: boolean
  mirrorAgeNs: number | null
  hostEpoch: number
}

/** One drifted entry of an overlay: its path and what the disk did to it. */
export interface DriftEntry {
  path: string
  kind: 'Modified' | 'Deleted' | 'Replaced' | 'TypeChanged'
}

/** An overlay's whole drift, read a page at a time (A-267): `unverified` holds the entries whose re-check the host
 * refused now, `oversized` counts the entries too long for any reply. */
export interface DriftedResult {
  drifted: DriftEntry[]
  unverified: string[]
  oversized: number
}

/** A volume in a listing. `notRecovered` is null, or for a volume recovery could not rebuild its cause's kind. */
export interface VolumeEntry {
  id: string
  name: string
  referencedBytes: number
  uniqueBytes: number
  overlay: boolean
  liveShipped: boolean
  notRecovered: string | null
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
  /** Spins this long (nanoseconds) for a synchronous call's reply before parking; `null` restores the wake estimate. */
  spinFor(spinNs: number | null): void
  /** `[parks, replies]`: the synchronous calls that parked for their reply, and the replies taken. */
  parkRatio(): [number, number]
  create(name: string, sizeBytes: number, dynamic?: boolean, fold?: boolean, requireLocked?: boolean, base?: string): string
  snapshot(volume: string): number
  status(volume: string): VolumeStatus
  /** Every drifted entry of an overlay, past what a status carries (A-267). */
  drifted(volume: string): DriftedResult
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
  copy(work: string, from: string, to: string, version?: number, replace?: boolean): void
  land(volume: string, target: string, snapshot?: number, include?: string[], exclude?: string[], grant?: number): LandingResult
}

/**
 * The async-primary client (R6, D-19): every verb returns a Promise resolved by the completion fd's
 * readiness (a libuv-polled socket/pipe), never blocking the event loop. The fast path resolves within
 * the daemon's spin window without touching the loop. A call waits while its daemon lives; a daemon that
 * will never answer is ended by its anchor or its own watchdog (A-232). Every call ends: answered, refused
 * typed, `DaemonGone` when recovery finds no daemon, or `CompletionLost` when the completion reader fails
 * (AUD-29-20). Calls past the outstanding bound are refused
 * `TooManyOutstanding` at once.
 */
export class AsyncClient {
  /**
   * Connects without holding the event loop (AUD-29-19): the rendezvous claim is made at once and its
   * answer read on timers; rejects typed when the daemon refuses the claim or leaves it unanswered.
   */
  static connect(instance: string, replyNs: number, reconnectNs: number): Promise<AsyncClient>
  /**
   * Cancels a call by the Promise its verb returned (AUD-29-20): a queued call is never sent, an
   * in-flight one's reply is dropped, and the Promise rejects with an `AbortError`. Returns whether a
   * waiting call was cancelled.
   */
  cancel(promise: Promise<unknown>): boolean
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
  copy(work: string, from: string, to: string, version?: number, replace?: boolean): Promise<void>
  land(volume: string, target: string, snapshot?: number, include?: string[], exclude?: string[], grant?: number): Promise<LandingResult>
}

/** What `execCommand` takes beside the request (A-370). */
export interface ExecOptions {
  /**
   * The launcher's AppArmor option, off unless `true`: on a host whose AppArmor restricts unprivileged user namespaces
   * (Ubuntu 24.04 and later), the launcher makes its namespace under slates' AppArmor profile for `program`, which an
   * administrator loads as root (`apparmorProfile`); off, it never uses that profile.
   */
  apparmorProfile?: boolean
  /** The executable that serves the launcher: `slates` (found on the `PATH`) unless named. */
  program?: string
}

/**
 * The command that runs slates' chosen-path launcher (`slates exec`, §4.6; A-370): `volume` shown at the existing
 * directory `at` for `command`, in a namespace of its own (Linux), as an argument list for
 * `child_process.spawn(argv[0], argv.slice(1))`. The launcher finds the volume under `SLATES_ROOT` in the environment it
 * is given. Throws for an empty command.
 */
export function execCommand(volume: string, at: string, command: string[], options?: ExecOptions): string[]

/**
 * slates' AppArmor profile for `executable`, the absolute real path of the binary that runs the launcher (A-370): the
 * text an administrator saves under `/etc/apparmor.d` and loads with `apparmor_parser -r`, as root. Throws for a path
 * the profile cannot name exactly.
 */
export function apparmorProfile(executable: string): string
/** The client methods that declare a work's operations, one an operation the SDK offers (A-626 part 4). */
export declare function workOpMethods(): string[]
