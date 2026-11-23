# The Node SDK could not adopt the Windows completion socket, so every slow async call failed on Windows (2026-11-23)

## Description
CI run 45176536354, the `sdk` job's `windows-latest` and `windows-11-arm` legs (logs 137699123771 and 137699124130):
`async lifecycle over a live daemon` (`crates/sdk-node/tests/sdk_async.test.mjs:76`) failed at `AsyncClient.create`:

```
Error: CompletionLost { reason: "Unsupported fd type: UNKNOWN" }
    at AsyncClient._settle (async.mjs:285:22)
    at AsyncClient._lose (async.mjs:271:10)
    at AsyncClient._guard (async.mjs:257:12)
    at async.mjs:212:12
    at AsyncClient._await (async.mjs:210:12)
    at AsyncClient._call (async.mjs:200:26)
    at AsyncClient.create (async.mjs:67:17)
```

## Root cause
On Windows `completionFd()` returned a duplicate of the completion bridge's loopback socket (`SOCKET`), and
`async.mjs` wrapped it with `new net.Socket({ fd })`. Node's `createHandle` (`lib/net.js`) asks libuv what the value
is, and on Windows `uv_guess_handle` (`src/win/handle.c`) reads it as a C runtime file descriptor
(`uv__get_osfhandle`) and answers by `GetFileType`: a pipe, a console or a file, never a socket. A socket handle is no
C runtime descriptor, so it is `UV_UNKNOWN_HANDLE`, and Node throws `ERR_INVALID_FD_TYPE`, "Unsupported fd type:
UNKNOWN". The throw came inside `_ensureReader`, which `_guard` turned into the reader lost. The addon's doc had claimed
that "Node and libuv accept a `SOCKET` there"; they never did. The Windows suites first ran on 2026-11-18, so this was
the first run to reach the async lifecycle.

## Impact
On Windows every Node `AsyncClient` call that did not finish within its spin failed with `CompletionLost`, and the
client refused every call after it. The synchronous Node client and Python's async client (which polls the socket with
a selector) were not affected, and nothing was written wrongly.

## Edits
- `crates/ipc/src/completion_pipe.rs` (new): the completion pipe. It has a fresh name, a single first instance,
  rejects remote clients, admits the owner alone, and holds one nudge. Its `Listener` admits a reader only when the
  kernel names it this process, settling a connect without waiting and starting at most one connect a turn.
- `crates/ipc/src/completion.rs`: the Windows bridge signals a socket or a pipe (`Signal`); `start_pipe` and
  `run_pipe`, which waits beside the connect until a reader is admitted.
- `crates/ipc/src/endpoint.rs`: `enable_async_completion_pipe` replaces the Windows `enable_async_completion_dup`;
  three Windows tests, one with a reader from another process (the test binary run again).
- `crates/ipc/src/stream_pipe.rs`: `fresh_name(prefix)`, `OwnerOnly`, `last_error` and `os` shared with the crate.
- `crates/client/src/client.rs`: `enable_async_completion_pipe` on Windows.
- `crates/sdk-node/src/lib.rs`: `completionPipe()` beside `completionFd()`, each on every platform and refusing where
  its signal is not the platform's. `async.mjs` opens the pipe with `net.connect` on Windows. The suites name both.
  A first version compiled `completion_fd` out on Windows, and the addon's Windows build failed `cannot find value
  __napi__completion_fd` (diag run 45226402571). napi-rs registers a `#[napi] impl`'s methods by name whatever their
  `cfg`; before A-875 the two cfg'd arms shared that one name.
- `unsafe-budget.toml` (`slates-ipc` 169 → 179), `docs/wip/SLATES_DESIGN.md` (A-875), GAPS.

## Evidence
- Before: CI run 45176536354, as quoted, on both Windows legs. Reproduced by main's diag run 45225292920 on
  windows-latest at main.
- After (diag run 45230459274, windows-latest, 20cd9025e):
  - `slates-ipc`'s Windows completion and pipe tests pass, 8 of 8:
    `the_windows_completion_pipe_is_opened_by_name_and_signals_only_an_armed_reply`,
    `a_reply_that_landed_before_the_reader_connected_is_signalled_as_it_connects`,
    `a_reader_of_another_process_is_cut_off_and_this_processs_reader_is_admitted`, and the stream tests.
  - The addon builds, and "Unsupported fd type" is gone: the async lifecycle reaches the daemon's first create.
  - 4 of the 8 Node tests pass, among them "the addon exposes the async low-level primitives".
- Not yet: the other 4 Node tests, sync and async alike, are refused `BudgetExceeded { available: 5722144768 }` at
  their first create. That is the Windows daemon's own sizing (w-win2's), not this change, and the suites' full green
  on Windows waits on its fix.
- Main cross-linted `slates-ipc` for both Windows targets; the one finding, `run_pipe`'s complexity, is split.
- On macOS, unchanged: `slates-ipc --lib completion` 3 of 3, `--test completion_memory` 1 of 1, and the addon's
  method-list test against a built addon.
