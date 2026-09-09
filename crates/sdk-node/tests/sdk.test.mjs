// By-use tests for the slates Node SDK (R5): the addon loads, exposes the typed client's lifecycle
// verbs, and maps a typed refusal to a JS Error rather than crashing. Uses Node's built-in test runner
// (`node:test`) and assertions (`node:assert`) — nothing to npm-install. The addon path comes from
// SLATES_NODE_ADDON (the built `.node`), and the daemon round-trip is gated on SLATES_DAEMON, skipping
// loudly otherwise — the vorpal env-gated discipline. Run: `node --test crates/sdk-node/tests/sdk.test.mjs`
// (Node's `--test` takes test files, not a bare directory).

import test from 'node:test';
import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { createRequire } from 'node:module';
import { spawn } from 'node:child_process';

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

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

// Retries connect until the just-spawned daemon answers within the startup budget, so the test does not
// race the anchor's boot — each attempt is one real rendezvous, the same the client uses.
async function connectWhenReady(slates, instance) {
  const deadline = Date.now() + STARTUP_MS;
  for (;;) {
    try {
      return slates.Client.connect(instance, REPLY_NS, RECONNECT_NS);
    } catch (error) {
      if (Date.now() >= deadline) throw error;
      await sleep(POLL_MS);
    }
  }
}

test('the addon loads and exposes the Client surface', (t) => {
  if (!addonPath || !existsSync(addonPath)) {
    t.skip('SLATES_NODE_ADDON is not set to a built .node addon — skipped loudly');
    return;
  }
  const slates = require(addonPath);
  assert.equal(typeof slates.Client, 'function', 'Client class is exported');
  // The lifecycle verbs this slice binds; more (createGreen, edit, submit, land) are owed.
  const methods = Object.getOwnPropertyNames(slates.Client.prototype);
  for (const verb of ['create', 'snapshot', 'status', 'list', 'resize', 'destroy', 'clientId', 'reconnects']) {
    assert.ok(methods.includes(verb), `Client.prototype has ${verb}`);
  }
  assert.equal(typeof slates.Client.connect, 'function', 'Client.connect factory is exported');
});

test('connecting to a missing daemon throws a typed error', (t) => {
  if (!addonPath || !existsSync(addonPath)) {
    t.skip('SLATES_NODE_ADDON is not set to a built .node addon — skipped loudly');
    return;
  }
  const slates = require(addonPath);
  assert.throws(
    () => slates.Client.connect('slates-sdk-no-such-instance', REPLY_NS, RECONNECT_NS),
    (error) => {
      const message = String(error.message);
      // The message preserves the refusal kind (a daemon that is not there is "unavailable").
      return message.includes('navailable') || message.includes('Ipc');
    },
    'a missing daemon is a typed refusal, not a crash',
  );
});

test('lifecycle round trip over a live daemon', async (t) => {
  const daemon = process.env.SLATES_DAEMON;
  if (!addonPath || !existsSync(addonPath)) {
    t.skip('SLATES_NODE_ADDON is not set to a built .node addon — skipped loudly');
    return;
  }
  if (!daemon || !existsSync(daemon)) {
    t.skip('SLATES_DAEMON is not set to a built `slates` binary — round-trip skipped loudly');
    return;
  }
  // Spawn a real anchor+daemon, connect the SDK to it, and drive create → snapshot → status end to end
  // (R5). The anchor is a child killed in `finally`, so a failed assertion leaves no daemon behind.
  const slates = require(addonPath);
  const instance = `slates-sdk-node-${process.pid}`;
  // `detached` makes the anchor its own process-group leader, so teardown can kill the whole group —
  // the anchor and the daemon it supervises — at once, rather than leaving the daemon up until its own
  // liveness timeout notices the anchor gone.
  const anchor = spawn(daemon, ['--instance', instance, 'anchor', '--quick', '--shards', '1'], {
    stdio: 'ignore',
    detached: true,
  });
  try {
    const client = await connectWhenReady(slates, instance);

    // create → a 32-hex volume id.
    const volume = client.create('sdk-node-roundtrip', VOLUME_BYTES);
    assert.equal(volume.length, 32);
    assert.equal(typeof client.clientId(), 'number');

    // snapshot → a monotonic sequence.
    const snapshot = client.snapshot(volume);
    assert.equal(typeof snapshot, 'number');

    // status → the volume's real fields, read back over the same rings.
    const status = client.status(volume);
    assert.equal(status.id, volume);
    assert.equal(status.name, 'sdk-node-roundtrip');
    assert.equal(status.attachments, 0);
    assert.equal(status.snapshots, 1);
    // A laptop places the head immediately (f=0) under host epoch 1, with no mirror.
    assert.equal(status.placed, true);
    assert.equal(status.hostEpoch, 1);
    assert.ok(status.mirrorAgeNs == null, 'no mirror on a laptop');
    assert.ok('nfsPort' in status, 'the status carries the NFS port field');
    assert.deepEqual(status.drifted, []);

    // list → the volume appears with its fields (a scratch volume is not an overlay).
    const entry = client.list().find((v) => v.id === volume);
    assert.ok(entry, 'the created volume appears in list');
    assert.equal(entry.name, 'sdk-node-roundtrip');
    assert.equal(entry.overlay, false);

    // resize → a larger bound succeeds.
    client.resize(volume, 2 * VOLUME_BYTES);

    // destroy → the volume is gone from a later list.
    client.destroy(volume);
    assert.ok(
      client.list().every((v) => v.id !== volume),
      'the destroyed volume is gone from list',
    );
  } finally {
    // Kill the whole process group (negative pid) so the supervised daemon goes with the anchor at once.
    try {
      process.kill(-anchor.pid, 'SIGKILL');
    } catch {
      // The group is already gone; nothing to reap.
    }
  }
});
