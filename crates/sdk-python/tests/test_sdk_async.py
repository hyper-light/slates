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
import signal
import subprocess
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
# Test deadlines in nanoseconds; a production caller derives these from the machine's budgets.
REPLY_NS = 1_000_000_000
RECONNECT_NS = 2_000_000_000
# How long to wait for a freshly spawned anchor+daemon to answer, and the pause between polls.
STARTUP_SECS = 20.0
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
TICK_BOUND_SECS = REPLY_NS / 10 / 1e9
# The ticker's own period: a tenth of its bound, so a held loop shows as lateness well past one period.
TICK_SECS = TICK_BOUND_SECS / 10


class Ticker:
    """An independent task on the loop: sleeps one period at a time and records how late it woke."""

    def __init__(self):
        self.worst = 0.0
        self.task = asyncio.get_running_loop().create_task(self._run())

    async def _run(self):
        while True:
            due = time.monotonic() + TICK_SECS
            await asyncio.sleep(TICK_SECS)
            self.worst = max(self.worst, time.monotonic() - due)


async def _run_async(*argv):
    """Runs a command without blocking the loop (the ticker must measure the SDK, not this harness's own
    process spawns, which take tens of milliseconds on a loaded runner): its exit status and stdout."""
    process = await asyncio.create_subprocess_exec(
        *argv, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.DEVNULL
    )
    stdout, _ = await process.communicate()
    return process.returncode, stdout.decode()


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
        ticker = Ticker()
        try:
            client = await _connect_when_ready(instance)
            status, _ = await _run_async(binary, "--instance", instance, "bootstrap", "root")
            self.assertEqual(status, 0, "the consensus group bootstraps")
            limit = client.outstanding_limit()

            # (1) Overflow, then a restart under the calls. Stopped first, so the calls stay outstanding.
            first = await _daemon_pids(binary, instance)
            self.assertTrue(first, "the daemon is running")
            for pid in first:
                os.kill(pid, signal.SIGSTOP)
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

            # (2) A live but silent daemon: the call ends Stalled. Its anchor is stopped first, or its
            # supervision would replace the silent daemon and the call would be recovered instead.
            live = await _daemon_pids(binary, instance)
            os.kill(anchor.pid, signal.SIGSTOP)
            for pid in live:
                os.kill(pid, signal.SIGSTOP)
            with self.assertRaisesRegex(slates.SlatesError, "Stalled"):
                await client.list()

            # (3) A cancelled call is released: nothing is left waiting on the loop.
            waiting = asyncio.ensure_future(client.list())
            await asyncio.sleep(POLL_SECS)
            waiting.cancel()
            with self.assertRaises(asyncio.CancelledError):
                await waiting
            await asyncio.sleep(POLL_SECS)
            for pid in live:
                os.kill(pid, signal.SIGCONT)
            os.kill(anchor.pid, signal.SIGCONT)
            self.assertIsInstance(await client.list(), list, "the client serves after the cancel")

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
            self.assertLess(
                ticker.worst, TICK_BOUND_SECS,
                f"the loop was never held: worst lateness {ticker.worst:.3f} s",
            )
        finally:
            ticker.task.cancel()
            try:
                os.killpg(os.getpgid(anchor.pid), signal.SIGKILL)
            except ProcessLookupError:
                pass
            anchor.wait()


if __name__ == "__main__":
    unittest.main()
