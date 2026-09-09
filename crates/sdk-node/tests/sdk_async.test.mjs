// By-use tests for the slates Node SDK's ASYNC surface (R6, D-19): AsyncClient's verbs return
// Promises resolved by the completion fd's readiness — the fd is wrapped in a net.Socket that libuv
// polls (uv_poll), so a landed reply fires 'data' and the Promise resolves, never blocking the loop.
// The sync `Client` on the addon is the thin blocking facade; this is the primary async form. Uses
// Node's built-in test runner (`node:test`); the addon path comes from SLATES_NODE_ADDON and the
// daemon round-trip from SLATES_DAEMON, skipping loudly otherwise. Run:
//   SLATES_NODE_ADDON=<.node> SLATES_DAEMON=<slates> node --test crates/sdk-node/tests/sdk_async.test.mjs

import test from 'node:test';
import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { createRequire } from 'node:module';
import { spawn } from 'node:child_process';
import { AsyncClient } from '../async.mjs';

const require = createRequire(import.meta.url);
const addonPath = process.env.SLATES_NODE_ADDON;

// Test deadlines in nanoseconds; a production caller derives these from the machine's budgets.
const REPLY_NS = 5_000_000;
const RECONNECT_NS = 10_000_000;
// How long to wait for a freshly spawned anchor+daemon to answer, and the pause between polls.
const STARTUP_MS = 20_000;
const POLL_MS = 20;
// A small bounded RAM volume for the round-trip.
const VOLUME_BYTES = 8 * 1024 * 1024;
// How many creates to drive at once, to prove concurrent awaits each get their own reply.
const CONCURRENCY = 8;

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

// Retries connect until the just-spawned daemon answers within the startup budget, so the test does
// not race the anchor's boot — each attempt is one real rendezvous.
async function connectWhenReady(addon, instance) {
  const deadline = Date.now() + STARTUP_MS;
  for (;;) {
    try {
      return AsyncClient.connect(addon, instance, REPLY_NS, RECONNECT_NS);
    } catch (error) {
      if (Date.now() >= deadline) throw error;
      await sleep(POLL_MS);
    }
  }
}

test('the addon exposes the async low-level primitives', (t) => {
  if (!addonPath || !existsSync(addonPath)) {
    t.skip('SLATES_NODE_ADDON is not set to a built .node addon — skipped loudly');
    return;
  }
  const slates = require(addonPath);
  const methods = Object.getOwnPropertyNames(slates.Client.prototype);
  const expected = [
    'beginSpinCreate', 'pollCreate', 'beginSpinSnapshot', 'pollSnapshot',
    'beginSpinStatus', 'pollStatus', 'beginSpinList', 'pollList',
    'beginSpinResize', 'pollResize', 'beginSpinDestroy', 'pollDestroy',
    'completionFd', 'arm', 'disarm', 'takeReady',
  ];
  for (const verb of expected) {
    assert.ok(methods.includes(verb), `Client.prototype has ${verb}`);
  }
});

