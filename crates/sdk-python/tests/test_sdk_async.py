"""By-use tests for the slates Python SDK's ASYNC surface (R6, D-19): the AsyncClient's verbs are
awaitable on a real asyncio loop and resolve by the completion fd's readiness — the daemon signals
the fd, `loop.add_reader` fires, and the future resolves — never by blocking the loop. The sync
`Client` is the thin facade over the same daemon; this is the primary async form. Written with the
stdlib `unittest`/`asyncio` so it runs with a bare `python3 -m unittest` — no third-party runner. The
daemon round-trip is gated: it runs only when a built `slates` daemon binary is on PATH or named by
SLATES_DAEMON, and otherwise skips loudly (never fails on a machine without the binary).

Run: `maturin develop && SLATES_DAEMON=target/debug/slates python3 -m unittest discover crates/sdk-python/tests`."""

import asyncio
import faulthandler
import os
import signal
import subprocess
import sys
import threading
import time
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
# Shape: the noise sampler's sleep — the ticker's own period, so both measure the same scheduling grain.
NOISE_SAMPLE_SECS = TICK_SECS

# The noise sampler, run in its own interpreter: it sleeps NOISE_SAMPLE_SECS at a time until its stdin closes,
# then prints the most any sleep returned late.
_NOISE_SAMPLER = (
    "import sys, threading, time\n"
    "stop = threading.Event()\n"
    "threading.Thread(target=lambda: (sys.stdin.read(), stop.set()), daemon=True).start()\n"
    "worst = 0.0\n"
    "while not stop.is_set():\n"
    "    asked = time.monotonic()\n"
    "    time.sleep(float(sys.argv[1]))\n"
    "    worst = max(worst, time.monotonic() - asked - float(sys.argv[1]))\n"
    "print(worst)\n"
)


class Noise:
    """The machine's own scheduling noise while the test runs: a sampler in a separate process that sleeps
    NOISE_SAMPLE_SECS at a time and records how much later than asked each sleep returned — time the OS kept
    it off a core, as it can keep the loop's thread off. A separate process, not a thread: a native call that
    wrongly held the GIL would delay a sampler thread as much as the loop and mask the very hold the bound
    exists to catch. The loop's lateness is judged against the bound plus this noise — the pattern of the
    Rust driver test and the Node SDK test (CI 2026-10-01: a macOS runner's loop was 0.186 s late under load)."""

    def __init__(self):
        self._child = subprocess.Popen(
            [sys.executable, "-c", _NOISE_SAMPLER, str(NOISE_SAMPLE_SECS)],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            text=True,
        )
        self._worst = None

    def stop(self):
        """Ends the sampler (once; later calls return the same figure) and returns its worst lateness."""
        if self._worst is None:
            out, _ = self._child.communicate(input="")
            self._worst = float(out.strip() or "0")
        return self._worst


