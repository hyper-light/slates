# The async signal took the synchronous form's wakes (2026-11-18)

## Description

A client whose async completion signal was running, and which then made a synchronous call that parked for its
reply, often slept until the call's reply deadline. It found the reply only when the park timed out. Measured on the
Mac Studio (macOS arm64, 2026-11-18, kiln run on agents/g262-async b2f46e48b): after `AsyncInstance` handed its
instance back, 20 synchronous status calls each parked at once (`spin_for(Some(0))`). Over five runs, 16 to 19 of the
20 took over 500 ms, the slowest 2.0 s each time: the client's reply deadline (the liveness budget).

## Root cause

On macOS the async completion signal is a bridge thread (`slates_ipc::completion::CompletionBridge`). It parks on the
region's wake word, the same word a parked synchronous call waits on. On Windows both wait on the region's named
auto-reset Event. On a reply to a parked client the daemon wakes one waiter (`wake::wake_one`, a ulock or futex wake
of one; an auto-reset Event releases one). With the bridge running there are two waiters, and the kernel picked
either. When it picked the bridge, the parked synchronous call slept on until its timeout, the remaining reply
deadline. Then it re-checked the ring and took the reply that had been there all along. Linux's plain path has no
bridge (the daemon writes the eventfd directly), so it was not affected there.

## Impact

`AsyncInstance::into_instance` (not yet on main when found) handed back an instance whose synchronous calls each
took up to the reply deadline whenever they parked. The same hazard exists on main for the Node SDK's `Client`: its
`completionFd()` starts the bridge (`enable_async_completion_dup`), and its synchronous methods (`create`,
`snapshot`, `status`, `drifted`) park on the same client. A Node program that used the async calls and then a
synchronous one would see the same stalls on macOS and Windows (reported, not changed here). The Python SDK's
`AsyncClient` makes no parking call, so it is not affected.

## Exact edits

- `crates/ipc/src/endpoint.rs`: `ClientEnd::disable_async_completion` stops and joins the bridge (closing its
  descriptor) and clears the arm.
- `crates/client/src/client.rs`: `Client::disable_async_completion`.
- `crates/api/src/asynchronous.rs`: `AsyncInstance::into_instance` stops the signal before it hands the instance back
  (fallible: refused as the client refuses clearing its arm).
- `crates/api/tests/api.rs`: `expect_parked_synchronous_calls_woken` inside
  `an_async_instance_provisions_snapshots_reports_and_destroys`. It makes 20 synchronous calls that each park, each
  required under the reply deadline, with the client's park count as the non-vacuity counter. It fails with the
  bridge left running.

## At the source, the same day (second change)

`into_instance` stopping the bridge fixed one route; any client that keeps its bridge and parks synchronously had the
same stall. The Node SDK's `Client` is one: its `completionFd()` starts the bridge, and its synchronous verbs park on
the same client. The fix is now at the source (`crates/ipc/src/completion.rs`, `crates/ipc/src/endpoint.rs`):
- A synchronous park raises a mark (`CompletionBridge::sync_parked`, a `SyncPark` guard in `ClientEnd::wait`) before
  it raises the parked flag.
- A bridge woken while the mark stands wakes once more: `wake_one` on macOS, `wake_signal` on Windows. The parked call
  is then the only waiter left.
- `a_synchronous_park_beside_a_running_bridge_is_woken_by_its_reply` sends 20 calls that each park while the bridge
  runs. All are answered within half their 500 ms deadline. Without the pass-on, round 0 took 502 ms (Mac Studio).
- The Node SDK binds `spinFor` and `parkRatio`. Its by-use test, a synchronous call beside the async completion signal,
  runs in `crates/sdk-node/tests/sdk.test.mjs` and is filed for main's run.

Found on the way: `CompletionBridge::start` dropped a refused thread spawn (`.spawn(..).ok()`, three sites), a
swallowed error (CLAUDE.md §2 item 9). The bridge then started with no thread, and every async wait ran to its
deadline. The start is now refused typed (`IpcError::OsRefused`, "spawn the completion bridge's thread"), through a
`Spawn` seam the test refuses with `EAGAIN` (`a_bridge_whose_thread_the_os_refuses_is_refused_typed`, red with the
refusal bypassed).
