// By-use tests for the slates Node SDK (R5): the addon loads, exposes the typed client's lifecycle
// verbs, and maps a typed refusal to a JS Error rather than crashing. Uses Node's built-in test runner
// (`node:test`) and assertions (`node:assert`) — nothing to npm-install. The addon path comes from
// SLATES_NODE_ADDON (the built `.node`), and the daemon round-trip is gated on SLATES_DAEMON, skipping
// loudly otherwise — the vorpal env-gated discipline. Run: `node --test crates/sdk-node/tests`.

import test from 'node:test';
import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { createRequire } from 'node:module';

const require = createRequire(import.meta.url);
const addonPath = process.env.SLATES_NODE_ADDON;

// Test deadlines in nanoseconds; a production caller derives these from the machine's budgets.
const REPLY_NS = 5_000_000;
const RECONNECT_NS = 10_000_000;

test('the addon loads and exposes the Client surface', (t) => {
  if (!addonPath || !existsSync(addonPath)) {
    t.skip('SLATES_NODE_ADDON is not set to a built .node addon — skipped loudly');
    return;
  }
  const slates = require(addonPath);
  assert.equal(typeof slates.Client, 'function', 'Client class is exported');
  // The lifecycle verbs this slice binds; more (createGreen, edit, submit, land, status) are owed.
  const methods = Object.getOwnPropertyNames(slates.Client.prototype);
  for (const verb of ['create', 'snapshot', 'clientId', 'reconnects']) {
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

test('lifecycle round trip over a live daemon (skipped loudly without one)', (t) => {
  // When SLATES_DAEMON names a built daemon binary, this would spawn the anchor+daemon, connect, create
  // a volume, snapshot it, and assert the ids — the true end-to-end by-use test. The spawn harness is
  // owed; the test skips loudly so the round-trip is never silently missing.
  t.skip('daemon spawn harness for the SDK round-trip is owed (tracked in GAPS)');
});
