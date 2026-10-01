// The async surface of the slates Node SDK (§2.3, R6, D-19): AsyncClient wraps the addon's typed
// low-level primitives and settles each verb's Promise from the client's driver (AUD-29-19, AUD-29-20). A
// call is submitted and gets a ticket at once — sent when the client admits it, queued otherwise, never
// waiting — and the driver hands back events: a reply landed (decoded here by the verb's poll), or the call
// failed, typed. Two inputs feed it: the completion fd's readiness (wrapped in a net.Socket that libuv polls)
// and one timer at the driver's next wake (reply deadlines, reconnect attempts). The timer is referenced
// while a call waits, so the process lives until every call has ended; a reader error, the fd closing or a
// pump exception fails every call. No tokio and no extra thread — the loop is Node's.
//
// For the sandbox test the addon is passed in (loaded via SLATES_NODE_ADDON); a published package
// would load its own addon. The completion socket is created lazily on the first slow path, kept for
// the client's life, and unref'd so it never holds the process open on its own; it is not closed
// mid-life, because the fd is owned by the Rust client (a double close would race its descriptor).

import net from 'node:net';
import { createRequire } from 'node:module';

// The published package loads the napi addon from its generated loader; the sandbox tests pass a
// freshly built addon explicitly (via SLATES_NODE_ADDON), so this is resolved lazily and only in the
// package path — a test never triggers it.
let _defaultAddon = null;
function defaultAddon() {
  if (!_defaultAddon) {
    const require = createRequire(import.meta.url);
    _defaultAddon = require('./index.js');
  }
  return _defaultAddon;
}

// Nanoseconds in a millisecond, the unit setTimeout takes.
const NS_PER_MS = 1e6;

export class AsyncClient {
  constructor(inner) {
    this._c = inner;
    this._pending = new Map(); // ticket(string) -> { resolve, reject, poll }
    this._sock = null;
    this._timer = null;
    this._broken = null; // the error every call fails with once the completion reader is lost
    this._channel = -1; // the reconnect count of the channel the reader is attached to
    this._tickets = new WeakMap(); // the Promise a caller holds -> its ticket, for cancel()
  }

  // Connects to a daemon as a new async client. In the published package: `connect(instance, replyNs,
  // reconnectNs)` — the addon loads itself. In the sandbox tests: `connect(addon, instance, replyNs,
  // reconnectNs)` — an explicit, freshly built addon. The rendezvous claim is made at once and its
  // answer read on timers at the pacing the addon asks for, so a slow, stopped or dead daemon never
  // holds the event loop (AUD-29-19); the Promise rejects typed when the daemon refuses the claim or
  // leaves it unanswered past the claim wait.
  static async connect(a, b, c, d) {
    const [addon, instance, replyNs, reconnectNs] =
      typeof a === 'string' ? [defaultAddon(), a, b, c] : [a, b, c, d];
    const connecting = addon.Connecting.begin(instance, replyNs, reconnectNs);
    for (;;) {
      const client = connecting.poll();
      if (client != null) return new AsyncClient(client);
      const waitMs = Math.max(0, Math.ceil(connecting.nextPollNs() / NS_PER_MS));
      await new Promise((resolve) => setTimeout(resolve, waitMs));
    }
  }

  clientId() {
    return this._c.clientId();
  }

  create(name, sizeBytes, dynamic, fold, requireLocked, base) {
    return this._call(() => this._c.beginSpinCreate(name, sizeBytes, dynamic, fold, requireLocked, base), (w) => this._c.pollCreate(w));
  }

  snapshot(volume) {
    return this._call(() => this._c.beginSpinSnapshot(volume), (w) => this._c.pollSnapshot(w));
  }

  status(volume) {
    return this._call(() => this._c.beginSpinStatus(volume), (w) => this._c.pollStatus(w));
  }

  list() {
    return this._call(() => this._c.beginSpinList(), (w) => this._c.pollList(w));
  }

  // Resize and destroy return nothing: their poll yields `true` (done) rather than a value, which the
  // pump resolves the awaiting Promise with; the caller sees undefined.
  resize(volume, sizeBytes, dynamic) {
    return this._call(() => this._c.beginSpinResize(volume, sizeBytes, dynamic), (w) => this._c.pollResize(w), true);
  }

  destroy(volume) {
    return this._call(() => this._c.beginSpinDestroy(volume), (w) => this._c.pollDestroy(w), true);
  }

