# slates — Python SDK

A thin [PyO3](https://pyo3.rs) extension over the typed Rust `slates` client (design §2.3, D-19):
your Python agent drives a slates daemon through the *same* rings and completion records the Rust
client uses — never a parallel reimplementation. Every method is one client call; a typed refusal
from the daemon becomes a `SlatesError`, and volume ids cross as lowercase hex.

Two clients over one daemon: `AsyncClient` is the **async-primary** form (R6) — every verb an
`async` method resolved on your running `asyncio` loop by the completion descriptor's readiness,
never blocking the loop — and `Client` is the thin synchronous facade.

## Install

```
pip install slates
```

Wheels are `cp39-abi3`: one wheel per platform serves every CPython 3.9 or newer, for manylinux and
musllinux (x86_64, aarch64), macOS (x86_64, arm64) and Windows (x64); an sdist is published for
anything else and builds with a Rust toolchain. From a source checkout instead, with
[maturin](https://www.maturin.rs):

```
maturin build --release -m crates/sdk-python/Cargo.toml   # a cp39-abi3 wheel in target/wheels/
pip install target/wheels/slates-*.whl
```

or, for development, `maturin develop` into the active virtualenv. How a release is cut and
published is in `docs/publish.md`.

## Connect

```python
import slates

# reply_ns / reconnect_ns are nanosecond deadlines; a production caller derives them from the
# machine's budgets. The instance is the daemon `slates anchor --instance <name>` published.
client = slates.Client.connect("default", 5_000_000, 10_000_000)
```

A connection is pinned to the thread that made it (the rings are single-consumer), so use one
`Client` per thread. Connecting where no daemon answers raises `slates.SlatesError`.

`AsyncClient` resolves each verb on a running `asyncio` loop by the completion channel's readiness
(`loop.add_reader`), never blocking the loop — connecting included:

```python
client = await slates.AsyncClient.connect("default", 5_000_000, 10_000_000)
volumes = await client.list()
```

A call waits while its daemon lives, however long a busy machine keeps the daemon off its CPU: a daemon
that will never answer is ended (by its anchor, or by its own watchdog when it runs alone), and its calls
are then resent to the restarted daemon. Every async call ends: answered, refused typed, `DaemonGone` when
the daemon is gone and no restart answers within the reconnect budget, or `CompletionLost` when the
completion reader fails; a call cancelled by its task is released. Calls past the client's bound (its
`outstanding_limit()` in flight, as many again queued) raise `TooManyOutstanding` at once. **On Windows** the completion channel is a socket
(D-10), and `add_reader` on a socket needs a `SelectorEventLoop` — the default `ProactorEventLoop`
has none — so set the selector policy before you run the loop:

```python
import asyncio, sys
if sys.platform == "win32":
    asyncio.set_event_loop_policy(asyncio.WindowsSelectorEventLoopPolicy())
```

On macOS and Linux the default loop already polls fds, so nothing extra is needed.

## Volume lifecycle

```python
volume = client.create("scratch", 8 * 1024 * 1024)   # a bounded 8 MiB RAM volume; returns a hex id
status = client.status(volume)                        # a dict: placed, attachments, nfs_port, drift, …
for v in client.list():                               # dicts: id, name, referenced_bytes, overlay, …
    print(v["name"])
client.snapshot(volume)                               # a snapshot sequence number
client.resize(volume, 16 * 1024 * 1024)
client.destroy(volume)
```

## The merge workflow (§4.16)

Provision a shared **green**, clone a **work** volume, edit it, and submit — conflicts come back as
byte-exact windows to rebase against, never a silent interleave.

```python
green = client.create_green("main")
work = client.create_work(green, "feature")            # {"id": ..., "base": <green version>}

# content edits: a splice at `at` — remove `delete_len` bytes, insert `data` (bytes).
client.edit(work["id"], "/notes.txt", 0, 0, b"hello")

# namespace operations, the counterpart to a content edit:
client.mkdir(work["id"], "/dir")
client.rename(work["id"], "/notes.txt", "/dir/notes.txt")
client.chmod(work["id"], "/dir/notes.txt", 0o600)
client.symlink(work["id"], "/dir/link", "notes.txt")
client.link(work["id"], "/dir/hard.txt", "/dir/notes.txt")
client.set_xattr(work["id"], "/dir/notes.txt", "user.tag", b"v")
client.copy(work["id"], "/dir", "/dir-copy")  # by reference, from the work's base (version=, replace= optional)
# also: unlink, rmdir, remove_xattr

outcome = client.submit(work["id"])
if outcome["ok"]:
    print("landed at version", outcome["version"])
else:
    for window in outcome["conflicts"]:
        print("conflict at", window["path"], window["at"], window["len"])
    # resolve, then client.rebase(work["id"]) and submit again

print(client.versions(green))              # the green's head version
print(client.changed_since(green, 0))      # paths changed since a version
```

## A volume at a path for one command (`slates exec`, Linux)

`slates.exec_command` builds the command that runs slates' chosen-path launcher: the volume shown at an existing
directory for one command, in a namespace of its own. Spawn it as any command; the launcher finds the volume under
`SLATES_ROOT` in the environment you give it (the daemon's root mount):

```python
import asyncio, os, subprocess
import slates

argv = slates.exec_command("scratch", "/home/u/proj/build", ["cargo", "build"])
# ["slates", "exec", "--volume=scratch", "--at=/home/u/proj/build", "--", "cargo", "build"]
subprocess.run(argv, env={**os.environ, "SLATES_ROOT": "/run/user/1000/slates"}, check=True)
# or, on a running loop: await asyncio.create_subprocess_exec(*argv, env=...)
```

`program=` names the `slates` binary when it is not the one on the `PATH`.

**AppArmor (Ubuntu 24.04 and later).** Where AppArmor restricts unprivileged user namespaces
(`kernel.apparmor_restrict_unprivileged_userns=1`), the launcher needs slates' AppArmor profile for the binary, which an
administrator loads once, as root, and you opt into it per command with `apparmor_profile=True` (off unless set):

```python
print(slates.apparmor_profile("/usr/local/bin/slates"))   # the profile: save it as /etc/apparmor.d/usr.local.bin.slates
argv = slates.exec_command("scratch", "/home/u/proj/build", ["cargo", "build"],
                           apparmor_profile=True, program="/usr/local/bin/slates")
```

With the option, the launcher makes its namespace under the profile and runs the command outside it. Without it, the
launcher never uses the profile, even where it is loaded. `slates apparmor check` on the host says which applies and
prints the root commands that load or remove the profile (`docs/cli.md`, "The launcher and AppArmor").

## No grants (R10)

There is no verb here that creates a landing grant — landing to a real disk is a human-only act at
the CLI or a confirmation surface, never something an agent answers for itself.