test('async lifecycle over a live daemon', async (t) => {
  const daemon = process.env.SLATES_DAEMON;
  if (!addonPath || !existsSync(addonPath)) {
    t.skip('SLATES_NODE_ADDON is not set to a built .node addon — skipped loudly');
    return;
  }
  if (!daemon || !existsSync(daemon)) {
    t.skip('SLATES_DAEMON is not set to a built `slates` binary — async round-trip skipped loudly');
    return;
  }
  // Spawn a real anchor+daemon, connect the async client, and await create → snapshot → status, then
  // drive many creates concurrently — each await resolved by the completion fd, each reply routed to
  // its own request (R5, R6). The anchor is killed in `finally`, so a failed assertion leaves nothing.
  const slates = require(addonPath);
  const instance = `slates-node-async-${process.pid}`;
  // `detached` makes the anchor its own process-group leader, so teardown kills the whole group — the
  // anchor and the daemon it supervises — at once.
  const anchor = spawn(daemon, ['--instance', instance, 'anchor', '--quick', '--shards', '1'], {
    stdio: 'ignore',
    detached: true,
  });
  try {
    const client = await connectWhenReady(slates, instance);
    assert.equal(typeof client.clientId(), 'number');

    // await create → a 32-hex volume id.
    const volume = await client.create('node-async-roundtrip', VOLUME_BYTES);
    assert.equal(volume.length, 32);

    // await snapshot → a monotonic sequence.
    const snapshot = await client.snapshot(volume);
    assert.equal(typeof snapshot, 'number');

    // await status → the volume's real fields, the same shape the sync verb returns.
    const status = await client.status(volume);
    assert.equal(status.id, volume);
    assert.equal(status.name, 'node-async-roundtrip');
    assert.equal(status.snapshots, 1);
    assert.equal(status.placed, true);
    assert.equal(status.hostEpoch, 1);

    // await list → the volume appears with its fields (a scratch volume is not an overlay).
    const listed = await client.list();
    const entry = listed.find((v) => v.id === volume);
    assert.ok(entry, 'the created volume appears in the async list');
    assert.equal(entry.name, 'node-async-roundtrip');
    assert.equal(entry.overlay, false);

    // await resize → a larger bound; a unit verb resolves to undefined.
    assert.equal(await client.resize(volume, 2 * VOLUME_BYTES), undefined);

    // await destroy → the volume is gone from a later list; resolves to undefined.
    assert.equal(await client.destroy(volume), undefined);
    const after = await client.list();
    assert.ok(after.every((v) => v.id !== volume), 'the destroyed volume is gone from the list');

    // Concurrency: many creates awaited at once, each its own reply — the multiplexing an async client
    // relies on (one reader serves them all, replies matched to requests by id).
    const names = Array.from({ length: CONCURRENCY }, (_, i) => `node-async-c${i}`);
    const ids = await Promise.all(names.map((name) => client.create(name, VOLUME_BYTES)));
    assert.equal(ids.length, CONCURRENCY);
    assert.equal(new Set(ids).size, CONCURRENCY, 'each concurrent create got a distinct volume id');

    // Merge workflow (§4.16): a green, a work over it, a content edit, a clean submit — the async
    // merge loop, awaited on the same loop.
    const green = await client.createGreen('g-node-async');
    assert.equal(green.length, 32);
    const work = await client.createWork(green, 'w-node-async');
    assert.equal(work.id.length, 32);
    assert.equal(typeof work.base, 'number');
    // edit resolves to undefined (a unit verb); it creates the file on write.
    assert.equal(
      await client.edit(work.id, '/notes.txt', 0, 0, Buffer.from('hello async merge')),
      undefined,
    );
    const outcome = await client.submit(work.id);
    assert.equal(outcome.ok, true, 'the async submit landed cleanly');
    assert.deepEqual(outcome.conflicts, []);
    assert.equal(typeof outcome.version, 'number');

    // await versions/changedSince → the green advanced past the work's base and lists the edit.
    assert.ok((await client.versions(green)) > work.base, 'the green advanced past the work base');
    const changed = await client.changedSince(green, work.base);
    assert.ok(changed.some((p) => p.includes('notes.txt')), 'changedSince lists the edit');
    // await rebase → a fresh work off the advanced head rebases cleanly.
    const fresh = await client.createWork(green, 'w2-node-async');
    const rebased = await client.rebase(fresh.id);
    assert.equal(rebased.ok, true, 'a fresh async work rebases cleanly');

    // namespace operations (§4.16): build a tree on a work volume with every op — each awaited and
    // resolving to undefined — then submit; the async form of the sync suite's namespace loop.
    const nsWork = await client.createWork(green, 'w-ns-node-async');
    const nsId = nsWork.id;
    assert.equal(await client.edit(nsId, '/keep.txt', 0, 0, Buffer.from('keep')), undefined);
    assert.equal(await client.edit(nsId, '/gone.txt', 0, 0, Buffer.from('gone')), undefined);
    assert.equal(await client.unlink(nsId, '/gone.txt'), undefined);
    assert.equal(await client.mkdir(nsId, '/d'), undefined);
    assert.equal(await client.rename(nsId, '/keep.txt', '/d/keep.txt'), undefined);
    assert.equal(await client.chmod(nsId, '/d/keep.txt', 0o600), undefined);
    assert.equal(await client.symlink(nsId, '/d/link', 'keep.txt'), undefined);
    assert.equal(await client.link(nsId, '/d/hard.txt', '/d/keep.txt'), undefined);
    assert.equal(await client.setXattr(nsId, '/d/keep.txt', 'user.slates', Buffer.from('1')), undefined);
    assert.equal(await client.removeXattr(nsId, '/d/keep.txt', 'user.slates'), undefined);
    const nsOutcome = await client.submit(nsId);
    assert.equal(nsOutcome.ok, true, 'the async namespace ops submitted cleanly');
  } finally {
    // Kill the whole process group so the supervised daemon goes with the anchor at once.
    try {
      process.kill(-anchor.pid, 'SIGKILL');
    } catch {
      // already gone
    }
  }
});
