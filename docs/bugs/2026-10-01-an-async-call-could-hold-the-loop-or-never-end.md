# An async call could hold its event loop, or never end

**Date:** 2026-10-01. **Area:** `slates-client` (async core, new `driver`), `slates-ipc` (rendezvous, new
`exit_watch`), both SDKs (`crates/sdk-node`, `crates/sdk-python`). **Audit:** AUD-29-19 (P1) and AUD-29-20
(P1), one change. **Design:** §4.7 status (A-56), §4.9, R6, banned item 9.

## Description

- **The loop was held (AUD-29-19).**
  - An async verb's "begin" sent synchronously. On a full command ring it spun up to the reply deadline,
    and on a lost channel it reconnected through a loop that slept.
  - The rendezvous waited for its answer inside one call: up to the one-second claim wait on macOS and
    Windows (two when the daemon had taken the claim), and on Linux a blocking `recvmsg` with no bound at
    all.
  - Both SDKs connected synchronously.
- **A call could never end (AUD-29-20).**
  - A pending call waited only for completion readiness. No deadline drove it, and nothing failed it on
    daemon loss, a reader failure or a cancellation.
  - Node dropped socket errors. Python left a cancelled future's entry until a reply that might never come.
- **A killed daemon read as alive on macOS and Windows.** Liveness compared the bootstrap object's start
  stamp, but the object outlives its daemon (a POSIX shared-memory name until unlinked; a section while any
  client maps it). A daemon killed with nothing to restart it was a stall, and recovery never began.

## Measured before the fix (2026-10-01, this machine)

- **Node acceptance test, first version:**
  - The suite hung until its 300 s bound.
  - A trace of the wrapper showed, after the anchor was killed, 31 of 32 calls settled and one pending
    forever: `nextWakeNs` constant at 1,000,000,000, `reconnects` 0, the timer re-armed every second for 53 s.
  - Logs then located the cause: a **queued** call (no request id yet) had no deadline. `tick` judged only
    sent calls, and `next_wake_ns` returned the reply deadline for a queue-only driver, so the timer re-armed
    forever. (One run of that trace used a stale addon: the addon build had failed and the copy step went
    ahead anyway. The failure was found by reading the build log, and every later run rebuilt first.)
- **With queued deadlines fixed:**
  - The loop's worst lateness was **1,002 ms**, against a 100 ms bound. Lateness logged per phase put it in
    the fresh `connect` of phase 4: the anchor had replaced the stopped daemon, and the synchronous connect
    waited out the claim wait (`CONNECT-FAIL … the daemon did not answer the claim within the claim wait`).
  - Every call to the `SIGKILL`ed daemon ended `Stalled { after_ns: 1000000000 }`, never `DaemonGone`: the
    stamp-only liveness.
- **Rust driver test, first run with queued deadlines:** a queued call failed `Stalled` after a successful
  restart, because its clock ran from submission while the resent calls refilled the new ring.

## Root cause

- The client's async primitives were the synchronous ones with a spin in front. Admission, reconnection and
  the rendezvous each waited inside a call.
- The SDKs held per-call state (futures, promises) but no per-call lifecycle. A deadline, a recovery and a
  failure had no owner.
- Liveness on the bootstrap platforms observed an object whose life is not the daemon's.

## Fix

- **Nothing in the client waits on the daemon.**
  - `begin` is one send attempt, typed `RingFull`, `ChannelLost` or `Rebinding`. A consumer's attest is sent
    without waiting, and its answer is read with the next reply.
  - The rendezvous is a claim made at once and polled: `slates_ipc::begin_connect_as` → `Claim::poll` on
    every platform, with Linux's socket non-blocking.
  - `connect_as` is the blocking facade over it: a poll, then a platform wait bounded by the claim wait.
  - A claim dropped unanswered gives its slot back.
  - `Client::try_reconnect` is one step of a claim it keeps between ticks. `Client::begin_connect` →
    `Connecting::poll` connects from a loop, paced as a reconnect is.
- **`slates_client::driver` owns every async call from submission to its end.**
  - A call is sent when the client admits it, and queued otherwise. The queue is bounded at the outstanding
    limit, and a call past it is refused `TooManyOutstanding` at once.
  - Every call has the reply deadline from when it was sent or submitted. An overdue call fails `Stalled`
    while the daemon lives, and starts recovery when it is gone.
  - Recovery reconnects one claim step per tick. It resends every sent call under its own id (its completion
    record answers one the daemon already served), restarts the queue's clocks, and past the reconnect
    budget fails every call `DaemonGone`.
  - A reader failure fails every call `CompletionLost`. A cancelled call is released: never sent if it was
    queued, and its reply dropped if it was in flight.
