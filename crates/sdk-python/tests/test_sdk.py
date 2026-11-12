"""By-use tests for the slates Python SDK (R5): the extension loads, exposes the typed client's
lifecycle verbs, and maps a typed refusal to a Python exception rather than crashing. Written with the
stdlib `unittest` so it runs with a bare `python3 -m unittest` — no third-party test runner to install.
The daemon round-trip is gated: it runs only when a built `slates` daemon binary is on PATH or named by
SLATES_DAEMON, and otherwise skips loudly (never fails on a machine without the binary) — the vorpal
env-gated discipline. Run: `maturin develop && python3 -m unittest discover crates/sdk-python/tests`."""

import json
import os
import shutil
import signal
import subprocess
import tempfile
import time
import unittest

import slates

# The lifecycle and merge verbs this slice binds; `land` and the async form are owed.
BOUND_VERBS = {
    "connect",
    "create",
    "snapshot",
    "status",
    "list",
    "resize",
    "destroy",
    "land",
    "create_green",
    "create_work",
    "versions",
    "changed_since",
    "edit",
    "submit",
    "rebase",
    "unlink",
    "rename",
    "mkdir",
    "rmdir",
    "chmod",
    "symlink",
    "link",
    "set_xattr",
    "remove_xattr",
    "client_id",
    "reconnects",
}
# Test deadlines in nanoseconds — generous on purpose. This is a functional round-trip that spawns a real
# anchor+daemon on a shared, often-loaded CI runner, not a latency gate (the provisioning histogram is that);
# a tight 5 ms reply deadline flaked with `Stalled` when a verb's reply crossed it under load. A production
# caller derives these from the machine's budgets.
REPLY_NS = 1_000_000_000
RECONNECT_NS = 2_000_000_000
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


def _listed_until_gone(client, volume):
    """Polls `list` until `volume` is no longer in it, within the startup budget: a destroy replies
    first and the daemon tears the volume down in cooperative slices after (§4.4), so the very next
    list can still show it — measured 54–302 µs after the reply here, 2026-09-14. Returns whether it
    is gone; the Rust client test (`wait_until_listed`) waits the same way."""
    deadline = time.monotonic() + STARTUP_SECS
    while any(v["id"] == volume for v in client.list()):
        if time.monotonic() >= deadline:
            return False
        time.sleep(POLL_SECS)
    return True


def _cli(binary, instance, *args):
    """Runs `slates --instance INSTANCE ARGS...` and returns its standard output, failing on a refusal."""
    done = subprocess.run([binary, "--instance", instance, *args], capture_output=True, text=True,
                          timeout=STARTUP_SECS)
    if done.returncode != 0:
        raise AssertionError(f"slates {' '.join(args)}: {done.stderr}")
    return done.stdout


