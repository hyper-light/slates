"""By-use tests for the slates Python SDK's ASYNC surface (R6, D-19): the AsyncClient's verbs are
awaitable on a real asyncio loop and resolve by the completion fd's readiness — the daemon signals
the fd, `loop.add_reader` fires, and the future resolves — never by blocking the loop. The sync
`Client` is the thin facade over the same daemon; this is the primary async form. Written with the
stdlib `unittest`/`asyncio` so it runs with a bare `python3 -m unittest` — no third-party runner. The
daemon round-trip is gated: it runs only when a built `slates` daemon binary is on PATH or named by
SLATES_DAEMON, and otherwise skips loudly (never fails on a machine without the binary).

Run: `maturin develop && SLATES_DAEMON=target/debug/slates python3 -m unittest discover crates/sdk-python/tests`."""

import asyncio
import os
import selectors
import signal
import subprocess
import sys
import threading
import time
import traceback
import unittest

import slates

# The async verbs bound: the volume lifecycle, the whole merge workflow, and the namespace operations.
ASYNC_VERBS = {
    "connect", "create", "snapshot", "status", "list", "resize", "destroy", "client_id",
    "create_green", "create_work", "edit", "submit", "versions", "changed_since", "rebase",
    "unlink", "rename", "mkdir", "rmdir", "chmod", "symlink", "link", "set_xattr", "remove_xattr",
    "land", "reconnects", "outstanding_limit",
}
# How much slower this lane is than a dev machine, the same knob the Rust client tests read
# (SLATES_TEST_RESTART_SLOWDOWN; the gates lane sets 10 and the TSan lane 25). An integer >= 1,
# default 1, so a normal local run is unchanged.
SLOWDOWN = max(1, int(os.environ.get("SLATES_TEST_RESTART_SLOWDOWN") or 1))
# Test deadlines in nanoseconds; a production caller derives these from the machine's budgets.
# Scaled, because they are budgets for work a slow machine does slowly. Measured 2026-10-20: with ten
# busy spinners on an 18-core machine (load 32), phase 1's calls came back
# `Stalled { after_ns: 1000000000 }` instead of being answered across the restart — the restarted
# daemon simply had not answered inside a flat one second. One failure in five runs; none in six at
# four spinners. That is the same defect as the Rust side's flat START_WAIT, in a test that read no
# knob at all.
REPLY_NS = 1_000_000_000 * SLOWDOWN
RECONNECT_NS = 2_000_000_000 * SLOWDOWN
# How long to wait for a freshly spawned anchor+daemon to answer, and the pause between polls.
STARTUP_SECS = 20.0 * SLOWDOWN
POLL_SECS = 0.02
# A small bounded RAM volume for the round-trip.
VOLUME_BYTES = 8 * 1024 * 1024
# How many creates to drive at once, to prove concurrent awaits each get their own reply.
CONCURRENCY = 8


def _daemon_binary():
    """The path to a built `slates` daemon binary, or None to skip the round-trip loudly."""
    named = os.environ.get("SLATES_DAEMON")
    if named and os.path.exists(named):
        return named
    for candidate in ("target/release/slates", "target/debug/slates"):
        if os.path.exists(candidate):
            return candidate
    return None


async def _connect_when_ready(instance):
    """Retries connect until the just-spawned daemon answers within the startup budget, awaiting
    between tries so the loop stays live — each attempt is one real rendezvous."""
    deadline = time.monotonic() + STARTUP_SECS
    while True:
        try:
            return await slates.AsyncClient.connect(instance, REPLY_NS, RECONNECT_NS)
        except slates.SlatesError:
            if time.monotonic() >= deadline:
                raise
            await asyncio.sleep(POLL_SECS)


class SlatesAsyncSurface(unittest.TestCase):
    def test_module_exposes_the_async_client(self):
        """The extension exposes AsyncClient with its awaitable verbs, alongside the sync Client."""
        self.assertTrue(hasattr(slates, "AsyncClient"))
        exposed = {name for name in dir(slates.AsyncClient) if not name.startswith("__")}
        self.assertTrue(ASYNC_VERBS <= exposed, exposed)


