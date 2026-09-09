# slates — Python SDK

A thin [PyO3](https://pyo3.rs) extension over the typed Rust `slates` client (design §2.3, D-19):
your Python agent drives a slates daemon through the *same* rings and completion records the Rust
client uses — never a parallel reimplementation. Every method is one client call; a typed refusal
from the daemon becomes a `SlatesError`, and volume ids cross as lowercase hex.

This is the **synchronous** base the design says the async form wraps (R6); the async form (over the
completion descriptor) is owed.

## Install

Built with [maturin](https://www.maturin.rs):

```
maturin build -m crates/sdk-python/Cargo.toml   # a cp39-abi3 wheel in target/wheels/
pip install target/wheels/slates-*.whl
```

or, for development, `maturin develop` into the active virtualenv.

## Connect

```python
import slates

# reply_ns / reconnect_ns are nanosecond deadlines; a production caller derives them from the
# machine's budgets. The instance is the daemon `slates anchor --instance <name>` published.
client = slates.Client.connect("default", 5_000_000, 10_000_000)
```

A connection is pinned to the thread that made it (the rings are single-consumer), so use one
`Client` per thread. Connecting where no daemon answers raises `slates.SlatesError`.

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

## No grants (R10)

There is no verb here that creates a landing grant — landing to a real disk is a human-only act at
the CLI or a confirmation surface, never something an agent answers for itself.
