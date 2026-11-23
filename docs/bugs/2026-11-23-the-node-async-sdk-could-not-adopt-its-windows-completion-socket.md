# The Node async SDK could not adopt its Windows completion socket (2026-11-23)

## Description
On both Windows legs of CI's SDK lane (run 45176536354: job 137699123771 on windows-latest, job 137699124130 on
windows-11-arm, Node v24.22.3), `crates/sdk-node/tests/sdk_async.test.mjs`'s "async lifecycle over a live daemon"
failed at its first verb with `CompletionLost { reason: "Unsupported fd type: UNKNOWN" }`, from `AsyncClient._settle`
through `_lose` and `_guard` while `AsyncClient.create` awaited its reply. Every async verb on Windows fails so the
first time its reply is not ready at once.

## Root cause
`async.mjs` wraps the completion signal in `new net.Socket({ fd })`, and on Windows the addon's `completionFd()`
returned the raw handle of the completion bridge's loopback socket (a `SOCKET`, `CompletionBridge::dup_socket`). Node
asks libuv's `uv_guess_handle` what the number is, and on Windows libuv takes it for a C runtime file descriptor and
looks up its handle with `_get_osfhandle`. A socket's handle value is no descriptor in that table, so libuv answers
`UV_UNKNOWN_HANDLE`, and `net.Socket` throws `ERR_INVALID_FD_TYPE`, "Unsupported fd type: UNKNOWN" (the error CI
read). The descriptor table is the C runtime's of `node.exe`, so no descriptor the addon could make would be found
there either. The addon's doc said "Node and libuv accept a `SOCKET` there"; the Windows legs that would have shown it
otherwise were added on 2026-11-18 (4ae973864) without a run on Windows. The libuv and Node details are read from
their sources, not measured here.

## Impact
The Node SDK's async surface could not settle a call on Windows unless its reply came within the spin, so the SDK's
Windows legs were red and every async Node program on Windows failed its first parked call.

## Exact edits
- `crates/ipc/src/stream_pipe.rs`: `listener`, a pipe this process's reader opens by name, made as a pair is (a fresh
  name, first instance, one instance, no remote client, the owner alone) and outbound only; `accept_own`, which holds
  the reader only when it is this process (`GetNamedPipeClientProcessId`) and sets the end never to wait; `knock`,
  which returns that wait at a stop; `write_nowait`. The pipe's creation, client open and mode are the pair's sites,
  moved into `server_end`, `client_end` and `set_mode`.
- `crates/ipc/src/completion.rs`: the Windows bridge serves either form (`Polled`): the loopback socket a selector
  polls (Python), or a pipe a loop opens by its name (Node), the same nudge loop writing either
  (`nudge_while_armed`). `dup_socket` is gone with its only caller.
- `crates/ipc/src/endpoint.rs`, `crates/client/src/client.rs`: `enable_async_completion_pipe` replaces the Windows
  `enable_async_completion_dup`.
- `crates/sdk-node/src/lib.rs`: on Windows `completionPipe()`, the pipe's name, replaces `completionFd()`; each
  platform keeps a refusing twin of the other's method, since napi registers every method of the block everywhere.
  `async.mjs` opens the pipe with `net.connect({ path })` on Windows; the tests name the platform's method.
- `unsafe-budget.toml`: slates-ipc 169 -> 171 (four sites, from a count of 167).
