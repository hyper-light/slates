// By-use tests for the slates Node SDK (R5): the addon loads, exposes the typed client's lifecycle
// verbs, and maps a typed refusal to a JS Error rather than crashing. Uses Node's built-in test runner
// (`node:test`) and assertions (`node:assert`) — nothing to npm-install. The addon path comes from
// SLATES_NODE_ADDON (the built `.node`), and the daemon round-trip is gated on SLATES_DAEMON, skipping
// loudly otherwise — the vorpal env-gated discipline. Run: `node --test crates/sdk-node/tests/sdk.test.mjs`
// (Node's `--test` takes test files, not a bare directory).

import test from 'node:test';
import assert from 'node:assert/strict';
import { closeSync, existsSync, mkdirSync, mkdtempSync, rmSync, rmdirSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createRequire } from 'node:module';
import { spawnSync } from 'node:child_process';
import { endAnchor, startAnchor } from './anchor.mjs';

const require = createRequire(import.meta.url);
const addonPath = process.env.SLATES_NODE_ADDON;

// Test deadlines in nanoseconds; a production caller derives these from the machine's budgets.
const REPLY_NS = 1_000_000_000;
const RECONNECT_NS = 2_000_000_000;
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

test('the addon loads and exposes the Client surface', async (t) => {
  if (!addonPath || !existsSync(addonPath)) {
    t.skip('SLATES_NODE_ADDON is not set to a built .node addon — skipped loudly');
    return;
  }
  const slates = require(addonPath);
  assert.equal(typeof slates.Client, 'function', 'Client class is exported');
  // The lifecycle and merge verbs this slice binds; `land` and the async form are owed.
  const methods = Object.getOwnPropertyNames(slates.Client.prototype);
  const expected = [
    'create', 'snapshot', 'status', 'list', 'resize', 'destroy',
    'createGreen', 'createWork', 'versions', 'changedSince', 'edit', 'submit', 'rebase',
    'unlink', 'rename', 'mkdir', 'rmdir', 'chmod', 'symlink', 'link', 'setXattr', 'removeXattr', 'copy',
    'clientId', 'reconnects', 'spinFor', 'parkRatio', 'sessionLog',
  ];
  for (const verb of expected) {
    assert.ok(methods.includes(verb), `Client.prototype has ${verb}`);
  }
  // Every work operation the SDK offers (its exhaustive map over the wire's `WorkOp`, A-626 part 4) is a verb of the
  // native client and of the async one, so an appended operation reaches this surface whole or breaks its build.
  const offered = slates.workOpMethods();
  assert.ok(offered.includes('copy'), JSON.stringify(offered));
  const { AsyncClient } = await import('../async.mjs');
  const asyncMethods = Object.getOwnPropertyNames(AsyncClient.prototype);
  for (const verb of offered) {
    assert.ok(methods.includes(verb), `Client.prototype has ${verb}`);
    assert.ok(asyncMethods.includes(verb), `AsyncClient.prototype has ${verb}`);
  }
  assert.equal(typeof slates.Client.connect, 'function', 'Client.connect factory is exported');
});