def _listed_until(instance, predicate):
    """Lists through a fresh SDK client until some entry satisfies `predicate`, within the startup budget: across a
    daemon's restart the first lists may be refused or may not yet show the recovered catalog. Returns the last list
    and whether it held."""
    deadline = time.monotonic() + STARTUP_SECS
    listed = []
    while time.monotonic() < deadline:
        try:
            listed = _connect_when_ready(instance).list()
        except slates.SlatesError:
            listed = []
        if any(predicate(v) for v in listed):
            return listed, True
        time.sleep(POLL_SECS)
    return listed, False


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
        # `start_new_session` makes the anchor its own process-group leader, so teardown can kill the
        # whole group — the anchor and the daemon it supervises — at once, rather than leaving the daemon
        # up until its own liveness timeout notices the anchor gone.
        anchor = subprocess.Popen(
            [binary, "--instance", instance, "anchor", "--quick", "--shards", "1"],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            start_new_session=True,
        )
        try:
            client = _connect_when_ready(instance)
            subprocess.run([binary, "--instance", instance, "bootstrap", "root"],
                           check=True, timeout=STARTUP_SECS)

            # create → a 32-hex volume id.
            volume = client.create("sdk-roundtrip", VOLUME_BYTES)
            self.assertEqual(len(volume), 32)
            self.assertIsInstance(client.client_id(), int)

            # snapshot → a monotonic sequence.
            snapshot = client.snapshot(volume)
            self.assertIsInstance(snapshot, int)

            # A hostile volume id (the hostile-input rule for every parser of external bytes): 32 bytes whose
            # first pair splits a multi-byte character is refused as SlatesError, and the process lives on.
            # Before 2026-09-29 the parser sliced the string through the character and panicked.
            with self.assertRaises(slates.SlatesError) as caught:
                client.status("a\u20ac" + "0" * 28)
            self.assertIn("not a hex byte", str(caught.exception))

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

            # list → the volume appears with its fields (a scratch volume is not an overlay).
            listed = client.list()
            entry = next((v for v in listed if v["id"] == volume), None)
            self.assertIsNotNone(entry, "the created volume appears in list")
            self.assertEqual(entry["name"], "sdk-roundtrip")
            self.assertFalse(entry["overlay"])

            # resize → a larger bound succeeds.
            client.resize(volume, 2 * VOLUME_BYTES)

            # destroy → the volume is gone from a later list (the teardown runs in slices after the
            # reply, so the list is polled within the budget).
            client.destroy(volume)
            self.assertTrue(
                _listed_until_gone(client, volume),
                "the destroyed volume is gone from list",
            )

            # merge workflow (§4.16): a green, a work over it, a content edit, a clean submit.
            green = client.create_green("g-roundtrip")
            self.assertEqual(len(green), 32)
            work = client.create_work(green, "w-roundtrip")
            self.assertEqual(len(work["id"]), 32)
            base = work["base"]
            client.edit(work["id"], "/notes.txt", 0, 0, b"hello merge")
            outcome = client.submit(work["id"])
            self.assertTrue(outcome["ok"], f"the submit landed cleanly: {outcome}")
            self.assertIsInstance(outcome["version"], int)
            self.assertEqual(outcome["conflicts"], [])
            # the green advanced past the work's base and lists the edited file.
            self.assertGreater(client.versions(green), base)
            changed = client.changed_since(green, base)
            self.assertTrue(any("notes.txt" in path for path in changed), changed)
            # a fresh work off the advanced head rebases cleanly (no pending edits, nothing to conflict).
            fresh = client.create_work(green, "w2-roundtrip")
            rebased = client.rebase(fresh["id"])
            self.assertTrue(rebased["ok"], f"a fresh work rebases cleanly: {rebased}")

            # namespace operations (§4.16): build a directory tree with a rename, an unlink, a mode
            # change, a symlink, a hard link and an xattr on a work volume, then submit — the counterpart
            # to edit's content splice (edit creates a file on write, so these have real targets).
            ns_work = client.create_work(green, "w-ns-roundtrip")
            ns_base = ns_work["base"]
            work_id = ns_work["id"]
            client.edit(work_id, "/keep.txt", 0, 0, b"keep")
            client.edit(work_id, "/gone.txt", 0, 0, b"gone")
            client.unlink(work_id, "/gone.txt")
            client.mkdir(work_id, "/d")
            client.rename(work_id, "/keep.txt", "/d/keep.txt")
            client.chmod(work_id, "/d/keep.txt", 0o600)
            client.symlink(work_id, "/d/link", "keep.txt")
            client.link(work_id, "/d/hard.txt", "/d/keep.txt")
            client.set_xattr(work_id, "/d/keep.txt", "user.slates", b"1")
            ns_outcome = client.submit(work_id)
            self.assertTrue(ns_outcome["ok"], f"the namespace ops submitted cleanly: {ns_outcome}")
            ns_changed = client.changed_since(green, ns_base)
            self.assertTrue(any("keep.txt" in path for path in ns_changed), ns_changed)
        finally:
            # Kill the whole process group so the supervised daemon goes with the anchor at once.
            try:
                os.killpg(os.getpgid(anchor.pid), signal.SIGKILL)
            except ProcessLookupError:
                pass
            anchor.wait()


    @unittest.skipIf(
        _daemon_binary() is None,
        "no built `slates` daemon binary (set SLATES_DAEMON or build the cli) — listing round-trip skipped loudly",
    )
    def test_a_listing_says_which_volumes_are_live_shipped_and_which_recovery_could_not_rebuild(self):
        """The two listing fields the CLI's `volume list` shows, by use against a real anchor+daemon: a volume made
        live-shipped lists `live_shipped` true; a volume over a host directory whose directory is removed before the
        daemon restarts lists `not_recovered` as its cause's kind, `"BaseMissing"`; and neither field is set on a
        volume it does not describe. Before 2026-11-09 the SDK's entries carried neither."""
        binary = _daemon_binary()
        instance = f"slates-sdk-list-{os.getpid()}"
        anchor = subprocess.Popen(
            [binary, "--instance", instance, "anchor", "--quick", "--shards", "1"],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            start_new_session=True,
        )
        scratch = tempfile.mkdtemp(prefix="slates-sdk-list-")
        try:
            client = _connect_when_ready(instance)
            _cli(binary, instance, "bootstrap", "root")
            live = client.create("sdk-live", VOLUME_BYTES)
            _cli(binary, instance, "live-ship", live)
            base = os.path.join(scratch, "base")
            os.mkdir(base)
            created = _cli(binary, instance, "volume", "create", "sdk-unrecovered", "--bounded", "8MiB",
                           "--base", base)
            unrecovered = next(line.split(": ", 1)[1].strip() for line in created.splitlines()
                               if line.startswith("id: "))
            _cli(binary, instance, "volume", "snapshot", unrecovered)
            os.rmdir(base)
            # The daemon restarts in order under its anchor; its successor holds the record and cannot rebuild it.
            daemon = json.loads(_cli(binary, instance, "status", "--json"))["pid"]
            os.kill(daemon, signal.SIGTERM)
            listed, recovered = _listed_until(
                instance, lambda v: v["id"] == unrecovered and v["not_recovered"] is not None
            )
            self.assertTrue(recovered, f"the restarted daemon lists the unrecovered volume: {listed}")
            entries = {v["id"]: v for v in listed}
            self.assertEqual(entries[unrecovered]["not_recovered"], "BaseMissing")
            self.assertFalse(entries[unrecovered]["live_shipped"])
            self.assertTrue(entries[unrecovered]["overlay"])
            self.assertTrue(entries[live]["live_shipped"], f"the live-shipped volume says so: {entries[live]}")
            self.assertIsNone(entries[live]["not_recovered"])
        finally:
            try:
                os.killpg(os.getpgid(anchor.pid), signal.SIGKILL)
            except ProcessLookupError:
                pass
            anchor.wait()
            shutil.rmtree(scratch, ignore_errors=True)

if __name__ == "__main__":
    unittest.main()
