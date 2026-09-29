# The delivery test read a reused handle value as the parent's decoy

Date: 2026-09-28. Scope: `crates/ipc/tests/delivery.rs`, the cross-process test of the consumer-capability
delivery (§4.13, GAP-A9-9). Severity: test-only — an intermittent false failure; the sibling it exposed in
the production take is recorded below and fixed in its own change.

## Symptom

CI run 36500813478 (`7ac1942`, which changed no ipc code), job 109190947906 "windows (nightly cadence)",
2026-09-29T00:00:54Z:

```
test a_consumer_child_takes_the_capability_from_the_one_inherited_descriptor_and_nothing_else ... FAILED
assertion `left == right` failed: the consumer child took its delivery (… 13 the decoy came along …)
  left: Some(13)
 right: Some(0)
```

The same test passed in the nightly Windows job of the four neighbouring runs on the same ipc code
(36495103302, 36498699200, 36500799020, 36502844343): one failure in five.

## Root cause

The parent holds a decoy — an inheritable handle on Windows, a close-on-exec pipe end on Unix — that the
child must not inherit, and passed the child only its **number**. The child asked whether a handle was open
at that number (`GetHandleInformation`; `fstat` on Unix). Handle values and descriptor numbers are indexes
into each process's own table: an inherited handle keeps its value (Microsoft, "Handle Inheritance": "It
also has the same value and access privileges"), but the child's own handles fill the same small values, so
"open at that number" is true whenever one of the child's own handles landed there. The check could not tell
"the decoy came along" from "a handle of mine has that number".

What the log does not show: the child printed nothing identifying the object at that value, so the reuse is
the cause consistent with the evidence (an intermittent failure on unchanged code, a check that accepts any
handle), not one a log line proves. The new check makes a recurrence self-diagnosing.

## Fix

The parent passes the decoy's **identity**, and the child confirms the object at that number is that very
object:

- Windows: the decoy is an inheritable handle to a uniquely named event (`Local\slates-delivery-decoy-<pid>-
  <nanos>`); the child opens the event by name and compares it with whatever sits at the decoy's value
  (`CompareObjectHandles`, Windows 10 1607 and later).
- Unix: the decoy is a pipe the parent keeps open for the whole test; the child compares the device and
  inode of the descriptor at that number with the decoy's. A live pipe's pair is unique (POSIX `fstat`);
  a dead pipe's is not on macOS (measured below), which is why the decoy stays open throughout.

A true inheritance still fails the test (exit 13), now with the decoy's identity on stderr.

## Measured (the sibling's evidence)

`pipe_identity.py` (scratchpad), 2,000 rounds of `pipe` / `fstat` / `close` each, 2026-09-28:

| Host | new pipes whose `(st_dev, st_ino)` matched a dead pipe | at the dead pipe's own number |
|---|---|---|
| macOS 26.4 (Darwin 25.4.0), this box | 1,999 of 2,000 (`st_dev` is 0 for every pipe) | 1,999 |
| Linux 6.12.76-linuxkit (Docker Desktop, `python:3-slim`) | 0 of 2,000 | 0 |

In the one-shot stale case (a pipe written, closed, then a fresh pipe at the same number), macOS gave the
fresh pipe the dead one's exact `(st_dev, st_ino)` with an `st_mtime` 2,459 ns later; Linux gave it the next
inode with the **same** coarse `st_mtime`.

## Sibling: the production take trusts a number (open; fixed in its own change)

`delivery::take_named` does what the test did. It adopts the descriptor at the number `SLATES_CONSUMER_FD`
names once it is an open pipe or socket (Windows: an open pipe), then clears its close-on-exec or inherit
flag, sets `O_NONBLOCK` on its open file description, reads up to 49 bytes from it, and closes it. The
variable is inherited by every process a consumer starts, and the descriptor is not (it is closed at the
take, and many spawners close everything but the standard three, for example Python's `subprocess`
default). A consumer's subprocess that opens a slates client — the CLI, an SDK script, the MCP server —
therefore adopts **its own** pipe or socket at that number, eats its bytes, flips its blocking mode for
every holder, and closes it under its owner, before the take is refused. This is the same number-reuse
fact that failed CI run 36201084174 ("IO Safety violation: owned file descriptor already closed") and the
2026-09-14 unit-test pair (`docs/bugs/2026-09-14-delivery-test-reuses-a-closed-descriptor-number.md`).
Both times the tests were changed and the take was not. The measurement above rules out `(st_dev, st_ino)`
alone as the identity on macOS.