// A-370, the chosen-path launcher's AppArmor option from Node (§4.6 "The launcher"). Do: build the launcher's command
// with no options, with `{ apparmorProfile: true }` and with it `false`, and an empty command; render slates' AppArmor
// profile for the installed binary's path and for one it cannot name exactly. Expect: the option off unless asked and on
// exactly when asked, in every surface's grammar (each value after `=`); the profile named by the path granting
// `userns,`; and a JS error for the empty command and the unnamable path.
test('execCommand carries the AppArmor option only when asked, and apparmorProfile names the executable', (t) => {
  if (!addonPath || !existsSync(addonPath)) {
    t.skip('SLATES_NODE_ADDON is not set to a built .node addon — skipped loudly');
    return;
  }
  const slates = require(addonPath);
  assert.deepEqual(
    slates.execCommand('scratch', '/home/u/build', ['cargo', 'build', '--', '-x']),
    ['slates', 'exec', '--volume=scratch', '--at=/home/u/build', '--', 'cargo', 'build', '--', '-x'],
  );
  assert.deepEqual(
    slates.execCommand('scratch', '/home/u/build', ['cargo', 'build'], {
      apparmorProfile: true,
      program: '/usr/local/bin/slates',
    }),
    ['/usr/local/bin/slates', 'exec', '--volume=scratch', '--at=/home/u/build', '--apparmor-profile', '--', 'cargo', 'build'],
  );
  assert.ok(!slates.execCommand('v', '/p', ['true'], { apparmorProfile: false }).includes('--apparmor-profile'));
  assert.throws(() => slates.execCommand('v', '/p', []), /exec needs a command/);
  const profile = slates.apparmorProfile('/usr/local/bin/slates');
  assert.ok(profile.includes('profile /usr/local/bin/slates flags=(unconfined) {\n  userns,\n}'), profile);
  assert.throws(() => slates.apparmorProfile('/opt/my tools/slates'), /cannot name exactly/);
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
  const anchor = startAnchor(daemon, instance);
  try {
    const client = await connectWhenReady(slates, instance);
    const bootstrap = spawnSync(daemon, ['--instance', instance, 'bootstrap', 'root'], {
      encoding: 'utf8', timeout: STARTUP_MS,
    });
    assert.equal(bootstrap.status, 0, bootstrap.stderr);

    // create → a 32-hex volume id.
    const volume = client.create('sdk-node-roundtrip', VOLUME_BYTES);
    assert.equal(volume.length, 32);
    assert.equal(typeof client.clientId(), 'number');

    // snapshot → a monotonic sequence.
    const snapshot = client.snapshot(volume);
    assert.equal(typeof snapshot, 'number');

    // A hostile volume id (the hostile-input rule for every parser of external bytes): 32 bytes whose first
    // pair splits a multi-byte character is refused as a JS error, and the process lives on. Before
    // 2026-09-29 the parser sliced the string through the character and panicked.
    assert.throws(
      () => client.status('a\u20ac' + '0'.repeat(28)),
      /not a hex byte/,
      'a volume id that is not two hex digits a byte is a typed refusal, not a crash',
    );

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
    // drifted → every drifted entry, past what a status carries (A-267): a scratch volume has none.
    assert.deepEqual(client.drifted(volume), { drifted: [], unverified: [], oversized: 0 });

    // list → the volume appears with its fields (a scratch volume is not an overlay).
    const entry = client.list().find((v) => v.id === volume);
    assert.ok(entry, 'the created volume appears in list');
    assert.equal(entry.name, 'sdk-node-roundtrip');
    assert.equal(entry.overlay, false);

    // resize → a larger bound succeeds.
    client.resize(volume, 2 * VOLUME_BYTES);

    // destroy → the volume is gone from a later list. The teardown runs in slices after the reply
    // (§4.4; measured 54–302 µs after it, 2026-09-14), so the list is polled within the startup
    // budget, as the Rust client test does.
    client.destroy(volume);
    const goneBy = Date.now() + STARTUP_MS;
    while (client.list().some((v) => v.id === volume)) {
      assert.ok(Date.now() < goneBy, 'the destroyed volume is gone from list');
      await sleep(POLL_MS);
    }

    // merge workflow (§4.16): a green, a work over it, a content edit, a clean submit.
    const green = client.createGreen('g-node-roundtrip');
    assert.equal(green.length, 32);
    const work = client.createWork(green, 'w-node-roundtrip');
    assert.equal(work.id.length, 32);
    const base = work.base;
    client.edit(work.id, '/notes.txt', 0, 0, Buffer.from('hello merge'));
    const outcome = client.submit(work.id);
    assert.equal(outcome.ok, true, `the submit landed cleanly: ${JSON.stringify(outcome)}`);
    assert.equal(typeof outcome.version, 'number');
    assert.deepEqual(outcome.conflicts, []);
    // the green advanced past the work's base and lists the edited file.
    assert.ok(client.versions(green) > base);
    const changed = client.changedSince(green, base);
    assert.ok(changed.some((p) => p.includes('notes.txt')), JSON.stringify(changed));
    // a fresh work off the advanced head rebases cleanly (no pending edits, nothing to conflict).
    const fresh = client.createWork(green, 'w2-node-roundtrip');
    const rebased = client.rebase(fresh.id);
    assert.equal(rebased.ok, true, `a fresh work rebases cleanly: ${JSON.stringify(rebased)}`);

    // namespace operations (§4.16): a directory tree with a rename, an unlink, a mode change, a
    // symlink, a hard link and an xattr on a work volume, then submit — the counterpart to edit.
    const nsWork = client.createWork(green, 'w-ns-node-roundtrip');
    const nsBase = nsWork.base;
    const w = nsWork.id;
    client.edit(w, '/keep.txt', 0, 0, Buffer.from('keep'));
    client.edit(w, '/gone.txt', 0, 0, Buffer.from('gone'));
    client.unlink(w, '/gone.txt');
    client.mkdir(w, '/d');
    client.rename(w, '/keep.txt', '/d/keep.txt');
    client.chmod(w, '/d/keep.txt', 0o600);
    client.symlink(w, '/d/link', 'keep.txt');
    client.link(w, '/d/hard.txt', '/d/keep.txt');
    client.setXattr(w, '/d/keep.txt', 'user.slates', Buffer.from('1'));
    const nsOutcome = client.submit(w);
    assert.equal(nsOutcome.ok, true, `the namespace ops submitted cleanly: ${JSON.stringify(nsOutcome)}`);
    const nsChanged = client.changedSince(green, nsBase);
    assert.ok(nsChanged.some((p) => p.includes('keep.txt')), JSON.stringify(nsChanged));

    // a copy (A-626 part 4): a work lands `/c/x.txt`; another copies `/c`'s tree to `/e` from its base by reference
    // and submits; the CLI reads the copied file's bytes at the green's head.
    const source = client.createWork(green, 'w-copy-source-node');
    client.edit(source.id, '/c/x.txt', 0, 0, Buffer.from('copied'));
    assert.equal(client.submit(source.id).ok, true);
    const copying = client.createWork(green, 'w-copy-node');
    client.copy(copying.id, '/c', '/e');
    const copied = client.submit(copying.id);
    assert.equal(copied.ok, true, `the copy submits: ${JSON.stringify(copied)}`);
    const read = spawnSync(daemon, ['--instance', instance, 'read', green, '/e/x.txt'], { encoding: 'utf8' });
    assert.equal(read.status, 0, read.stderr);
    assert.equal(read.stdout, 'copied');
    // a copy of nothing is refused before any verdict, typed by its case (A-586).
    const empty = client.createWork(green, 'w-copy-nothing-node');
    client.copy(empty.id, '/nothing', '/z');
    assert.throws(() => client.submit(empty.id), /CopyRefused.*source_empty/);

    // the session log (A-773), `slates log --json`'s view in camelCase: two works over a fresh green edit one span
    // differently; the second collides and is the one decision that needs a person.
    const watched = client.createGreen('g-log-node');
    const first = client.createWork(watched, 'w-log-first-node');
    const second = client.createWork(watched, 'w-log-second-node');
    client.edit(first.id, '/f.txt', 0, 0, Buffer.from('one'));
    client.edit(second.id, '/f.txt', 0, 0, Buffer.from('two'));
    assert.equal(client.submit(first.id).ok, true);
    assert.equal(client.submit(second.id).ok, false);
    const log = client.sessionLog();
    const needs = log.needsYou.filter((n) => n.work === second.id);
    assert.equal(needs.length, 1, JSON.stringify(log.needsYou));
    assert.equal(needs[0].kind, 'collided');
    assert.equal(needs[0].path, 'f.txt');
    const shown = log.greens.find((g) => g.id === watched);
    assert.equal(shown.head, 1);
    const marks = Object.fromEntries(shown.works.map((w) => [w.id, w.mark.state]));
    assert.equal(marks[first.id], 'accepted');
    assert.equal(marks[second.id], 'collided');
  } finally {
    await endAnchor(anchor);
  }
});