- **Both SDKs bind the driver.** The loop's completion reader pumps it, one timer ticks it at the wake it
  asks for, the reader moves to the new channel's descriptor on a reconnect, and connect polls on the timer.
  - **Python:** the reader and timer callbacks never raise (a raise there is only logged by asyncio). A
    failure fails every call instead. A cancelled task's future releases its call through a done callback.
  - **Node:** every verb goes through one `_call`. `client.cancel(promise)` releases the call and rejects it
    with an `AbortError`.
  - `AsyncClient.connect` is awaitable in both SDKs.
- **The client watches the daemon's process** (`slates_ipc::exit_watch`), from the moment its claim is
  answered. The daemon now writes its pid into the bootstrap header before publishing the header.
  - macOS: `EVFILT_PROC`/`NOTE_EXIT` on a kqueue of the client's own, bound to the process, not the pid.
  - Windows: a `SYNCHRONIZE` handle, so the pid is not reused while the handle is open.
  - A stopped daemon has not exited, so a stall stays a stall. Linux already read its control socket.
  - Five new `unsafe` sites, each with its invariant. The budget goes from 41 to 46 with the sites named.

## Tests

- `crates/client/tests/driver.rs`, `every_call_ends_and_the_loop_is_never_held_across_restart_and_death`
  (macOS, and Linux in Docker with io_uring available). A ticker loop times every step, connects from the
  loop, and drives:
  - an overflow: three bounds submitted, the excess refused at once;
  - a restart under a ring's worth of calls: every admitted call answered, with at least one reconnect as
    the non-vacuity counter;
  - a death: every call `DaemonGone`;
  - a cancel: nothing left outstanding.

  The longest step stays under a tenth of the reply deadline. It passed 3/3 here and 1/1 on Linux.
- `crates/ipc/tests/rendezvous.rs`, `a_claim_is_polled_without_waiting_and_refused_at_the_claim_wait`
  (macOS and Linux). Every poll returns under a tenth of the claim wait, the claim is polled more than once,
  the unanswered claim is refused no sooner than the claim wait, and an answered one connects. On Linux it
  runs the socket path, whose old connect had no bound.
- `rendezvous::platform::tests::a_claim_dropped_unanswered_gives_its_slot_back` (macOS/Windows).
- `exit_watch::tests::a_stopped_process_is_alive_and_a_killed_one_has_exited` (macOS), and the Windows twin.
- `crates/sdk-node/tests/sdk_async.test.mjs`, `every async call ends across restart, silence, reader loss
  and death`, over a real anchor and daemon:
  - `SIGSTOP` with 3× the limit submitted, then `SIGKILL`: the overflow refused, every admitted call
    answered after the anchor's restart;
  - anchor and daemon stopped: `Stalled`;
  - a cancel: `AbortError`, nothing waiting;
  - the completion socket destroyed: `CompletionLost`;
  - a fresh connect, then the anchor group killed: every call `DaemonGone`.

  The ticker is never late past 100 ms. It passed 6/6 here.
- `crates/sdk-python/tests/test_sdk_async.py`,
  `test_every_async_call_ends_across_restart_silence_cancellation_and_death`. It is the same history with a
  task cancel, and passed 4/4 here.
- The first acceptance runs above are the red evidence: the hang, the 1,002 ms hold, and `Stalled` for a
  killed daemon. The driver test's queued-clock failure was red before its fix. The rendezvous test could
  not be red against the old code, because the non-blocking API it drives did not exist. The old code's
  Linux connect had no bound at all, which is the reason the test exists.

## Siblings reported

- **The anchor's supervision races a "silent daemon" history.** It replaces a daemon that stops beating.
  The acceptance tests stop the anchor too when they mean a stall. A test that means "the daemon is slow"
  must do the same.
- **A doc bullet of A-53 had been split** by an earlier edit, with its tail stranded at the end of the
  amendment log. It is repaired in this change.
- **`slates-server`'s own client-liveness probe on Windows** (`peer.rs`) opens the client's process per
  probe. The exit watch's held handle is the pid-reuse-safe shape. Not changed here.
