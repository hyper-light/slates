// The async surface of the slates Node SDK (§2.3, R6, D-19): AsyncClient wraps the addon's typed
// low-level primitives and resolves each verb's Promise by the completion fd's readiness — the fd is
// wrapped in a net.Socket, which libuv polls (uv_poll), so a landed reply fires 'data' and the
// awaiting Promise resolves; the loop is never blocked. The sync `Client` on the addon is the thin
// blocking facade; this is the primary async form. No tokio and no extra thread — the loop is Node's,
// the readiness is the completion fd's.
//
// For the sandbox test the addon is passed in (loaded via SLATES_NODE_ADDON); a published package
// would load its own addon. The completion socket is created lazily on the first slow path, kept for
// the client's life, and unref'd so it never holds the process open on its own; it is not closed
// mid-life, because the fd is owned by the Rust client (a double close would race its descriptor).

import net from 'node:net';

export class AsyncClient {
  constructor(inner) {
    this._c = inner;
    this._pending = new Map(); // word(string) -> { resolve, reject, poll }
    this._sock = null;
  }

  // Connects to `instance` as a new async client over the loaded `addon`. The rendezvous is a fast
  // blocking handshake done once; every verb after it is async.
  static connect(addon, instance, replyNs, reconnectNs) {
    return new AsyncClient(addon.Client.connect(instance, replyNs, reconnectNs));
  }

  clientId() {
    return this._c.clientId();
  }

  async create(name, sizeBytes, dynamic, fold, requireLocked, base) {
    const { word, fast } = this._c.beginSpinCreate(
      name,
      sizeBytes,
      dynamic,
      fold,
      requireLocked,
      base,
    );
    if (fast != null) return fast;
    return this._await(word, (w) => this._c.pollCreate(w));
  }

  async snapshot(volume) {
    const { word, fast } = this._c.beginSpinSnapshot(volume);
    if (fast != null) return fast;
    return this._await(word, (w) => this._c.pollSnapshot(w));
  }

  async status(volume) {
    const { word, fast } = this._c.beginSpinStatus(volume);
    if (fast != null) return fast;
    return this._await(word, (w) => this._c.pollStatus(w));
  }

  async list() {
    const { word, fast } = this._c.beginSpinList();
    if (fast != null) return fast;
    return this._await(word, (w) => this._c.pollList(w));
  }

  // Resize and destroy return nothing: their poll yields `true` (done) rather than a value, which the
  // pump resolves the awaiting Promise with; the caller sees undefined.
  async resize(volume, sizeBytes, dynamic) {
    const { word, fast } = this._c.beginSpinResize(volume, sizeBytes, dynamic);
    if (fast != null) return undefined;
    await this._await(word, (w) => this._c.pollResize(w));
    return undefined;
  }

  async destroy(volume) {
    const { word, fast } = this._c.beginSpinDestroy(volume);
    if (fast != null) return undefined;
    await this._await(word, (w) => this._c.pollDestroy(w));
    return undefined;
  }

  async createGreen(name, requireEvidence) {
    const { word, fast } = this._c.beginSpinCreateGreen(name, requireEvidence);
    if (fast != null) return fast;
    return this._await(word, (w) => this._c.pollCreateGreen(w));
  }

  async createWork(green, name) {
    const { word, fast } = this._c.beginSpinCreateWork(green, name);
    if (fast != null) return fast;
    return this._await(word, (w) => this._c.pollCreateWork(w));
  }

  async edit(work, path, at, deleteLen, data) {
    const { word, fast } = this._c.beginSpinEdit(work, path, at, deleteLen, data);
    if (fast != null) return undefined;
    await this._await(word, (w) => this._c.pollEdit(w));
    return undefined;
  }

  async submit(work) {
    const { word, fast } = this._c.beginSpinSubmit(work);
    if (fast != null) return fast;
    return this._await(word, (w) => this._c.pollSubmit(w));
  }

  async versions(green) {
    const { word, fast } = this._c.beginSpinVersions(green);
    if (fast != null) return fast;
    return this._await(word, (w) => this._c.pollVersions(w));
  }

  async changedSince(green, version) {
    const { word, fast } = this._c.beginSpinChangedSince(green, version);
    if (fast != null) return fast;
    return this._await(word, (w) => this._c.pollChangedSince(w));
  }

  async rebase(work) {
    const { word, fast } = this._c.beginSpinRebase(work);
    if (fast != null) return fast;
    return this._await(word, (w) => this._c.pollRebase(w));
  }

  // The namespace operations (§4.16), each declaring one WorkOp on a work volume and resolving to
  // undefined. Each defers to _declare, which shares one poll (the WorkOp is built in Rust).
  async unlink(work, path) {
    return this._declare(this._c.beginSpinUnlink(work, path));
  }

  async rename(work, from, to) {
    return this._declare(this._c.beginSpinRename(work, from, to));
  }

  async mkdir(work, path) {
    return this._declare(this._c.beginSpinMkdir(work, path));
  }

  async rmdir(work, path) {
    return this._declare(this._c.beginSpinRmdir(work, path));
  }

  async chmod(work, path, mode) {
    return this._declare(this._c.beginSpinChmod(work, path, mode));
  }

  async symlink(work, path, target) {
    return this._declare(this._c.beginSpinSymlink(work, path, target));
  }

  async link(work, path, target) {
    return this._declare(this._c.beginSpinLink(work, path, target));
  }

  async setXattr(work, path, name, value) {
    return this._declare(this._c.beginSpinSetXattr(work, path, name, value));
  }

  async removeXattr(work, path, name) {
    return this._declare(this._c.beginSpinRemoveXattr(work, path, name));
  }

  // A namespace declaration resolves to undefined; its poll yields `true` (done), which the pump
  // resolves the awaiting Promise with.
  async _declare({ word, fast }) {
    if (fast != null) return undefined;
    await this._await(word, (w) => this._c.pollDeclare(w));
    return undefined;
  }

  // Registers the pending future, arms the completion signal, ensures the reader, and closes the race
  // with a reply that landed between the spin's end and the arm.
  _await(word, poll) {
    return new Promise((resolve, reject) => {
      this._pending.set(word, { resolve, reject, poll });
      this._c.arm();
      this._ensureReader();
      this._pump();
    });
  }

  // Wraps the completion fd in a libuv-polled stream once; its readable 'data' (the fd become
  // readable) drives the pump. unref so a live reader never holds the process open on its own.
  _ensureReader() {
    if (this._sock) return;
    const fd = this._c.completionFd();
    this._sock = new net.Socket({ fd, readable: true, writable: false });
    this._sock.on('data', () => this._pump());
    this._sock.on('error', () => {});
    this._sock.unref();
  }

  // Drains every ready reply and resolves each waiting request by its word; disarms when none remain.
  _pump() {
    for (const word of this._c.takeReady()) {
      const entry = this._pending.get(word);
      if (!entry) continue;
      let value;
      try {
        value = entry.poll(word);
      } catch (err) {
        this._pending.delete(word);
        entry.reject(err);
        continue;
      }
      if (value != null) {
        this._pending.delete(word);
        entry.resolve(value);
      }
    }
    if (this._pending.size === 0) this._c.disarm();
  }
}
