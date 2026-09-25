# A delivery test took a descriptor number it had already closed

Date: 2026-09-25. Scope: a unit test in `crates/ipc/src/delivery.rs` (§4.13 consumer-capability
delivery), not the product. Found by CI run 36201084174 (`ee823e0`, Linux gates).

## Symptom

`cargo test -p slates-ipc --lib` aborted on CI:

```
fatal runtime error: IO Safety violation: owned file descriptor already closed, aborting
error: test failed, to rerun pass `-p slates-ipc --lib`
```

The delivery tests had just run beside it. The same crate's tests passed on the three runs before it.

## Root cause

`a_whole_record_takes_once_and_the_descriptor_is_closed` took a pipe's read end by number. The take
adopts the descriptor and closes it, as it must. The test then called `take_named` on the same number
again to show it was closed.

Tests run on parallel threads, and the kernel hands the lowest free number to the next open. So by the
second take, another test's pipe or socket can hold that number. The take accepts any pipe or socket
(`adopt` checks the kind with `fstat`), adopts it and closes it. When the other test drops its
`OwnedFd`, the descriptor is closed a second time, and the runtime aborts the whole test binary.

The sibling test in the same module met the same reuse on 2026-09-14 (its comment records it, and the
two failed together). That fix moved the sibling to a number no process reaches, but left this test's
second take in place.

## Fix

The test proves the close on the pipe itself. It keeps the pipe's write end, takes the whole record,
and expects the next write to fail with `EPIPE`: the pipe has no reader left. The Rust runtime ignores
`SIGPIPE`, so the failure is an error return, not a signal. A take that did not close the descriptor
would leave the write succeeding, so the assertion cannot pass vacuously.

## Evidence and limits

- After the fix, the crate's unit tests passed 30 of 30 on Linux with 16 test threads.
- The old test also passed 30 of 30 locally. The race window is the few microseconds between the
  first take's close and the second take's check, and it did not reproduce here. The CI abort is the
  direct evidence; the unsoundness holds by construction.
- Sweep of every descriptor adopted by number across the tree:
  - The segment handoff duplicates the inherited number and never adopts it (2026-09-14).
  - The completion bridge moves each descriptor into its thread as an `OwnedFd`.
  - The async completion duplicate has one owner.
  - The FUSE handshake adopts its own standard input in a child process.
  - The anchor passes its listener to the supervisor by move.

  None repeats the pattern. The anchor's Linux-only abort of the same kind
  (`slates-anchor --test anchor`) is not explained by this and stays open.

## Edits

- `crates/ipc/src/delivery.rs`: the test proves the close by `EPIPE`.
- `docs/wip/TBD_FIXES.md`.
