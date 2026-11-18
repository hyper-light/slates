# The completion bridge kept two blocks every start (2026-11-18)

## Description

A client that turned its async completion signal on and off again and again kept two heap blocks for good every time
it turned it on, on macOS and Windows. Measured on the Mac Studio (macOS arm64, 2026-11-18, kiln run r104 on
`agents/g262-bridge-red` 1d7f46a05): `a_client_turning_its_async_completion_on_and_off_keeps_no_memory` turned a
client's completion on and off 64 times after four warm-up turns and counted the blocks the turning thread made that
nobody freed: 128 kept, two a turn.

## Root cause

The macOS and Windows bridge (`slates_ipc::completion::CompletionBridge`) is a thread that waits where the daemon
wakes the client: the region's wake word on macOS, its named Event on Windows. The thread needs a `'static` view of
what it shares with its owner, and slates has no `Arc` (R2). Each start made two `&'static AtomicBool`s with
`Box::leak`: the thread's stop flag, and the mark a synchronous park raised so the bridge passed on a wake it took
from that park (the second change of
`docs/bugs/2026-11-18-the-async-signal-took-the-synchronous-forms-wakes.md`). A stop joined the thread but freed
neither: leaked by construction, so freeing one would have been unsound while anything held the reference.

## Impact

Unbounded growth (CLAUDE.md §2 item 8), small per turn and without end: every `AsyncInstance::new` and
`into_instance` pair of a host on its own event loop, every Node `Client` whose `completionFd()` starts a bridge, and
every `enable_async_completion` after a `disable_async_completion` kept two more blocks for the life of the process.
A host that makes a client per call and awaits it (kiln's blocking pool, moved onto the async form) would grow a little
with every call. Linux was not affected: its bridge stops through a pipe and needs no shared flag.

## Exact edits

- `crates/ipc/src/completion.rs`: the stop is a `std::sync::mpsc` channel. The owner holds the sender and drops it to
  stop the thread, then wakes it; the thread finds its receiver disconnected (`stopped`). The channel is freed with the
  last of its two ends. The pass-on reads the client's parked flag instead of a mark (`synchronous_park_waiting`), so
  `CompletionBridge::sync_parked` is gone. Both arms, macOS and Windows.
- `crates/ipc/src/region.rs`: `PARKED_ASYNC` (3), the async arm's own value of the parked flag. The daemon reads it as
  a park that stamps slot 0, as it reads every armed value but slot one's, so it changes nothing on the daemon's side.
- `crates/ipc/src/park.rs`: `parks_synchronously`, whether the flag's value is a synchronous park (either stamp slot's
  value, never the async arm's nor zero), with its unit test.
- `crates/ipc/src/endpoint.rs`: `ClientEnd::arm_async` stores `PARKED_ASYNC`; the `SyncPark` guard in `ClientEnd::wait`
  is gone, since the park's own flag is the mark.
- `crates/ipc/tests/completion_memory.rs`: the failing test, in a binary of its own on an allocator that marks each
  block with whether the measuring thread made it, so a block is counted until it is freed on whichever thread frees
  it. Red before the change (128 kept over 64 turns); `a_synchronous_park_beside_a_running_bridge_is_woken_by_its_reply`
  holds the pass-on through the new mark.
