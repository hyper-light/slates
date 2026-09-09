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
}
# Test deadlines in nanoseconds; a production caller derives these from the machine's budgets.
REPLY_NS = 5_000_000
RECONNECT_NS = 10_000_000
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
            return slates.AsyncClient.connect(instance, REPLY_NS, RECONNECT_NS)
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

            # await destroy → the volume is gone from a later list; resolves to None.
            self.assertIsNone(await client.destroy(volume))
            after = await client.list()
            self.assertTrue(
                all(v["id"] != volume for v in after), "the destroyed volume is gone from the list"
            )

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


if __name__ == "__main__":
    unittest.main()