  createGreen(name, requireEvidence) {
    return this._call(() => this._c.beginSpinCreateGreen(name, requireEvidence), (w) => this._c.pollCreateGreen(w));
  }

  createWork(green, name) {
    return this._call(() => this._c.beginSpinCreateWork(green, name), (w) => this._c.pollCreateWork(w));
  }

  edit(work, path, at, deleteLen, data) {
    return this._call(() => this._c.beginSpinEdit(work, path, at, deleteLen, data), (w) => this._c.pollEdit(w), true);
  }

  submit(work) {
    return this._call(() => this._c.beginSpinSubmit(work), (w) => this._c.pollSubmit(w));
  }

  versions(green) {
    return this._call(() => this._c.beginSpinVersions(green), (w) => this._c.pollVersions(w));
  }

  changedSince(green, version) {
    return this._call(() => this._c.beginSpinChangedSince(green, version), (w) => this._c.pollChangedSince(w));
  }

  rebase(work) {
    return this._call(() => this._c.beginSpinRebase(work), (w) => this._c.pollRebase(w));
  }

  // The namespace operations (§4.16), each declaring one WorkOp on a work volume and resolving to
  // undefined; they share one poll (the WorkOp is built in Rust).
  unlink(work, path) {
    return this._call(() => this._c.beginSpinUnlink(work, path), (w) => this._c.pollDeclare(w), true);
  }

  rename(work, from, to) {
    return this._call(() => this._c.beginSpinRename(work, from, to), (w) => this._c.pollDeclare(w), true);
  }

  mkdir(work, path) {
    return this._call(() => this._c.beginSpinMkdir(work, path), (w) => this._c.pollDeclare(w), true);
  }

  rmdir(work, path) {
    return this._call(() => this._c.beginSpinRmdir(work, path), (w) => this._c.pollDeclare(w), true);
  }

  chmod(work, path, mode) {
    return this._call(() => this._c.beginSpinChmod(work, path, mode), (w) => this._c.pollDeclare(w), true);
  }

  symlink(work, path, target) {
    return this._call(() => this._c.beginSpinSymlink(work, path, target), (w) => this._c.pollDeclare(w), true);
  }

  link(work, path, target) {
    return this._call(() => this._c.beginSpinLink(work, path, target), (w) => this._c.pollDeclare(w), true);
  }

  setXattr(work, path, name, value) {
    return this._call(() => this._c.beginSpinSetXattr(work, path, name, value), (w) => this._c.pollDeclare(w), true);
  }

  removeXattr(work, path, name) {
    return this._call(() => this._c.beginSpinRemoveXattr(work, path, name), (w) => this._c.pollDeclare(w), true);
  }

  // Executes a landing (§4.15): resolves to the finished outcome, or grant-required with the exact
  // `slates grant` command a human runs. The SDK creates no grant itself (R10).
  land(volume, target, snapshot, include, exclude, grant) {
    return this._call(() => this._c.beginSpinLand(volume, target, snapshot, include, exclude, grant), (w) => this._c.pollLand(w));
  }

  // Cancels a call by the Promise its verb returned (AUD-29-20): queued, it is never sent; in flight, its
  // reply is dropped when it comes. The Promise rejects with an `AbortError`. Returns whether a waiting
  // call was cancelled (false for one already ended, or a Promise this client did not return).
  cancel(promise) {
    const ticket = this._tickets.get(promise);
    if (ticket == null) return false;
    const entry = this._pending.get(ticket);
    if (!entry) return false;
    this._pending.delete(ticket);
    this._guard(() => this._c.cancel(ticket));
    const error = new Error('Cancelled: the caller cancelled the call');
    error.name = 'AbortError';
    entry.reject(error);
    this._settle([]);
    return true;
  }

  // One verb's call: begins it (sent or queued, never waiting), resolves at once when its reply came within
  // the spin, else waits for it by ticket. A refusal at the begin rejects the returned Promise. A unit verb
  // (`unit`) resolves to undefined, its poll yielding `true` when done. The Promise returned is the one
  // `cancel` takes.
  _call(begin, poll, unit = false) {
    let begun;
    try {
      begun = begin();
    } catch (err) {
      return Promise.reject(err);
    }
    if (begun.fast != null) return Promise.resolve(unit ? undefined : begun.fast);
    const waiting = this._await(begun.word, poll);
    const promise = unit ? waiting.then(() => undefined) : waiting;
    this._tickets.set(promise, begun.word);
    return promise;
  }