class SlatesAsyncRoundTrip(unittest.TestCase):
    @unittest.skipIf(
        _daemon_binary() is None,
        "no built `slates` daemon binary (set SLATES_DAEMON or build the cli) — async round-trip skipped loudly",
    )
    def test_async_lifecycle_over_a_live_daemon(self):
        """Spawns a real anchor+daemon, connects the async client, and awaits create → snapshot →
        status on a real asyncio loop, then drives many creates concurrently — each await resolved by
        the completion fd, each reply routed to its own request (R5, R6). The anchor is killed and
        reaped in `finally`, so a failed assertion leaves no daemon behind."""
        asyncio.run(self._lifecycle())

    async def _lifecycle(self):
        binary = _daemon_binary()
        instance = f"slates-async-{os.getpid()}"
        # `start_new_session` makes the anchor its own process-group leader, so teardown kills the whole
        # group — the anchor and the daemon it supervises — at once.
        anchor = subprocess.Popen(
            [binary, "--instance", instance, "anchor", "--quick", "--shards", "1"],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            start_new_session=True,
        )
        try:
            client = await _connect_when_ready(instance)
            subprocess.run([binary, "--instance", instance, "bootstrap", "root"],
                           check=True, timeout=STARTUP_SECS)
            self.assertIsInstance(client.client_id(), int)

            # await create → a 32-hex volume id.
            volume = await client.create("async-roundtrip", VOLUME_BYTES)
            self.assertEqual(len(volume), 32)

            # await snapshot → a monotonic sequence.
            snapshot = await client.snapshot(volume)
            self.assertIsInstance(snapshot, int)

            # await status → the volume's real fields, the same shape the sync verb returns.
            status = await client.status(volume)
            self.assertEqual(status["id"], volume)
            self.assertEqual(status["name"], "async-roundtrip")
            self.assertEqual(status["snapshots"], 1)
            self.assertTrue(status["placed"])
            self.assertEqual(status["host_epoch"], 1)

            # await list → the volume appears with its fields (a scratch volume is not an overlay).
            listed = await client.list()
            entry = next((v for v in listed if v["id"] == volume), None)
            self.assertIsNotNone(entry, "the created volume appears in the async list")
            self.assertEqual(entry["name"], "async-roundtrip")
            self.assertFalse(entry["overlay"])

            # await resize → a larger bound; a unit verb resolves to None.
            self.assertIsNone(await client.resize(volume, 2 * VOLUME_BYTES))

            # await destroy → the volume is gone from a later list; resolves to None. The teardown
            # runs in slices after the reply (§4.4; measured 54–302 µs after it, 2026-09-14), so the
            # list is polled within the startup budget, as the Rust client test does.
            self.assertIsNone(await client.destroy(volume))
            gone_by = time.monotonic() + STARTUP_SECS
            while any(v["id"] == volume for v in await client.list()):
                self.assertLess(
                    time.monotonic(), gone_by, "the destroyed volume is gone from the list"
                )
                await asyncio.sleep(POLL_SECS)

            # Concurrency: many creates awaited at once, each its own reply — the multiplexing an async
            # client relies on (one reader serves them all, replies matched to requests by id).
            names = [f"async-concurrent-{i}" for i in range(CONCURRENCY)]
            ids = await asyncio.gather(*(client.create(name, VOLUME_BYTES) for name in names))
            self.assertEqual(len(ids), CONCURRENCY)
            self.assertEqual(
                len(set(ids)), CONCURRENCY, "each concurrent create got a distinct volume id"
            )

            # Merge workflow (§4.16): a green, a work over it, a content edit, a clean submit — the
            # async form of the sync suite's merge loop, awaited on the same loop.
            green = await client.create_green("g-async")
            self.assertEqual(len(green), 32)
            work = await client.create_work(green, "w-async")
            self.assertEqual(len(work["id"]), 32)
            self.assertIsInstance(work["base"], int)
            # edit resolves to None (a unit verb); it creates the file on write.
            self.assertIsNone(await client.edit(work["id"], "/notes.txt", 0, 0, b"hello async merge"))
            outcome = await client.submit(work["id"])
            self.assertTrue(outcome["ok"], f"the async submit landed cleanly: {outcome}")
            self.assertEqual(outcome["conflicts"], [])
            self.assertIsInstance(outcome["version"], int)

            # await versions/changed_since → the green advanced past the work's base and lists the edit.
            self.assertGreater(await client.versions(green), work["base"])
            changed = await client.changed_since(green, work["base"])
            self.assertTrue(any("notes.txt" in path for path in changed), changed)
            # await rebase → a fresh work off the advanced head rebases cleanly (nothing to conflict).
            fresh = await client.create_work(green, "w2-async")
            rebased = await client.rebase(fresh["id"])
            self.assertTrue(rebased["ok"], f"a fresh async work rebases cleanly: {rebased}")

            # namespace operations (§4.16): build a tree on a work volume with every op — each awaited
            # and resolving to None — then submit; the async form of the sync suite's namespace loop.
            ns_work = await client.create_work(green, "w-ns-async")
            ns_id = ns_work["id"]
            self.assertIsNone(await client.edit(ns_id, "/keep.txt", 0, 0, b"keep"))
            self.assertIsNone(await client.edit(ns_id, "/gone.txt", 0, 0, b"gone"))
            self.assertIsNone(await client.unlink(ns_id, "/gone.txt"))
            self.assertIsNone(await client.mkdir(ns_id, "/d"))
            self.assertIsNone(await client.rename(ns_id, "/keep.txt", "/d/keep.txt"))
            self.assertIsNone(await client.chmod(ns_id, "/d/keep.txt", 0o600))
            self.assertIsNone(await client.symlink(ns_id, "/d/link", "keep.txt"))
            self.assertIsNone(await client.link(ns_id, "/d/hard.txt", "/d/keep.txt"))
            self.assertIsNone(await client.set_xattr(ns_id, "/d/keep.txt", "user.slates", b"1"))
            self.assertIsNone(await client.remove_xattr(ns_id, "/d/keep.txt", "user.slates"))
            ns_outcome = await client.submit(ns_id)
            self.assertTrue(ns_outcome["ok"], f"the async namespace ops submitted cleanly: {ns_outcome}")
        finally:
            # Kill the whole process group so the supervised daemon goes with the anchor at once.
            try:
                os.killpg(os.getpgid(anchor.pid), signal.SIGKILL)
            except ProcessLookupError:
                pass
            anchor.wait()


