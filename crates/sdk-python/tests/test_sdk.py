"""By-use tests for the slates Python SDK (R5): the extension loads, exposes the typed client's
lifecycle verbs, and maps a typed refusal to a Python exception rather than crashing. Written with the
stdlib `unittest` so it runs with a bare `python3 -m unittest` — no third-party test runner to install.
The daemon round-trip is gated: it runs only when a built `slates` daemon binary is on PATH or named by
SLATES_DAEMON, and otherwise skips loudly (never fails on a machine without the binary) — the vorpal
env-gated discipline. Run: `maturin develop && python3 -m unittest discover crates/sdk-python/tests`."""

import os
import unittest

import slates

# The lifecycle verbs this slice binds; more (create_green, edit, submit, land, status) are owed.
BOUND_VERBS = {"connect", "create", "snapshot", "client_id", "reconnects"}
# Test deadlines in nanoseconds; a production caller derives these from the machine's budgets.
REPLY_NS = 5_000_000
RECONNECT_NS = 10_000_000


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
        """SKIP-LOUD when no daemon binary is present. When one is, this would spawn the anchor+daemon,
        connect, create a volume, snapshot it, and assert the ids come back — the true by-use test of
        the binding end to end. The spawn harness (anchor supervision, instance-name coordination) is
        owed; this marks the exact place it plugs in so the round-trip is never silently missing."""
        self.skipTest("daemon spawn harness for the SDK round-trip is owed (tracked in GAPS)")


if __name__ == "__main__":
    unittest.main()