// The synchronous calls the parked-beside-the-signal test makes, each parking at once.
const PARKED_CALLS = 20;

// §4.7, D-19: a synchronous call on a client whose async completion signal runs is woken by its reply. Do: on one
// client, open the completion descriptor (which starts the signal: a bridge thread on macOS and Windows) and close the
// copy handed over, have the client spin for none of a reply, and make twenty synchronous status calls. Expect each
// answered within half the reply deadline, and every one of them parked. Before 2026-11-18 the bridge waited where a
// parked call waits and the daemon wakes one waiter per reply, so on macOS a parked call often slept to its deadline
// (16 to 19 of 20, Mac Studio); the bridge now passes such a wake on.
test('a synchronous call beside the async completion signal is woken by its reply', async (t) => {
  const daemon = process.env.SLATES_DAEMON;
  if (!addonPath || !existsSync(addonPath)) {
    t.skip('SLATES_NODE_ADDON is not set to a built .node addon — skipped loudly');
    return;
  }
  if (!daemon || !existsSync(daemon)) {
    t.skip('SLATES_DAEMON is not set to a built `slates` binary — skipped loudly');
    return;
  }
  const slates = require(addonPath);
  const instance = `slates-sdk-node-beside-${process.pid}`;
  const anchor = startAnchor(daemon, instance);
  try {
    const client = await connectWhenReady(slates, instance);
    const bootstrap = spawnSync(daemon, ['--instance', instance, 'bootstrap', 'root'], {
      encoding: 'utf8', timeout: STARTUP_MS,
    });
    assert.equal(bootstrap.status, 0, bootstrap.stderr);
    const volume = client.create('sdk-node-beside', VOLUME_BYTES);
    // The descriptor is a copy the caller owns (a net.Socket would adopt and close it); the signal keeps running. A
    // Windows copy is a socket handle, which fs cannot close, so it is left to the process's end there.
    const completion = client.completionFd();
    if (process.platform !== 'win32') closeSync(completion);
    client.spinFor(0);
    const [parksBefore] = client.parkRatio();
    const deadlineNs = BigInt(REPLY_NS);
    for (let call = 0; call < PARKED_CALLS; call += 1) {
      const asked = process.hrtime.bigint();
      assert.equal(client.status(volume).name, 'sdk-node-beside');
      const tookNs = process.hrtime.bigint() - asked;
      assert.ok(
        tookNs < deadlineNs / 2n,
        `call ${call}: a parked call took ${tookNs} ns of its ${deadlineNs} ns deadline: its wake went elsewhere`,
      );
    }
    const [parksAfter] = client.parkRatio();
    assert.ok(parksAfter - parksBefore >= PARKED_CALLS, `every call parked: ${parksBefore} before, ${parksAfter} after`);
    client.spinFor(null);
  } finally {
    await endAnchor(anchor);
  }
});