  // Registers the pending call by its ticket, arms the completion signal, ensures the reader and the timer,
  // and closes the race with a reply that landed between the spin's end and the arm.
  _await(ticket, poll) {
    if (this._broken) return Promise.reject(this._broken);
    return new Promise((resolve, reject) => {
      this._pending.set(ticket, { resolve, reject, poll });
      this._guard(() => {
        this._c.arm();
        this._ensureReader();
        this._settle(this._c.pump());
      });
    });
  }

  // Wraps the current channel's completion fd in a libuv-polled stream; its readable 'data' drives the
  // pump. Each channel has its own fd: when the daemon goes, its fd reaches end of file — the channel is
  // gone, not the reader — so the reader is dropped and the timer drives recovery, and once the client has
  // reconnected the reader attaches to the new channel's fd (and re-arms it). A reader error, or the stream
  // closing without having reached end of file, is the reader lost: every call fails.
  _ensureReader() {
    const channel = this._c.reconnects();
    if (this._sock && this._channel === channel) return;
    if (this._sock && this._channel !== channel) {
      const old = this._sock;
      this._sock = null;
      old.removeAllListeners();
      old.destroy();
    }
    this._channel = channel;
    const fd = this._c.completionFd();
    const sock = new net.Socket({ fd, readable: true, writable: false });
    let ended = false;
    sock.on('data', () => this._guard(() => this._settle(this._c.pump())));
    sock.on('error', (err) => this._lose(err));
    sock.on('end', () => {
      ended = true;
      if (this._sock === sock) this._sock = null;
    });
    sock.on('close', () => {
      if (!ended && this._sock === sock) this._lose(new Error('the completion fd closed'));
    });
    sock.unref();
    this._sock = sock;
  }

  // Runs one step of the loop's work; an exception from the addon (a pump or a tick refused) fails every
  // call rather than escaping the event callback.
  _guard(step) {
    try {
      step();
    } catch (err) {
      this._lose(err);
    }
  }

  // The completion reader is lost: every waiting call fails, and every later call is refused with it.
  _lose(err) {
    if (this._broken) return;
    this._broken = err instanceof Error ? err : new Error(String(err));
    let events = [];
    try {
      events = this._c.failAll(this._broken.message);
    } catch (_) {
      events = [...this._pending.keys()].map((ticket) => ({ ticket, error: this._broken.message }));
    }
    this._settle(events);
    for (const [, entry] of this._pending) entry.reject(this._broken);
    this._pending.clear();
    this._idle();
  }

  // Settles the driver's events: a landed reply is decoded by its verb's poll and resolves its call, a
  // failure rejects it with the typed error's text; then the timer is set for the driver's next wake.
  _settle(events) {
    for (const event of events) {
      const entry = this._pending.get(event.ticket);
      if (!entry) continue;
      if (event.error != null) {
        this._pending.delete(event.ticket);
        entry.reject(new Error(event.error));
        continue;
      }
      let value;
      try {
        value = entry.poll(event.word);
      } catch (err) {
        this._pending.delete(event.ticket);
        this._c.finish(event.ticket);
        entry.reject(err);
        continue;
      }
      if (value != null) {
        this._pending.delete(event.ticket);
        this._c.finish(event.ticket);
        entry.resolve(value);
      }
    }
    if (this._pending.size === 0) {
      this._idle();
    } else {
      // A reconnect gives the client a new channel: attach the reader to its fd and arm it.
      if (!this._broken && (this._sock == null || this._channel !== this._c.reconnects())) {
        // Arm, attach, then pump: a reply that landed before the arm signals nothing, so it is taken now.
        this._guard(() => {
          this._c.arm();
          this._ensureReader();
          this._settle(this._c.pump());
        });
        return;
      }
      this._schedule();
    }
  }

  // Sets the one timer for the driver's next wake. Referenced: a waiting call keeps the process alive until
  // it ends, by its reply or by its deadline.
  _schedule() {
    if (this._timer) clearTimeout(this._timer);
    this._timer = null;
    const wake = this._c.nextWakeNs();
    if (wake == null) return;
    this._timer = setTimeout(() => {
      this._timer = null;
      this._guard(() => this._settle(this._c.tick()));
    }, Math.max(0, Math.ceil(wake / NS_PER_MS)));
  }

  // No call waits: the arm and the timer go.
  _idle() {
    if (this._timer) clearTimeout(this._timer);
    this._timer = null;
    if (!this._broken) this._guard(() => this._c.disarm());
  }
}
