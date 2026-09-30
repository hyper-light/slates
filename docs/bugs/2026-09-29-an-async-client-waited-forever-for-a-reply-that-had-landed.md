# An async client waited forever for a reply that had landed (2026-09-29)

## Description

CI's SDK packaging job on the macOS runner hung at `cead594` (run 36641516056). The step running the
Python suites against the installed wheel over a live daemon printed `bootstrapped` for
`test_async_lifecycle_over_a_live_daemon` at 22:47:34 and nothing more until the run was cancelled at
00:36:47, 1 h 49 min later. The job has no step timeout, so it would have held a macOS runner for
GitHub's 6 h default. The Ubuntu variant of the same job passed, as did the macOS job on the six later
runs.

The test's daemon runs one shard (`--shards 1`), so nothing is forwarded between shards. The destroy fix
of the same day, `afeed36`, does not bear on it: first suspected, that attribution is withdrawn.

## Reproduction

A runner script (scratch, not in the tree) ran the async lifecycle test one process at a time, bounded to
60 s, with `sample` taken of the test process and its daemon on a hang.

| Build | Runs | Hangs |
|---|---|---|
| `cead594`, serial | 40 | 0 |
| `cead594`, 8 workers × 40 at load 10–12 | 320 | 1 (worker 7, run 19) |
| HEAD before the fix, serial | 200 | 0 |
| HEAD before the fix, 8 workers × 40 at load 12–14 | 320 | 0 |

The hang's samples (3 s each):

- **The client.** The asyncio loop's main thread was in `kevent` on the completion fd. The
  `slates-completion` bridge thread was in `os_sync_wait_on_address_with_timeout` → `__ulock_wait2` on the
  region's wake word.
- **The daemon.** `slates-shard-0` was in `ShardContext::park` → `park_unless_pending` → `wait_in_driver`
  → `kevent`, with 2 of 2,522 samples in timer harvests. The `slates-doorbell` thread was in
  `__ulock_wait2` on the doorbell word.

Every party was waiting for a signal nobody would send.

## Root cause

Two defects in the reply half of §4.7's wake strategy.

**The park protocol had no fences.**

- The client raised `client_parked` (`Release`) and then re-checked its reply ring (`Acquire`): the sync
  `ClientEnd::wait`, and the async SDKs' pump after `arm_async`.
- The daemon pushed the reply, advanced the wake word, and read `client_parked` (`Acquire`).
- Each side writes one word and then reads another: the store-buffering shape. Without a `SeqCst` fence
  between the write and the read on both sides, both reads may miss. The client finds no reply and waits;
  the daemon finds no parked client and does not wake it.
- On Apple silicon, LLVM lowers an `Acquire` load to `LDAPR`, which may be satisfied before an earlier
  store-release to another address. On x86 the client's store sits in the store buffer.
- The request doorbell had the same shape and was fenced on 2026-09-28
  (`2026-09-28-a-client-request-waited-for-a-timer-after-a-lost-doorbell.md`). That fix's sibling sweep did
  not reach the reply side.

**The macOS and Windows bridges were edge-triggered.**

- Neither platform can share a descriptor, so a client-local thread makes a pipe (macOS) or socket
  (Windows) readable. It parked on the wake word or the region's Event, and on a change of the word it
  nudged only when `client_parked` was set.
- A change it saw while the client was not yet armed was recorded as seen and never acted on, which is the
  fast-path intent: a reply taken during the spin needs no nudge.
- When the client armed a moment later and its re-check missed the reply (the first defect), even a
  daemon wake found "no change" at the bridge. The bridge's 1 s timeouts found none either. Nothing would
  ever nudge the event loop again.

## Impact

An async SDK call (Python or Node, macOS or Windows) could wait forever for a reply already in its ring,
at a rate of about 1 in 320 contended lifecycles here. A sync client's or Linux eventfd's wait was exposed
to the first defect only. The kernel's value-checked wait on the word, and the sync wait's derived
deadline, bound those.

## Exact edits

- **`crates/ipc/src/park.rs`** (new) holds the protocol's three pieces:
  - `after_arming`: the client's fence.
  - `after_publishing_armed`: the daemon's fence, then its read of the flag.
  - `reply_waiting_for_armed`: a bridge's level — a fence, then "armed and a reply waits".

  The loom model drives them with a client, a daemon and a bridge, and keeps the old protocol as a witness
  that must deadlock.
- **`crates/ipc/src/endpoint.rs`.**
  - `arm_async` and the sync `wait` fence right after raising the flag.
  - `DaemonEnd::reply` reads the flag through `after_publishing_armed`. When the client is armed, it
    advances the word once more before waking, so a wake that lands while a bridge is between two waits
    reaches its next wait, which is checked against the word's value.
- **`crates/ipc/src/completion.rs`.** The macOS and Windows bridge threads nudge after every return from
  their wait while the client is armed and the completion ring's depth is non-zero
  (`armed_with_reply_waiting`, read with `Acquire`). The Linux bridge is unchanged: it forwards an eventfd
  the daemon writes only for an armed client, so the fences close its race.

## Evidence

- **Loom.**
  - `park::loom_tests::the_edge_triggered_unfenced_protocol_loses_a_wake` deadlocks at interleaving 1: the
    witness.
  - `park::loom_tests::a_reply_published_while_an_async_client_parks_is_never_lost` explores 42,826
    interleavings at the CI preemption bound with no loss. Its non-vacuity counts show replies taken in the
    spin, found on the re-check and brought by the bridge's nudge.
  - CI's loom command (`RUSTFLAGS="--cfg loom" cargo test -p slates-mem -p slates-rt -p slates-ipc --lib
    --release loom`) passes whole.
- **By use.** With the fix: 8 workers × 100 async lifecycles at load 6–11, 800 passed, 0 hung. The stress
  alone cannot prove the fix at a 1-in-320 base rate. The loom pair and the sampled stacks, which match the
  modelled lost wake, carry the claim.
- **Suites.** slates-ipc (18 + 2 + 5 + 8 + 3); slates-client (all); the Python SDK suite (5, sync and async)
  and the Node SDK suites (5, sync and async) over a live daemon on macOS. Clippy clean on macOS, Linux
  (Docker, with the ipc tests) and Windows (Docker cross-lint of ipc, client and server). xtask check ok.

## Siblings found

- **The SDK job has no step timeout.** A hang holds a macOS runner for 6 h, and the cancelled run was
  blocking the queue. A per-step timeout would need a derived bound; it is not added here.
- **The Python pump leaves a reply for a word with no pending future in the client's buffer** (the `continue`
  in `pump`). It is bounded by the requests the client made, but it is never released.