// Runs `slates --instance INSTANCE ARGS...` and returns its standard output, throwing on a refusal.
function cli(daemon, instance, ...args) {
  const done = spawnSync(daemon, ['--instance', instance, ...args], { encoding: 'utf8', timeout: STARTUP_MS });
  if (done.status !== 0) throw new Error(`slates ${args.join(' ')}: ${done.stderr}`);
  return done.stdout;
}

// Lists through a fresh client until some entry satisfies `predicate`, within the startup budget: across a daemon's
// restart the first lists may be refused or may not yet show the recovered catalog. The last list and whether it held.
async function listedUntil(slates, instance, predicate) {
  const deadline = Date.now() + STARTUP_MS;
  let listed = [];
  while (Date.now() < deadline) {
    try {
      listed = (await connectWhenReady(slates, instance)).list();
    } catch {
      listed = [];
    }
    if (listed.some(predicate)) return { listed, held: true };
    await sleep(POLL_MS);
  }
  return { listed, held: false };
}

// The two listing fields the CLI's `volume list` shows, by use against a real anchor+daemon: a volume made live-shipped
// lists `liveShipped` true; a volume over a host directory whose directory is removed before the daemon restarts lists
// `notRecovered` as its cause's kind, 'BaseMissing'; and neither is set on a volume it does not describe. Before
// 2026-11-09 the SDK's entries carried neither.
test('a listing says which volumes are live-shipped and which recovery could not rebuild', async (t) => {
  const daemon = process.env.SLATES_DAEMON;
  if (!addonPath || !existsSync(addonPath)) {
    t.skip('SLATES_NODE_ADDON is not set to a built .node addon — skipped loudly');
    return;
  }
  if (!daemon || !existsSync(daemon)) {
    t.skip('SLATES_DAEMON is not set to a built `slates` binary — listing round-trip skipped loudly');
    return;
  }
  const slates = require(addonPath);
  const instance = `slates-sdk-node-list-${process.pid}`;
  const anchor = startAnchor(daemon, instance);
  const scratch = mkdtempSync(join(tmpdir(), 'slates-sdk-node-list-'));
  try {
    const client = await connectWhenReady(slates, instance);
    cli(daemon, instance, 'bootstrap', 'root');
    const live = client.create('sdk-live', VOLUME_BYTES);
    cli(daemon, instance, 'live-ship', live);
    const base = join(scratch, 'base');
    mkdirSync(base);
    const created = cli(daemon, instance, 'volume', 'create', 'sdk-unrecovered', '--bounded', '8MiB', '--base', base);
    const unrecovered = created.split('\n').find((line) => line.startsWith('id: ')).slice('id: '.length).trim();
    cli(daemon, instance, 'volume', 'snapshot', unrecovered);
    rmdirSync(base);
    // The daemon restarts in order under its anchor; its successor holds the record and cannot rebuild it.
    const pid = JSON.parse(cli(daemon, instance, 'status', '--json')).pid;
    process.kill(pid, 'SIGTERM');
    const { listed, held } = await listedUntil(
      slates, instance, (v) => v.id === unrecovered && v.notRecovered != null,
    );
    assert.ok(held, `the restarted daemon lists the unrecovered volume: ${JSON.stringify(listed)}`);
    const entries = Object.fromEntries(listed.map((v) => [v.id, v]));
    assert.equal(entries[unrecovered].notRecovered, 'BaseMissing');
    assert.equal(entries[unrecovered].liveShipped, false);
    assert.equal(entries[unrecovered].overlay, true);
    assert.equal(entries[live].liveShipped, true, `the live-shipped volume says so: ${JSON.stringify(entries[live])}`);
    assert.ok(entries[live].notRecovered == null, `no recovery cause on a live volume: ${JSON.stringify(entries[live])}`);
  } finally {
    await endAnchor(anchor);
    rmSync(scratch, { recursive: true, force: true });
  }
});
