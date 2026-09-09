"""By-use tests for the slates Python SDK (R5): the extension loads, exposes the typed client's
lifecycle verbs, and maps a typed refusal to a Python exception rather than crashing. Written with the
stdlib `unittest` so it runs with a bare `python3 -m unittest` — no third-party test runner to install.
The daemon round-trip is gated: it runs only when a built `slates` daemon binary is on PATH or named by
SLATES_DAEMON, and otherwise skips loudly (never fails on a machine without the binary) — the vorpal
env-gated discipline. Run: `maturin develop && python3 -m unittest discover crates/sdk-python/tests`."""

import os
import subprocess
import time
import unittest

import slates

# The lifecycle verbs this slice binds; more (create_green, edit, submit, land) are owed.
BOUND_VERBS = {"connect", "create", "snapshot", "status", "client_id", "reconnects"}
# Test deadlines in nanoseconds; a production caller derives these from the machine's budgets.
REPLY_NS = 5_000_000
RECONNECT_NS = 10_000_000
# How long to wait for a freshly spawned anchor+daemon to answer, and the pause between polls.
STARTUP_SECS = 20.0
POLL_SECS = 0.02
# A small bounded RAM volume for the round-trip.
VOLUME_BYTES = 8 * 1024 * 1024


def _client_verbs():
    return {name for name in dir(slates.Client) if not name.startswith("_")}


def _daemon_binary():
    """The path to a built `slates` daemon binary, or None to skip the round-trip loudly."""
    named = os.environ.get("SLATES_DAEMON")
    if named and os.path.exists(named):
        return named
    for candidate in ("target/release/slates", "target/debug/slates"):
        if os.path.exists(candidate):
            return candidate
    return None


def _connect_when_ready(instance):
    """Retries connect until the just-spawned daemon answers within the startup budget, so the test
    does not race the anchor's boot — each attempt is one real rendezvous, the same the client uses."""
    deadline = time.monotonic() + STARTUP_SECS
    while True:
        try:
            return slates.Client.connect(instance, REPLY_NS, RECONNECT_NS)
        except slates.SlatesError:
            if time.monotonic() >= deadline:
                raise
            time.sleep(POLL_SECS)


class SlatesSdkSurface(unittest.TestCase):
    def test_module_exposes_the_client_surface(self):
        """The extension loads and exposes the Client class, the SlatesError exception, and a version."""
        self.assertIsInstance(slates.__version__, str)
        self.assertTrue(slates.__version__)
        self.assertTrue(hasattr(slates, "Client"))
        self.assertTrue(hasattr(slates, "SlatesError"))
        self.assertTrue(BOUND_VERBS <= _client_verbs())

    def test_connect_to_a_missing_daemon_raises_a_typed_error(self):
        """Connecting to an instance with no daemon raises SlatesError carrying the typed refusal — the
        error crosses the FFI boundary as an exception, not a crash or a silent failure."""
        with self.assertRaises(slates.SlatesError) as caught:
            slates.Client.connect("slates-sdk-no-such-instance", REPLY_NS, RECONNECT_NS)
        message = str(caught.exception)
        self.assertTrue("navailable" in message or "Ipc" in message, message)


class SlatesSdkRoundTrip(unittest.TestCase):
    @unittest.skipIf(
        _daemon_binary() is None,
        "no built `slates` daemon binary (set SLATES_DAEMON or build the cli) — round-trip skipped loudly",
    )
    def test_lifecycle_round_trip_over_a_live_daemon(self):
        """Spawns a real anchor+daemon, connects the SDK client to it, creates a volume, snapshots it,
        and reads its status back — the true by-use test of the binding end to end (R5). The anchor is
        a subprocess killed and reaped in `finally`, so a failed assertion leaves no daemon behind (the
        cli.rs AnchorProcess discipline, in Python)."""
        binary = _daemon_binary()
        instance = f"slates-sdk-{os.getpid()}"
        anchor = subprocess.Popen(
            [binary, "--instance", instance, "anchor", "--quick", "--shards", "1"],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        try:
            client = _connect_when_ready(instance)

            # create → a 32-hex volume id.
            volume = client.create("sdk-roundtrip", VOLUME_BYTES)
            self.assertEqual(len(volume), 32)
            self.assertIsInstance(client.client_id(), int)

            # snapshot → a monotonic sequence.
            snapshot = client.snapshot(volume)
            self.assertIsInstance(snapshot, int)

            # status → the volume's real fields, read back over the same rings.
            status = client.status(volume)
            self.assertEqual(status["id"], volume)
            self.assertEqual(status["name"], "sdk-roundtrip")
            self.assertEqual(status["attachments"], 0)
            self.assertEqual(status["snapshots"], 1)
            # A laptop places the head immediately (f=0) under host epoch 1, with no mirror.
            self.assertTrue(status["placed"])
            self.assertEqual(status["host_epoch"], 1)
            self.assertIsNone(status["mirror_age_ns"])
            self.assertIn("nfs_port", status)
            self.assertEqual(status["drifted"], [])
        finally:
            anchor.kill()
            anchor.wait()


if __name__ == "__main__":
    unittest.main()
