# The FUSE notify round's wake was built and never wired

- Date: 2026-10-20
- Area: §4.6 bridges, cache posture; the fix recorded in
  `docs/bugs/2026-10-17-fuse-notify-deadlocks-the-serving-thread.md` (landed `b9935dfe`)
- Status: open — root cause confirmed, fix not attempted here (see "Why not fixed in this change")
- Found by: sweeping `crates/bridge-fuse` for public functions nothing calls.

## Description

`b9935dfe` moved the FUSE invalidation write off the serving thread to break a mount deadlock.
The round runs on a `NotifyThread`, and its outcome reaches the serve loop out of band. The
design for that out-of-band signal was built: `ServeState::wakes_with` stores a `ChangeNotifier`,
`deliver_owed` clones it into `NotifyThread::start`, and the thread fires it the moment the round
is sent:

```rust
// crates/bridge-fuse/src/channel.rs:318
if send.send(round).is_ok()
  && let Some(wake) = wake
{
  let _ = wake.notify();
}
```

**`wakes_with` has no caller.** Its definition at `channel.rs:564` is the only occurrence in the
workspace. So `state.notify_wake` is always `None`, `NotifyThread::start` always receives
`wake: None`, and the branch above never fires.

## What it costs

Not the first round's delivery: the thread writes the invalidation messages to the device itself,
so those reach the kernel whether or not anyone is woken. What is lost is the loop learning that
the round finished.

The serve task of a mount is (`crates/server/src/fuse.rs:467`):

```rust
loop {
  readiness::readable(raw).await;      // only the device wakes this
  loop { turn(...) until Turned::Idle }
}
```

A completed round does not make the device readable, so the loop does not run again until the
kernel sends the next request. Until it does:

- `state.notifying` stays `Some`, and `deliver_owed` starts no new round while one is in flight
  (deliberately — `Bridge::invalidations` is not idempotent, so at most one batch is out).
- Every invalidation raised after the in-flight round therefore waits for the next kernel request.

On a busy mount that is microseconds. On a quiet one it is unbounded: a change made while a round
is in flight is not pushed to the kernel until something else touches the mount, and a kernel
serving a cached entry sends no request — which is the exact condition entry invalidation exists
to break.

Nothing is dropped: the batch is computed only when the slot is free, and the base-plane half it
consumed is held in the round, so this is a staleness and latency defect, not a loss.

## Why no test caught it

The unit test the earlier fix added asserts the owner is not blocked and that `returned()` is
`None` while the write is stuck — the deadlock property. No test asserts the loop is *woken*, and
no test drives a second round behind a first.

## Why not fixed in this change

The one-line reading — call `wakes_with` in `serve_blocking`, which already has a `ChangeSignal`
to make a notifier from — fixes only the standalone path and not the daemon, which has no
`ChangeSignal` anywhere in `crates/server/src/fuse.rs` and waits on device readability instead.
Fixing the daemon means its serve task must await *either* the device or the round's completion,
which is a change to the task's wait, not a missing call. Wiring only the easy half would leave
two paths with different behaviour — the layering CLAUDE.md #7 forbids — so the whole thing wants
doing at once, with a test that drives a second round behind a first on a mount receiving no
requests.

## Why no lint could have caught it either

`channel.rs` is `#[cfg(target_os = "linux")]` (`lib.rs:28`), so this laptop never type-checks it
at all — `cargo check -p slates-bridge-fuse` passes on macOS without compiling the file. And on
Linux `dead_code` says nothing, because `wakes_with` is `pub` in a library crate and a `pub` item
is reachable by definition. A setter that no one calls, in a module half the fleet never
compiles, is invisible to every tool the project runs. The sweep found it because it asks a
question no lint asks: not "is this reachable?" but "does anything actually reach it?"

## The shape a fix should probably take

Racing two readiness futures in the serve task is the obvious reading and the worse one: it means
registering the notifier's fd with the driver per mount and hand-rolling a select, in the loop
whose last bug deadlocked mounts.

The runtime already has the right mechanism, and it is now tested. A **poller**
(`ShardContext::register_poller`) wakes a task whenever its predicate says ready, which is exactly
how the daemon's own control loops are driven (`daemon.rs:4031`, `:4189`). Registering one per
mount whose predicate reports "this mount's round has returned" makes a completed round wake the
serve task through a path already in use, with no new wait and no second fd. It also cleans
itself up: a poller is dropped when its task ends, which
`crates/rt/tests/pollers.rs::a_poller_is_forgotten_when_its_task_ends` now holds.

One trap to design around, named in the project's own record: a poller predicate runs inside the
shard loop, so it must not take a `with_state` borrow that something up the stack already holds —
a nested borrow returns `None` silently rather than refusing.

## Exact edits a fix would make

1. `crates/server/src/fuse.rs::serve` — wait on device readability **or** the mount's round
   completion, and give the mount's `ServeState` a notifier for the latter.
2. `crates/bridge-fuse/src/channel.rs::serve_blocking` — `state.wakes_with(signal.notifier()?)`
   when a signal was supplied, so both drivers use the same mechanism.
3. A test that makes a change, lets a round go out, makes a second change, and asserts the second
   is delivered with no further kernel request — red before, green after.
