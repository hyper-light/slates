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
import { basename } from 'node:path';
import { spawn, spawnSync } from 'node:child_process';
import { Worker } from 'node:worker_threads';
import { AsyncClient } from '../async.mjs';

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
// How many creates to drive at once, to prove concurrent awaits each get their own reply.
const CONCURRENCY = 8;

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

// Retries connect until the just-spawned daemon answers within the startup budget, so the test does
// not race the anchor's boot — each attempt is one real rendezvous.
async function connectWhenReady(addon, instance) {
  const deadline = Date.now() + STARTUP_MS;
  for (;;) {
    try {
      return await AsyncClient.connect(addon, instance, REPLY_NS, RECONNECT_NS);
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
    'beginSpinCreateGreen', 'pollCreateGreen', 'beginSpinCreateWork', 'pollCreateWork',
    'beginSpinEdit', 'pollEdit', 'beginSpinSubmit', 'pollSubmit',
    'beginSpinVersions', 'pollVersions', 'beginSpinChangedSince', 'pollChangedSince',
    'beginSpinRebase', 'pollRebase', 'beginSpinUnlink', 'beginSpinRename', 'beginSpinMkdir',
    'beginSpinRmdir', 'beginSpinChmod', 'beginSpinSymlink', 'beginSpinLink',
    'beginSpinSetXattr', 'beginSpinRemoveXattr', 'pollDeclare',
    'beginSpinLand', 'pollLand', 'completionFd', 'arm', 'disarm', 'takeReady',
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
    const bootstrap = spawnSync(daemon, ['--instance', instance, 'bootstrap', 'root'], {
      encoding: 'utf8', timeout: STARTUP_MS,
    });
    assert.equal(bootstrap.status, 0, bootstrap.stderr);
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
    // The teardown runs in slices after the reply (§4.4; measured 54–302 µs after it, 2026-09-14),
    // so the list is polled within the startup budget, as the Rust client test does.
    const goneBy = Date.now() + STARTUP_MS;
    while ((await client.list()).some((v) => v.id === volume)) {
      assert.ok(Date.now() < goneBy, 'the destroyed volume is gone from the list');
      await sleep(POLL_MS);
    }

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

// AUD-29-19 / AUD-29-20: the bound on any one late timer callback while calls wait, restart or fail — a
// tenth of the reply deadline, the longest pause the reconnect pacing itself allows. A call that blocked
// the loop on a reply, a ring slot or a reconnect would exceed it.
const TICK_BOUND_MS = REPLY_NS / 10 / 1e6;
// The ticker's own period: a tenth of its bound, so a held loop shows as lateness well past one period.
const TICK_MS = TICK_BOUND_MS / 10;

// An independent event-loop ticker: records how late its callbacks run.
function startTicker() {
  const ticker = { worst: 0, timer: null };
  let due = Date.now() + TICK_MS;
  ticker.timer = setInterval(() => {
    const now = Date.now();
    ticker.worst = Math.max(ticker.worst, now - due);
    due = now + TICK_MS;
  }, TICK_MS);
  return ticker;
}

// The sampler's sleep: a millisecond, far below the bound it qualifies.
const NOISE_SAMPLE_MS = 1;

// The machine's own scheduling noise while the test runs: a worker thread (its own isolate, so nothing the
// main loop does can hold it) that sleeps NOISE_SAMPLE_MS at a time and records how much longer than asked
// each sleep took — time the OS kept it off a core. The main loop is kept off as long by the same cause, so
// its lateness is judged against the bound plus this noise (the pattern of the vfs bench's destroy_rows); a
// call that blocked the main loop leaves the worker's sleeps on time, and still fails (CI 2026-10-01: a
// 106 ms lateness against a 100 ms bound on the macOS runner, unattributed).
function startNoise() {
  const flag = new Int32Array(new SharedArrayBuffer(4));
  const worst = new Float64Array(new SharedArrayBuffer(8));
  const worker = new Worker(
    `const { workerData } = require('node:worker_threads');
     const flag = new Int32Array(workerData.flag);
     const worst = new Float64Array(workerData.worst);
     while (Atomics.load(flag, 0) === 0) {
       const asked = performance.now();
       Atomics.wait(flag, 0, 0, workerData.sample);
       worst[0] = Math.max(worst[0], performance.now() - asked - workerData.sample);
     }`,
    { eval: true, workerData: { flag: flag.buffer, worst: worst.buffer, sample: NOISE_SAMPLE_MS } },
  );
  const exited = new Promise((resolve) => worker.once('exit', resolve));
  // Idempotent: the test's `finally` stops it too, so a failed assertion never leaves the worker running.
  return {
    async stop() {
      Atomics.store(flag, 0, 1);
      Atomics.notify(flag, 0);
      await exited;
      return worst[0];
    },
  };
}

// The spawner's source, run on a worker thread: for each request it spawns the program on the worker's own loop and
// answers the exit status, the stdout, and how long `spawn()` took to return, which is how long the child took to
// exec. A program that cannot be run answers status -1, as `runAsync` always has; the main side takes the first
// answer for each request.
const SPAWNER_SOURCE = `
  const { parentPort } = require('node:worker_threads');
  const { spawn } = require('node:child_process');
  parentPort.on('message', ({ id, program, args }) => {
    const began = performance.now();
    const child = spawn(program, args, { stdio: ['ignore', 'pipe', 'ignore'] });
    const execWait = performance.now() - began;
    let stdout = '';
    child.stdout.on('data', (chunk) => {
      stdout += chunk;
    });
    child.on('close', (status) => parentPort.postMessage({ id, status, stdout, execWait }));
    child.on('error', () => parentPort.postMessage({ id, status: -1, stdout, execWait }));
  });`;

// The harness's process spawner, made on first use and stopped by the scenario's `finally`.
let spawner = null;

// One worker thread that runs the harness's process spawns, the requests it has yet to answer, and the slowest exec
// wait it has seen for each program, in milliseconds. A worker that fails rejects every request it holds, so a
// broken spawner fails the test instead of reading as a process that is gone.
function startSpawner() {
  const worker = new Worker(SPAWNER_SOURCE, { eval: true });
  const pending = new Map();
  const slowest = new Map();
  let next = 0;
  worker.on('message', ({ id, status, stdout, execWait }) => {
    const waiting = pending.get(id);
    if (!waiting) return;
    pending.delete(id);
    slowest.set(waiting.name, Math.max(slowest.get(waiting.name) ?? 0, execWait));
    waiting.resolve({ status, stdout });
  });
  const failAll = (error) => {
    for (const waiting of pending.values()) waiting.reject(error);
    pending.clear();
  };
  worker.on('error', failAll);
  worker.on('exit', (code) => failAll(new Error(`the spawner thread exited (${code}) with requests unanswered`)));
  return {
    run(program, args) {
      return new Promise((resolve, reject) => {
        const id = next;
        next += 1;
        pending.set(id, { name: basename(program), resolve, reject });
        worker.postMessage({ id, program, args });
      });
    },
    slowest,
    stop: () => worker.terminate(),
  };
}

// Runs `program` on the spawner's thread, so the loop stays free: its exit status and stdout. The ticker must measure
// the SDK, not this harness. `child_process.spawn` returns only once the child has exec'd (libuv waits for the exec on
// the calling thread), so until 2026-10-22 this helper held the loop for as long as each child took to exec. Measured
// on Node v24.22.1 with each exec delayed 150 ms, as a cold disk would delay it: `spawn()` returned after 153–159 ms
// and a loop ticker was as late
// (docs/bugs/2026-10-22-the-async-sdk-tests-own-process-spawns-held-the-loop-it-measures.md). On the spawner's
// thread the same wait holds only that thread.
function runAsync(program, args) {
  spawner ??= startSpawner();
  return spawner.run(program, args);
}

// The pids of the daemon an anchor for `instance` supervises (scoped by instance, never a bare name).
async function daemonPids(daemon, instance) {
  const found = await runAsync('pgrep', ['-f', `${daemon} --instance ${instance} daemon`]);
  return found.stdout.split('\n').filter(Boolean).map(Number);
}

async function waitFor(condition, what) {
  const deadline = Date.now() + STARTUP_MS;
  while (!(await condition())) {
    assert.ok(Date.now() < deadline, what);
    await sleep(POLL_MS);
  }
}

// The process's state letter, or null when it is gone; `ps` reads it on Linux and macOS alike. A `ps`
// that could not run at all is thrown, never reported as "gone": `runAsync` turns a spawn failure into
// status -1, and reading that as gone would make every wait below pass at once and silently restore the
// race they exist to close.
async function stateOf(pid) {
  const { status, stdout } = await runAsync('ps', ['-o', 'state=', '-p', String(pid)]);
  if (status === -1) throw new Error('`ps` could not be run, so no process state can be read');
  const letter = stdout.trim();
  return status === 0 && letter ? letter[0] : null;
}

// Signals, tolerating only the process having already exited between the scan and the signal -- which is
// the state the caller wanted anyway. Anything else is a real failure and is rethrown.
function signalIfAlive(pid, signal) {
  try {
    process.kill(pid, signal);
  } catch (error) {
    if (error.code !== 'ESRCH') throw error;
  }
}

// SIGSTOP every pid and return only once each has really stopped (or gone). `kill` queues the signal and
// returns; the target stops when it is next scheduled, and a daemon that has been signalled but not yet
// stopped is still a live daemon and still answers.
async function stopAndWait(pids, what) {
  for (const pid of pids) signalIfAlive(pid, 'SIGSTOP');
  await waitFor(async () => {
    for (const pid of pids) {
      const state = await stateOf(pid);
      if (state !== 'T' && state !== null) return false;
    }
    return true;
  }, what);
}

// Stops the anchor's whole process group and returns only once every member has really stopped. Measured
// on Linux 2026-10-21 against the Python twin: with the call made straight after the group stop, the
// daemon answered it 5 times in 100, reading state `R` in the instant after the stop. The anchor is
// confirmed stopped first, because only a running anchor can fork a replacement the group signal has
// already swept past; once it is stopped the set of daemons is closed.
async function stopGroupAndWait(daemon, instance, anchorPid) {
  signalIfAlive(-anchorPid, 'SIGSTOP');
  await stopAndWait([anchorPid], 'the anchor stopped');
  await waitFor(async () => {
    const running = [];
    for (const pid of await daemonPids(daemon, instance)) {
      const state = await stateOf(pid);
      if (state !== 'T' && state !== null) running.push(pid);
    }
    for (const pid of running) signalIfAlive(pid, 'SIGSTOP');
    return running.length === 0;
  }, 'every daemon the anchor supervises stopped');
}

test('every async call ends across restart, silence, reader loss and death', async (t) => {
  const daemon = process.env.SLATES_DAEMON;
  if (!addonPath || !existsSync(addonPath) || !daemon || !existsSync(daemon)) {
    t.skip('SLATES_NODE_ADDON or SLATES_DAEMON is not set — skipped loudly');
    return;
  }
  const slates = require(addonPath);
  const instance = `slates-node-ends-${process.pid}`;
  const anchor = spawn(daemon, ['--instance', instance, 'anchor', '--quick', '--shards', '1'], {
    stdio: 'ignore',
    detached: true,
  });
  const noise = startNoise();
  const ticker = startTicker();
  try {
    const client = await connectWhenReady(slates, instance);
    const bootstrap = await runAsync(daemon, ['--instance', instance, 'bootstrap', 'root']);
    assert.equal(bootstrap.status, 0, 'the consensus group bootstraps');
    const limit = client._c.outstandingLimit();

    // (1) Overflow, then a restart under the calls.
    const first = await daemonPids(daemon, instance);
    assert.ok(first.length > 0, 'the daemon is running');
    // Stopped first, so the calls stay outstanding (a live daemon answers within the fast-path spin) —
    // and stopped means stopped, or the daemon drains them and nothing overflows.
    await stopAndWait(first, 'the daemon stopped');
    const calls = Array.from({ length: limit * 3 }, () => client.list());
    for (const pid of first) process.kill(pid, 'SIGKILL');
    const settled = await Promise.allSettled(calls);
    const refused = settled.filter((r) => r.status === 'rejected' && /TooManyOutstanding/.test(r.reason.message));
    const answered = settled.filter((r) => r.status === 'fulfilled' && Array.isArray(r.value));
    assert.ok(refused.length > 0, 'the overflow is refused at once');
    assert.equal(answered.length + refused.length, settled.length,
      `every admitted call is answered after the restart: ${JSON.stringify(settled.filter((r) => r.status === 'rejected' && !/TooManyOutstanding/.test(r.reason.message)).map((r) => r.reason.message))}`);
    await waitFor(async () => (await daemonPids(daemon, instance)).length > 0, 'the anchor restarted the daemon');

    // (2) A live but silent daemon: the call ends Stalled at its reply deadline. The anchor's whole process
    // group (it leads it, `detached`, and every daemon it spawns is in it) is stopped in one signal (a negative
    // pid), so its supervision cannot replace the silent daemon and no child it forked can escape the stop.
    // The helper waits until every member has really stopped, because the signal only queues and a daemon
    // still running answers the call this requires to stall.
    await stopGroupAndWait(daemon, instance, anchor.pid);
    await assert.rejects(client.list(), /Stalled/);

    // (2b) A cancelled call is released at once: it rejects AbortError and nothing is left waiting.
    const cancelled = client.list();
    assert.ok(client.cancel(cancelled), 'a waiting call is cancelled');
    await assert.rejects(cancelled, { name: 'AbortError' });
    assert.equal(client._pending.size, 0, 'nothing is left waiting');
    assert.equal(client.cancel(cancelled), false, 'an ended call is not cancelled again');

    // (3) The completion reader lost under a waiting call.
    const waiting = client.list();
    await sleep(POLL_MS);
    client._sock.destroy();
    await assert.rejects(waiting, /completion fd|CompletionLost/);
    process.kill(-anchor.pid, 'SIGCONT');
    await assert.rejects(client.list(), /completion fd|CompletionLost/, 'a lost reader refuses later calls');

    // (4) Death: a fresh client, then the anchor and its daemon killed for good.
    const fresh = await connectWhenReady(slates, instance);
    process.kill(-anchor.pid, 'SIGKILL');
    await waitFor(async () => (await daemonPids(daemon, instance)).length === 0, 'the daemon is gone');
    const doomed = await Promise.allSettled(Array.from({ length: limit }, () => fresh.list()));
    for (const result of doomed) {
      assert.equal(result.status, 'rejected');
      assert.match(result.reason.message, /DaemonGone/);
    }
    const scheduling = await noise.stop();
    const waits = [...(spawner?.slowest ?? new Map())]
      .sort(([a], [b]) => a.localeCompare(b))
      .map(([name, ms]) => `${name} ${ms.toFixed(1)} ms`);
    console.error(`the harness's own process spawns, each off the loop: slowest exec wait ${waits.join(', ')}`);
    assert.ok(
      ticker.worst < TICK_BOUND_MS + scheduling,
      `the loop was never held: worst lateness ${ticker.worst} ms, bound ${TICK_BOUND_MS} ms plus the machine's own scheduling noise ${scheduling.toFixed(1)} ms`,
    );
  } finally {
    await noise.stop();
    clearInterval(ticker.timer);
    try {
      process.kill(-anchor.pid, 'SIGKILL');
    } catch (_) {
      // already gone
    }
    await spawner?.stop();
    spawner = null;
  }
});
