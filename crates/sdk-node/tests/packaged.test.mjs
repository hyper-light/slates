// By-use test of the PACKAGED Node SDK (R5; docs/publish.md): the tarball `npm pack` produced for
// `@hyper-light/slates` and the one for this machine's platform package are installed into a fresh
// project, and this file, copied into that project, imports the package by name — so it drives the
// exact bytes a user installs: the ESM entry, the loader resolving the addon through the
// `optionalDependencies` platform package (no locally built `.node` exists in an installed package),
// and the async-primary AsyncClient over a live daemon.
//
// Run from inside the project that installed the tarballs (the import below resolves through its
// node_modules), with the daemon named by SLATES_DAEMON; it skips loudly without one:
//   cp crates/sdk-node/tests/packaged.test.mjs "$PROJECT/" && (cd "$PROJECT" && SLATES_DAEMON=<slates> node --test packaged.test.mjs)

import test from 'node:test';
import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { createRequire } from 'node:module';
import { spawn, spawnSync } from 'node:child_process';
import addon, { Client, AsyncClient } from '@hyper-light/slates';

const require = createRequire(import.meta.url);

// Test deadlines in nanoseconds; a production caller derives these from the machine's budgets.
const REPLY_NS = 1_000_000_000;
const RECONNECT_NS = 2_000_000_000;
// How long to wait for a freshly spawned anchor+daemon to answer, and the pause between polls.
const STARTUP_MS = 20_000;
const POLL_MS = 20;
// A small bounded RAM volume for the round-trip.
const VOLUME_BYTES = 8 * 1024 * 1024;

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

// Retries connect until the just-spawned daemon answers within the startup budget, so the test does
// not race the anchor's boot — each attempt is one real rendezvous.
async function connectWhenReady(instance) {
  const deadline = Date.now() + STARTUP_MS;
  for (;;) {
    try {
      return AsyncClient.connect(instance, REPLY_NS, RECONNECT_NS);
    } catch (error) {
      if (Date.now() >= deadline) throw error;
      await sleep(POLL_MS);
    }
  }
}

test('the installed package loads its addon through exactly one platform package', () => {
  assert.equal(typeof Client, 'function', 'the sync Client class is exported');
  assert.equal(typeof AsyncClient, 'function', 'the AsyncClient class is exported');
  assert.equal(addon.Client, Client, 'the default export is the addon the named export came from');
  // The loader prefers a `.node` next to index.js; an installed package ships none, so the addon
  // must have come through the optional dependency for this machine — and only that one resolves.
  const manifest = require('@hyper-light/slates/package.json');
  const resolved = Object.keys(manifest.optionalDependencies).filter((name) => {
    try {
      require.resolve(name);
      return true;
    } catch {
      return false;
    }
  });
  assert.equal(resolved.length, 1, `exactly one platform package is installed: ${resolved}`);
  assert.ok(
    resolved[0].startsWith(`${manifest.name}-`),
    `the platform package name derives from the main name: ${resolved[0]}`,
  );
  const binary = require(`${resolved[0]}/package.json`).main;
  assert.match(binary, /^slates\..+\.node$/, 'the platform package names its binary');
});

test('connecting to a missing daemon throws a typed error, not a crash', () => {
  assert.throws(
    () => Client.connect('slates-packaged-no-such-instance', REPLY_NS, RECONNECT_NS),
    (error) => {
      const message = String(error.message);
      return message.includes('navailable') || message.includes('Ipc');
    },
  );
});

test('the packaged AsyncClient drives a live daemon end to end', async (t) => {
  const daemon = process.env.SLATES_DAEMON;
  if (!daemon || !existsSync(daemon)) {
    t.skip('SLATES_DAEMON is not set to a built `slates` binary — round-trip skipped loudly');
    return;
  }
  const instance = `slates-packaged-${process.pid}`;
  // `detached` makes the anchor its own process-group leader, so teardown kills the whole group —
  // the anchor and the daemon it supervises — at once.
  const anchor = spawn(daemon, ['--instance', instance, 'anchor', '--quick', '--shards', '1'], {
    stdio: 'ignore',
    detached: true,
  });
  try {
    const client = await connectWhenReady(instance);
    assert.equal(typeof client.clientId(), 'number');
    // A fresh daemon refuses every volume verb `ConsensusNotInitialized` until its root group is
    // bootstrapped — the explicit first-time step the SDK suites and the CLI flow take too.
    const bootstrap = spawnSync(daemon, ['--instance', instance, 'bootstrap', 'root'], {
      encoding: 'utf8',
    });
    assert.equal(bootstrap.status, 0, bootstrap.stderr);

    const volume = await client.create('packaged-roundtrip', VOLUME_BYTES);
    assert.equal(volume.length, 32, 'a volume id crosses as 32 hex characters');

    const snapshot = await client.snapshot(volume);
    assert.equal(typeof snapshot, 'number');

    const status = await client.status(volume);
    assert.equal(status.id, volume);
    assert.equal(status.name, 'packaged-roundtrip');
    assert.equal(status.snapshots, 1);
    assert.equal(status.placed, true, 'a laptop places the head immediately (f=0)');

    const listed = (await client.list()).find((entry) => entry.id === volume);
    assert.ok(listed, 'the created volume appears in list');

    await client.destroy(volume);
    // The teardown runs in slices after the reply (§4.4; measured 54–302 µs after it, 2026-09-14),
    // so the list is polled within the startup budget, as the SDK suites and the Rust client test do.
    const goneBy = Date.now() + STARTUP_MS;
    while ((await client.list()).some((entry) => entry.id === volume)) {
      assert.ok(Date.now() < goneBy, 'the destroyed volume is gone from list');
      await sleep(POLL_MS);
    }
  } finally {
    try {
      process.kill(-anchor.pid, 'SIGKILL');
    } catch {
      // The group is already gone.
    }
  }
});