class Ticker:
    """An independent task on the loop: sleeps one period at a time and records how late it woke, and in
    which phase of the scenario the worst lateness happened.

    The phase is carried because this test has failed on CI without one and could not be acted on: runs
    39538566246 and 39539155335 reported 0.298 s and 0.243 s of lateness against a 0.1 s bound, with the
    separate-process noise sampler seeing 1 ms and 0 ms over the same window — so the loop really was held,
    and nothing said by what. The scenario kills a daemon under outstanding calls, stops a process group,
    cancels a call and kills an anchor; those are very different suspects. Whoever sees the next failure
    should not have to guess which."""

    def __init__(self):
        self.worst = 0.0
        self.phase = "connecting"
        self.worst_phase = self.phase
        self.worst_cpu = 0.0
        self.worst_thread = 0.0
        self.thread_late = 0.0
        self._stop = threading.Event()
        # A sibling probe *inside this process*, on a thread of its own. The separate-process sampler
        # answers "was the machine scheduling anyone", which is not the same question: a process that
        # sleeps 99.9% of the time is easy to schedule even on a runner that is starving the test, whose
        # daemon, anchor and sampler all want the same cores. This one answers "could any Python code in
        # *this* process run", and the pair of them separates the two causes. The test's older comment
        # rejected a thread because a native call holding the GIL would delay it as much as the loop —
        # which is precisely the signal, once there is a process probe beside it to rule out starvation.
        self._thread = threading.Thread(target=self._sibling, daemon=True)
        self._thread.start()
        self.task = asyncio.get_running_loop().create_task(self._run())
        # Names the loop's thread in the hold dump `_run` arms: faulthandler prints each thread as
        # "Thread 0x" and sixteen hex digits, the same number `threading.get_ident` returns.
        print(
            f"ticker: the loop runs on thread {threading.get_ident():#018x}; a hold past "
            f"{TICK_SECS + TICK_BOUND_SECS:.3f} s prints every thread's Python stack",
            file=sys.stderr,
        )

    def _sibling(self):
        while not self._stop.is_set():
            due = time.monotonic() + TICK_SECS
            time.sleep(TICK_SECS)
            self.thread_late = max(self.thread_late, time.monotonic() - due)

    async def _run(self):
        try:
            while True:
                await self._tick()
        finally:
            faulthandler.cancel_dump_traceback_later()

    async def _tick(self):
        """One period: arms the hold dump, sleeps, disarms it, and records how late the loop came back.

        The probes below say what kind of hold it was; the dump says where. Before each sleep, faulthandler's
        watchdog is armed for one period plus the bound. If the loop is not back by then, the watchdog — a C
        thread that needs no GIL — prints every thread's Python stack while the hold is still in progress, and
        the loop thread's frames show the call that held it. Added after run 39974152220 (2026-10-23): 0.672 s
        in phase 'connecting', no CPU and the sibling free, with the harness's own spawns already off the loop,
        so the probes ruled out every suspect they could name."""
        due = time.monotonic() + TICK_SECS
        cpu = time.process_time()
        thread_before = self.thread_late
        faulthandler.dump_traceback_later(TICK_SECS + TICK_BOUND_SECS, file=sys.stderr)
        await asyncio.sleep(TICK_SECS)
        faulthandler.cancel_dump_traceback_later()
        late = time.monotonic() - due
        if late > self.worst:
            self.worst, self.worst_phase = late, self.phase
            # How much CPU this process burned across the gap, and whether a plain Python thread of
            # its own was held over the same gap. Together they read three ways:
            #
            #   CPU burned                      code ran on the loop while holding the GIL
            #   no CPU, sibling thread held     the OS kept this whole process off a core (the
            #                                   separate noise sampler is usually late too), or a
            #                                   blocking call kept the GIL (it is not)
            #   no CPU, sibling thread free     a blocking call on the loop thread that releases
            #                                   the GIL: what `park_timeout` in Rust looks like,
            #                                   and what a process spawn waiting on its child's
            #                                   exec looked like (`_run_async`, 2026-10-22)
            #
            # Measured 2026-10-20 to check neither probe is vacuous: a deliberate 200 ms busy GIL
            # hold read 200.7 ms late / 195.6 ms CPU / 24.7 ms thread (the sibling gets the GIL back
            # in switch-interval slices, so it reads less than the hold); a 200 ms GIL-releasing
            # `time.sleep` read 272.3 ms late / 1.0 ms CPU / 0.2 ms thread. And 2026-10-23, so the
            # middle row is measured too: a 200 ms native call that blocks while holding the GIL
            # (`ctypes.PyDLL` usleep) read 334–362 ms late / 0.3–0.5 ms CPU / 269–330 ms thread, 5 of 5
            # (Python 3.9.6, a loaded Mac): the loop's next `select` releases the GIL, and the sibling
            # records its wait before the ticker reads it.
            self.worst_cpu = time.process_time() - cpu
            self.worst_thread = self.thread_late - thread_before


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
    worker thread the same wait holds only that thread."""
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
            # Stop measuring before tearing the sampler down. `noise.stop()` blocks: `communicate()`
            # closes the child's stdin, reads to EOF and waits for a whole Python interpreter to exit,
            # and it was being called while the ticker still ran — so that block landed in
            # `ticker.worst`, charged to the SDK. The sampler has already stopped by then, so
            # `scheduling` cannot account for it either. Measured on an idle machine 2026-10-20:
            # `communicate()` blocked 3–11 ms and the loop's lateness tracked it almost exactly
            # (10.9 ms blocked → 10.6 ms recorded), while the sampler reported a flat ~8 ms.
            ticker.task.cancel()
            worst, worst_phase = ticker.worst, ticker.worst_phase
            scheduling = noise.stop()
            print(
                "the harness's own process spawns, each off the loop: slowest exec wait "
                + ", ".join(f"{name} {seconds:.3f} s" for name, seconds in sorted(_exec_waits.items())),
                file=sys.stderr,
            )
            self.assertLess(
                worst, TICK_BOUND_SECS + scheduling,
                f"the loop was never held: worst lateness {worst:.3f} s in phase "
                f"{worst_phase!r}, bound {TICK_BOUND_SECS} s plus the machine's own scheduling "
                f"noise {scheduling:.3f} s. Across that gap this process burned "
                f"{ticker.worst_cpu:.3f} s of CPU and a plain Python thread of its own was held "
                f"{ticker.worst_thread:.3f} s — CPU burned means code ran on the loop holding the GIL; "
                f"no CPU with the thread held too means the whole process was kept off a core or a "
                f"blocking call kept the GIL, and the noise figure tells those apart; no CPU with the "
                f"thread free means the loop's thread was blocked in a call that released the GIL",
            )
        finally:
            noise.stop()
            ticker.task.cancel()
            ticker._stop.set()
            try:
                os.killpg(os.getpgid(anchor.pid), signal.SIGKILL)
            except ProcessLookupError:
                pass
            anchor.wait()


if __name__ == "__main__":
    unittest.main()