# Derived: the longest the loop may go without running a ready callback — a tenth of the reply deadline,
# the longest pause the reconnect pacing itself allows. A call that blocked the loop on a reply, a ring
# slot or a reconnect would exceed it.
# Deliberately *not* scaled, and derived from the unscaled reply budget rather than REPLY_NS. How
# responsive an event loop must be is not a function of how slow the lane is: scaling this with
# SLOWDOWN would take the TSan lane's bound to 2.5 s and mask the very hold it exists to catch.
TICK_BOUND_SECS = 1_000_000_000 / 10 / 1e9
# The ticker's own period: a tenth of its bound, so a held loop shows as lateness well past one period.
TICK_SECS = TICK_BOUND_SECS / 10
# Shape: the noise control's tick — the ticker's own period, so both measure the same scheduling grain.
NOISE_SAMPLE_SECS = TICK_SECS

# The noise control, run in its own interpreter: an event loop that ticks NOISE_SAMPLE_SECS at a time beside a
# sibling thread that waits as the Ticker's sibling waits, until its stdin closes; then it prints the loop's worst
# lateness. It is the Ticker without the SDK and without this harness: the same interpreter, the same two threads,
# the same waits through the same primitive.
_NOISE_SAMPLER = (
    "import asyncio, selectors, sys, threading, time\n"
    "tick = float(sys.argv[1])\n"
    "stop = threading.Event()\n"
    "threading.Thread(target=lambda: (sys.stdin.read(), stop.set()), daemon=True).start()\n"
    "def sibling():\n"
    "    with selectors.DefaultSelector() as selector:\n"
    "        while not stop.is_set():\n"
    "            selector.select(tick)\n"
    "threading.Thread(target=sibling, daemon=True).start()\n"
    "async def control():\n"
    "    worst = 0.0\n"
    "    while not stop.is_set():\n"
    "        due = time.monotonic() + tick\n"
    "        await asyncio.sleep(tick)\n"
    "        worst = max(worst, time.monotonic() - due)\n"
    "    return worst\n"
    "print(asyncio.run(control()))\n"
)


class Noise:
    """The machine's own scheduling noise while the test runs: a control in a separate process, an event loop that
    ticks as the Ticker does, whose worst lateness is time the OS kept a loop like this one waiting. A separate
    process, not a thread: a native call that wrongly held the GIL would delay a thread here as much as the loop and
    mask the very hold the bound exists to catch. A charged hold is judged against the bound plus this noise — the
    pattern of the Rust driver test and the Node SDK test (CI 2026-10-01: a macOS runner's loop was 0.186 s late under
    load).

    The control waits through the loop's own primitive, because on macOS the primitive decides the figure. Measured
    2026-10-26 on this Mac (18 cores, load 19 to 72, Python 3.9 and 3.13, Rust std alike), from a process that a
    launchd job started, as kiln's agents and GitHub's macOS runner both start one: a 10 ms `time.sleep`, `select`,
    `nanosleep`, `park_timeout` or `Condvar` wait came back 76 ms late at the median, at most 116 ms; the delay is
    about eight times the interval up to about 150 ms (6 ms late at 1 ms asked, 148 ms at 100 ms and at 1 s). The
    event loop's own wait, a kevent timeout, came back 1.3 ms late at the median and at most 11 ms. Until 2026-10-26
    this sampler slept with `time.sleep`, so its figure was mostly the other primitive's delay, and the loop's own tail
    was compared against it: runs 40227172048 and 40239373189 failed with the loop 0.268 s and 0.286 s late against
    noise of 0.134 s and 0.150 s."""

    def __init__(self):
        self._child = subprocess.Popen(
            [sys.executable, "-c", _NOISE_SAMPLER, str(NOISE_SAMPLE_SECS)],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            text=True,
        )
        self._worst = None

    def stop(self):
        """Ends the control (once; later calls return the same figure) and returns its worst lateness."""
        if self._worst is None:
            out, _ = self._child.communicate(input="")
            self._worst = float(out.strip() or "0")
        return self._worst


