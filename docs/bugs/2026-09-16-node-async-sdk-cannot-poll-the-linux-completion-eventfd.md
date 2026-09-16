# The Node async SDK cannot poll the Linux completion eventfd, so every slow-path await throws

Date: 2026-09-16
Area: `crates/ipc/src/completion.rs`, `crates/ipc/src/endpoint.rs` (the client completion channel);
surfaced by `crates/sdk-node/async.mjs`
Severity: on Linux the Node async SDK (`AsyncClient`) was dead — the first verb that took the slow
path (parked for its reply) threw `ERR_INVALID_FD_TYPE` and rejected the awaiting Promise. Every
concurrent workload hit it (a create that parks, the 8-way concurrent block). Python's async SDK was
unaffected, and the Node SDK was fine on macOS and Windows.

## Symptom

The `SDK packaging by use (ubuntu-latest)` CI lane ran the Node async suite over a live daemon and
failed only on Linux:

```
✖ async lifecycle over a live daemon (308.237885ms)
  TypeError [ERR_INVALID_FD_TYPE]: Unsupported fd type: UNKNOWN
      code: 'ERR_INVALID_FD_TYPE'
```

The Node *sync* suite passed, the Python *async* suite passed on the same runner, and the Node async
suite passed on macOS and Windows — so the fault was Linux-and-Node-specific.

## Root cause

On Linux the rendezvous hands the client the daemon's shared **completion eventfd** (§4.7,
`crates/ipc/src/rendezvous.rs`): the daemon writes it (adds one) when it places a reply for a parked
client, so an async event loop that polls it wakes without spinning. Python's `asyncio.add_reader`
registers the eventfd with epoll directly and polls it fine. The Node SDK, though, wraps the
descriptor in `new net.Socket({ fd })` (`crates/sdk-node/async.mjs` `_ensureReader`), and libuv's
`uv_guess_handle` classifies an eventfd as `UV_UNKNOWN_HANDLE` — it recognizes only sockets, pipes
and ttys — so Node's `net.Socket` constructor throws `ERR_INVALID_FD_TYPE: Unsupported fd type:
UNKNOWN`. An eventfd is pollable by epoll but not adoptable as a Node stream.

macOS and Windows never hit this because they pass no shared completion fd (Mach and named sockets
are refused, D-10): there the client already runs a `CompletionBridge` — a thread parking on the
region's wake word/Event that nudges a self-pipe (macOS) or loopback socket (Windows), both of which
`net.Socket` accepts. Linux had no bridge because the eventfd needed none for Python; the Node
limitation was the gap.

The async serve loop worked until it first had to await: a fast-path reply taken during the spin
never touched the reader, so the failure moved with scheduling (a create that completed in the spin
passed; the first one that parked, or the concurrent block, threw).

## Fix

Give Linux a `CompletionBridge` arm too, but only on the **dup** path an fd-adopting SDK uses
(`enable_async_completion_dup`, Node), leaving the plain path (`enable_async_completion`, Python)
returning the raw eventfd — Python keeps polling it directly, with no bridge and no thread. The Linux
bridge is a thread that polls the completion eventfd (a dup it owns) together with a stop pipe; when
the daemon makes the eventfd readable it drains it and writes a self-pipe whose read end the SDK
polls. Node's `net.Socket` accepts the pipe (the same primitive the macOS bridge already proved).

The bridge needs no armed-flag check of its own: `DaemonEnd::reply` writes the Linux completion
eventfd only under the `client_parked` check (`crates/ipc/src/endpoint.rs`), so the async fast path
never makes the eventfd readable and the bridge never nudges — the "fast path never wakes the loop"
property (§4.7) holds for free, unlike the macOS bridge which parks on a wake word that bumps on
every reply and must gate itself.

## Verification

- `crates/ipc` new test `the_linux_completion_dup_is_a_pollable_pipe_fed_by_the_eventfd`: the dup fd
  a Linux async SDK receives is a pipe (not the eventfd), stays quiet for a fast-path reply, becomes
  readable for a parked reply, and the bridge stops clean on drop.
- `crates/sdk-node/tests/sdk_async.test.mjs` over a live daemon on Linux: the full async lifecycle
  (create/snapshot/status/list/resize/destroy, the 8-way concurrent block, the merge and namespace
  loops) passes — reproduced in Docker with the addon built on `rust:1.98.0`.
- Python async unchanged (still the raw eventfd, no bridge); macOS/Windows Node unchanged.

## Sibling sweep

- The only fd-adopting SDK is Node; Python polls with `add_reader` and never adopts, so it is the one
  consumer that needed the pipe. No other caller of `enable_async_completion_dup` exists on Unix
  (grep).
- The Windows and macOS bridges already gave `net.Socket` a socket/pipe; Linux was the one platform
  handing it a raw eventfd. No other raw eventfd or non-socket fd is handed to an SDK event loop.