# Shape: how many waits of each kind `_wait_delays` takes the median of — five keeps the line under a second where a
# `time.sleep` comes back 76 ms late.
WAIT_DELAY_SAMPLES = 5


def _wait_delays():
    """How late this process's waits of one tick come back, the median of WAIT_DELAY_SAMPLES each: a `time.sleep` (a
    nanosleep on Python 3.11 and later, a select before) and a selector wait (the loop's own primitive). Printed after
    the measurement beside the ticker's figures, so every run records the timer policy its process ran under
    (`Noise`)."""

    def median_late(wait):
        lates = []
        for _ in range(WAIT_DELAY_SAMPLES):
            due = time.monotonic() + TICK_SECS
            wait()
            lates.append(time.monotonic() - due)
        return sorted(lates)[len(lates) // 2]

    with selectors.DefaultSelector() as selector:
        selected = median_late(lambda: selector.select(TICK_SECS))
    slept = median_late(lambda: time.sleep(TICK_SECS))
    return (
        f"this process's {TICK_SECS * 1e3:.0f} ms waits came back late by, at the median of {WAIT_DELAY_SAMPLES}: "
        f"time.sleep {slept * 1e3:.1f} ms, the selector {selected * 1e3:.1f} ms"
    )


def _idle_in_selector(frame):
    """Whether `frame`, the loop thread's innermost Python frame, is the event loop's own idle wait: the selector's
    `select`, which asyncio's `_run_once` calls once nothing is ready, and in which the thread waits in the kernel
    (kevent on macOS, epoll on Linux) with the GIL released. A thread anywhere else is running a callback or is
    blocked in a call."""
    return frame is not None and frame.f_code is selectors.DefaultSelector.select.__code__


class _Gap:
    """What the sibling saw during one tick: its own worst lateness and, while the tick was more than a period
    overdue, each sample of the loop thread's frame (`Ticker._observe`). The ticker reads it once the tick returns
    (`Ticker._charge`)."""

    __slots__ = ("heartbeat", "samples", "last_idle", "busy_stack", "sibling_late")

    def __init__(self, heartbeat):
        # When the ticker last ran: the tick in progress is due one period after.
        self.heartbeat = heartbeat
        self.samples = 0
        # The last time the loop thread was seen idle in its selector while the tick was overdue.
        self.last_idle = None
        # The loop thread's stack the first time it was seen anywhere else while the tick was overdue.
        self.busy_stack = None
        self.sibling_late = 0.0


class Ticker:
    """An independent task on the loop: sleeps one period at a time and charges each tick with how long the loop was
    held, by what a sibling thread saw while the tick was overdue; and records in which phase of the scenario the
    worst charge fell.

    The phase is carried because this test has failed on CI without one and could not be acted on: runs
    39538566246 and 39539155335 reported 0.298 s and 0.243 s of lateness against a 0.1 s bound, with the
    separate-process noise sampler seeing 1 ms and 0 ms over the same window — so the loop really was held,
    and nothing said by what. The scenario kills a daemon under outstanding calls, stops a process group,
    cancels a call and kills an anchor; those are very different suspects. Whoever sees the next failure
    should not have to guess which.

    Late is not held. A tick is late when the loop has not run it by its due time, and a loop idle in its selector,
    with nothing ready, waits in the kernel, which may wake it late. So while a tick is more than a period overdue,
    the sibling samples the loop thread's frame each time it wakes, and the tick is charged:
    - from the last moment the sibling saw the loop idle in its selector, when it saw it there at all, the sibling
      itself was never late past the bound across the tick (so neither the GIL nor the whole process was held that
      long), and the process burned less than one period of CPU across it;
    - otherwise with its whole lateness.
    Every hold the test exists to catch is charged whole. A call on the loop that blocks with the GIL released is
    seen busy, in the call: the harness's process spawns held the loop that way until 2026-10-22. A busy hold of the
    GIL burns CPU and keeps the sibling off. A call that blocks holding the GIL, on the loop's thread or any other,
    keeps the sibling off for as long. Added 2026-10-26, after runs 40227172048 and 40239373189 failed with the loop
    0.268 s and 0.286 s late, the process at 0.002 s and 0.005 s of CPU, and, in the second, the sibling awake 0.128 s
    into the hold and finding the loop in `kqueue.control`: the event loop's own idle wait, woken late by the kernel,
    which the bound charged as a hold."""

    def __init__(self):
        self.phase = "connecting"
        # The worst charge, the figures of the tick that had it, and the stack the sibling first saw the loop busy at.
        self.held = 0.0
        self.held_phase = self.phase
        self.held_late = 0.0
        self.held_cpu = 0.0
        self.held_sibling = 0.0
        self.held_stack = None
        # The worst lateness however it was charged, and how many ticks were late past the bound yet not charged
        # (the loop idle in its selector, the sibling free, the process quiet): the kernel's lateness, on record.
        self.late = 0.0
        self.late_phase = self.phase
        self.late_held = 0.0
        self.uncharged = 0
        self._loop_ident = threading.get_ident()
        self._gap = _Gap(time.monotonic())
        self._stop = threading.Event()
        # A sibling probe *inside this process*, on a thread of its own. The separate-process control
        # answers "was the machine scheduling a loop like this one", which is not the same question: this
        # one answers "could any Python code in *this* process run", and it can look at the loop while the
        # loop is late. The test's older comment rejected a thread because a native call holding the GIL
        # would delay it as much as the loop — which is precisely the signal, once there is a process
        # control beside it to rule out starvation.
        self._thread = threading.Thread(target=self._sibling, daemon=True)
        self._thread.start()
        self.task = asyncio.get_running_loop().create_task(self._run())

    def stop(self):
        """Ends the measurement: the ticker's task and the sibling together, so the scenario's own teardown is
        neither measured nor charged."""
        self.task.cancel()
        self._stop.set()

    def _sibling(self):
        # It waits as the loop waits, through the selector. A `time.sleep` is a nanosleep, which a launchd job's
        # policy delays eight times over (`Noise`): a sibling that slept so woke every 86 ms instead of every 10,
        # looked at the loop that coarsely, and read the delay as its own lateness.
        with selectors.DefaultSelector() as selector:
            while not self._stop.is_set():
                due = time.monotonic() + TICK_SECS
                selector.select(TICK_SECS)
                now = time.monotonic()
                self._observe(now, now - due)

    def _observe(self, now, late):
        """One sibling wake: its own lateness, and, while the tick in progress is more than a period overdue, a
        sample of where the loop thread is. A hold that releases the GIL is seen while it lasts. A hold that keeps
        the GIL keeps the sibling off until it ends and is seen at once after: the interpreter hands the GIL to a
        thread that has waited a switch interval at the holder's next instruction, so the loop's frame is still the
        holding call's, and the sibling's own lateness records the hold.

        Measured and rejected, 2026-10-23: `faulthandler.dump_traceback_later`, armed before each tick's sleep
        and cancelled after. Every arm starts a new watchdog thread and every cancel waits for the old one to
        exit while holding the GIL (CPython 3.11 `Modules/faulthandler.c`, lines 657, 771 and 787), and both
        fell inside the interval the ticker measures. At load average 14 to 17 on macOS the pair cost a 1.1 ms
        median, 19.7 ms at p99, 64.9 ms at p999 and 74.7 ms at worst over 2000 ticks, and the test failed 2 of
        5 there with no hold dumped."""
        gap = self._gap
        gap.sibling_late = max(gap.sibling_late, late)
        if now - gap.heartbeat <= 2 * TICK_SECS or self._stop.is_set():
            return
        frame = sys._current_frames().get(self._loop_ident)
        gap.samples += 1
        if _idle_in_selector(frame):
            gap.last_idle = now
        elif gap.busy_stack is None:
            gap.busy_stack = (
                "".join(traceback.format_stack(frame)) if frame is not None else "  (no Python frame)\n"
            )

    async def _run(self):
        while True:
            await self._tick()

    async def _tick(self):
        """One period: sleeps, hands the sibling a fresh record, and charges the tick that ended."""
        due = time.monotonic() + TICK_SECS
        cpu = time.process_time()
        await asyncio.sleep(TICK_SECS)
        now = time.monotonic()
        gap, self._gap = self._gap, _Gap(now)
        self._charge(gap, now - due, now, time.process_time() - cpu)

    def _charge(self, gap, late, now, cpu):
        """Charges one tick by the rule in the class's docstring, keeps the worst, and prints any charge past the
        bound with the stack the sibling saw."""
        quiet = gap.last_idle is not None and gap.sibling_late <= TICK_BOUND_SECS and cpu < TICK_SECS
        held = now - gap.last_idle if quiet else late
        if held > self.held:
            self.held, self.held_phase = held, self.phase
            self.held_late, self.held_cpu, self.held_sibling = late, cpu, gap.sibling_late
            self.held_stack = gap.busy_stack
        if late > self.late:
            self.late, self.late_phase, self.late_held = late, self.phase, held
        if late > TICK_BOUND_SECS and quiet:
            self.uncharged += 1
        if held > TICK_BOUND_SECS:
            stack = gap.busy_stack or "  (never seen busy)\n"
            print(
                f"ticker: the loop was held {held:.3f} s in phase {self.phase!r} (the tick {late:.3f} s late, "
                f"{cpu:.3f} s of CPU, the sibling {gap.sibling_late:.3f} s late, {gap.samples} samples of the "
                f"loop); first seen busy at:\n{stack}",
                file=sys.stderr,
            )

    def summary(self, scheduling):
        """The run's figures, printed whether it passes or fails, so a passing run's margin is on record too."""
        return (
            f"the ticker: worst charge {self.held:.3f} s in phase {self.held_phase!r} (that tick "
            f"{self.held_late:.3f} s late, {self.held_cpu:.3f} s of CPU, the sibling {self.held_sibling:.3f} s "
            f"late); worst lateness {self.late:.3f} s in phase {self.late_phase!r}, charged "
            f"{self.late_held:.3f} s; {self.uncharged} ticks late past the bound and not charged (the loop idle "
            f"in its selector, the sibling free, the process quiet); the control loop's worst lateness "
            f"{scheduling:.3f} s"
        )


# The slowest exec wait of each command the harness ran, in seconds, by program name: how long its `Popen`
# took to return, which is how long the child took to exec. Printed once at the end of the scenario, so a
# runner whose first exec is slow says so in its log.
_exec_waits = {}


def _spawn_and_wait(argv):
    """Runs `argv` to its end on the calling thread: its exit status, its stdout, and how long the spawn
    took (`Popen` returns once the child has exec'd)."""
    began = time.monotonic()
    process = subprocess.Popen(argv, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
    exec_wait = time.monotonic() - began
    stdout, _ = process.communicate()
    return process.returncode, stdout, exec_wait


async def _run_async(*argv):
    """Runs a command on a worker thread, so the loop stays free: its exit status and stdout.

    The ticker must measure the SDK, not this harness. A process spawn blocks its calling thread until the
    child has exec'd: CPython's `Popen` waits on a vfork on Linux and on an error pipe on macOS, and
    `asyncio.create_subprocess_exec` runs `Popen` on the loop's own thread. That is how this helper held the
    loop until 2026-10-22. With the child's exec delayed 150 ms, as a cold disk would delay it, the test
    failed in phase (1) exactly as the ubuntu lane had: 0.150 s late, 0.002 s of CPU, a sibling thread
    free (docs/bugs/2026-10-22-the-async-sdk-tests-own-process-spawns-held-the-loop-it-measures.md). On a
    worker thread the same wait holds only that thread. The hand-off itself costs the loop something once: the
    first call imports the executor's module on the calling thread, so the scenario makes its first hand-off
    before the ticker starts (docs/bugs/2026-10-23-the-async-sdk-tests-first-thread-handoff-imported-a-module-on-the-loop.md)."""
    status, stdout, exec_wait = await asyncio.to_thread(_spawn_and_wait, argv)
    name = os.path.basename(argv[0])
    _exec_waits[name] = max(_exec_waits.get(name, 0.0), exec_wait)
    return status, stdout.decode()


async def _daemon_pids(binary, instance):
    """The pids of the daemon an anchor for `instance` supervises (scoped by instance, never a bare
    name)."""
    _, stdout = await _run_async("pgrep", "-f", f"{binary} --instance {instance} daemon")
    return [int(pid) for pid in stdout.split()]


async def _wait_for(condition, what):
    deadline = time.monotonic() + STARTUP_SECS
    while not await condition():
        if time.monotonic() >= deadline:
            raise AssertionError(what)
        await asyncio.sleep(POLL_SECS)


async def _state_of(pid):
    """The process's state letter, or `None` when the process is gone. `ps` reads it on every platform
    the SDK ships on; Linux has `/proc` and macOS does not."""
    status, stdout = await _run_async("ps", "-o", "state=", "-p", str(pid))
    letter = stdout.strip()
    return letter[0] if status == 0 and letter else None


def _signal_if_alive(pid, number):
    """Signals `pid`, tolerating only its having already exited — which is the state the caller wanted."""
    try:
        os.kill(pid, number)
    except ProcessLookupError:
        pass


async def _stop_and_wait(pids, what):
    """SIGSTOP every pid and return only once each has really stopped (or gone).

    `kill` and `killpg` queue a signal and return; the target stops when it is next scheduled. A daemon
    that has been signalled but not yet stopped is still a live daemon and still answers, so a test that
    calls straight after the signal is not yet testing a silent daemon.
    """
    for pid in pids:
        _signal_if_alive(pid, signal.SIGSTOP)

    async def all_stopped():
        for pid in pids:
            if await _state_of(pid) not in ("T", None):
                return False
        return True

    await _wait_for(all_stopped, what)


async def _stop_group_and_wait(binary, instance, anchor_pid):
    """Stop the anchor's whole process group and return the group id once every member has really
    stopped.

    Measured on Linux 2026-10-21: with the call made straight after `killpg`, the daemon answered it 5
    times in 100, reading state `R` in the instant after the stop — which is what made this test
    intermittent on CI (2026-10-02, 2026-10-06). The anchor is confirmed stopped first, because only a
    running anchor can fork a replacement that the group signal has already swept past; once it is
    stopped the set of daemons is closed, so any still running is stopped individually.
    """
    group = os.getpgid(anchor_pid)
    os.killpg(group, signal.SIGSTOP)
    await _stop_and_wait([anchor_pid], "the anchor stopped")

    async def daemons_stopped():
        running = []
        for pid in await _daemon_pids(binary, instance):
            if await _state_of(pid) not in ("T", None):
                running.append(pid)
        for pid in running:
            _signal_if_alive(pid, signal.SIGSTOP)
        return not running

    await _wait_for(daemons_stopped, "every daemon the anchor supervises stopped")
    return group


class SlatesAsyncEveryCallEnds(unittest.TestCase):
    @unittest.skipIf(
        _daemon_binary() is None,
        "no built `slates` daemon binary (set SLATES_DAEMON or build the cli) — skipped loudly",
    )
    def test_every_async_call_ends_across_restart_silence_cancellation_and_death(self):
        """AUD-29-19 / AUD-29-20: every call ends, and the loop is never held. Do, with an independent
        ticker on the loop and every connect made from it: (1) stop the daemon, issue three bounds' worth
        of list calls, and kill it under them, so the anchor restarts it; (2) stop the anchor and the
        daemon and issue a call; (3) issue a call to the stopped daemon and cancel it; (4) connect
        afresh, kill the anchor and its daemon for good and issue calls. Expect: the overflow refused at
        once; every admitted call answered after the restart; the call to the stopped daemon failed
        Stalled; the cancelled call released (nothing left waiting); every call after the death failed
        DaemonGone (the daemon's exit seen, not a stall); and the ticker never late past its bound."""
        asyncio.run(self._every_call_ends())

    async def _every_call_ends(self):
        binary = _daemon_binary()
        instance = f"slates-py-ends-{os.getpid()}"
        anchor = subprocess.Popen(
            [binary, "--instance", instance, "anchor", "--quick", "--shards", "1"],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            start_new_session=True,
        )
        noise = Noise()
        # The harness's first hand-off to a worker thread, before anything is measured. That first call makes
        # `run_in_executor` create the default executor, and `concurrent.futures` imports its thread module
        # (`queue` with it) lazily, on the calling thread, which is the loop's. An import reads from disk with
        # the GIL released, so on a slow disk it held the loop 0.672 s in 'connecting' (run 39974152220), and a
        # 0.3 s delay in that import failed this test 5 of 5 on macOS and 3 of 3 on Linux
        # (docs/bugs/2026-10-23-the-async-sdk-tests-first-thread-handoff-imported-a-module-on-the-loop.md).
        await asyncio.to_thread(lambda: None)
        ticker = Ticker()
        try:
            client = await _connect_when_ready(instance)
            status, _ = await _run_async(binary, "--instance", instance, "bootstrap", "root")
            self.assertEqual(status, 0, "the consensus group bootstraps")
            limit = client.outstanding_limit()

            ticker.phase = "(1) overflow, then a restart under the outstanding calls"
            # (1) Overflow, then a restart under the calls. Stopped first, so the calls stay outstanding.
            first = await _daemon_pids(binary, instance)
            self.assertTrue(first, "the daemon is running")
            await _stop_and_wait(first, "the daemon stopped")
            calls = []
            refused = 0
            for _ in range(limit * 3):
                try:
                    calls.append(client.list())
                except slates.SlatesError as error:
                    self.assertIn("TooManyOutstanding", str(error))
                    refused += 1
            for pid in first:
                os.kill(pid, signal.SIGKILL)
            settled = await asyncio.gather(*calls, return_exceptions=True)
            self.assertGreater(refused, 0, "the overflow is refused at once")
            failed = [str(result) for result in settled if isinstance(result, BaseException)]
            self.assertEqual(failed, [], "every admitted call is answered after the restart")
            self.assertGreaterEqual(client.reconnects(), 1, "non-vacuous: recovered by a reconnect")
            async def restarted():
                return bool(await _daemon_pids(binary, instance))

            await _wait_for(restarted, "the anchor restarted the daemon")

            ticker.phase = "(2) a live but silent daemon, stopped by signal"
            # (2) A live but silent daemon: the call ends Stalled. The anchor's whole process group (the
            # anchor leads it, `start_new_session`, and every daemon it spawns is in it) is stopped in one
            # signal, so its supervision cannot replace the silent daemon and no child it forked can escape
            # the stop — and the helper waits until every member has really stopped, because the signal
            # only queues and a daemon still running answers the call this requires to stall.
            group = await _stop_group_and_wait(binary, instance, anchor.pid)
            with self.assertRaisesRegex(slates.SlatesError, "Stalled"):
                await client.list()

            ticker.phase = "(3) a cancelled call is released"
            # (3) A cancelled call is released: nothing is left waiting on the loop.
            waiting = asyncio.ensure_future(client.list())
            await asyncio.sleep(POLL_SECS)
            waiting.cancel()
            with self.assertRaises(asyncio.CancelledError):
                await waiting
            await asyncio.sleep(POLL_SECS)
            os.killpg(group, signal.SIGCONT)
            self.assertIsInstance(await client.list(), list, "the client serves after the cancel")

            ticker.phase = "(4) the anchor and its daemon killed for good"
            # (4) Death: a fresh client, then the anchor and its daemon killed for good.
            fresh = await _connect_when_ready(instance)
            os.killpg(os.getpgid(anchor.pid), signal.SIGKILL)
            async def gone():
                return not await _daemon_pids(binary, instance)

            await _wait_for(gone, "the daemon is gone")
            doomed = await asyncio.gather(
                *(fresh.list() for _ in range(limit)), return_exceptions=True
            )
            for result in doomed:
                self.assertIsInstance(result, slates.SlatesError)
                self.assertIn("DaemonGone", str(result))
            # Stop measuring before tearing the control down. `noise.stop()` blocks: `communicate()`
            # closes the child's stdin, reads to EOF and waits for a whole Python interpreter to exit,
            # and it was being called while the ticker still ran — so that block landed in the ticker's
            # figures, charged to the SDK. The control has already stopped by then, so `scheduling`
            # cannot account for it either. Measured on an idle machine 2026-10-20: `communicate()`
            # blocked 3–11 ms and the loop's lateness tracked it almost exactly (10.9 ms blocked →
            # 10.6 ms recorded), while the sampler reported a flat ~8 ms.
            ticker.stop()
            scheduling = noise.stop()
            print(
                "the harness's own process spawns, each off the loop: slowest exec wait "
                + ", ".join(f"{name} {seconds:.3f} s" for name, seconds in sorted(_exec_waits.items())),
                file=sys.stderr,
            )
            print(ticker.summary(scheduling), file=sys.stderr)
            print(_wait_delays(), file=sys.stderr)
            stack = ticker.held_stack or (
                "  (never seen busy: the sibling or the whole process was held too, or the process burned CPU)\n"
            )
            self.assertLess(
                ticker.held, TICK_BOUND_SECS + scheduling,
                f"the loop was never held past the bound, {TICK_BOUND_SECS} s plus the control loop's worst "
                f"lateness: {ticker.summary(scheduling)}. CPU burned means code ran on the loop holding the GIL; "
                f"the sibling late too means the GIL or the whole process was held, and the control's figure "
                f"tells those apart; a busy stack names a call that blocked the loop. The loop's thread, first "
                f"seen busy in the worst charged tick:\n{stack}",
            )
        finally:
            noise.stop()
            ticker.stop()
            try:
                os.killpg(os.getpgid(anchor.pid), signal.SIGKILL)
            except ProcessLookupError:
                pass
            anchor.wait()


if __name__ == "__main__":
    unittest.main()
