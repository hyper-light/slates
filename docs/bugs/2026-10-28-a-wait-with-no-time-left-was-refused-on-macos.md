# A wait with no time left was refused on macOS (2026-10-28)

**Status:** fixed the same day, failing test first.

## Description

With a 1 ms reply deadline, a macOS client's `map_sync` failed with
`Ipc(OsRefused { call: "os_sync_wait_on_address", code: Some(22) })` (this Mac, macOS 26.4.1). The client had spun for
its whole deadline, then asked the wake word's wait for the time left, which was none.

## Root cause

`slates_ipc::wake::wait` takes a timeout of `Some(ns)`, and a zero means "has the word moved": Linux's futex answers
that at once (`ETIMEDOUT` when the word still holds the value, `EAGAIN` when not). macOS's
`os_sync_wait_on_address_with_timeout` refuses a zero timeout: "Passing zero for the @timeout_ns results in an error
being returned" and "EINVAL: The @timeout_ns passed is 0" (`os/os_sync_wait_on_address.h` in the macOS SDK). The macOS
branch passed the zero through, and turned the `EINVAL` into `OsRefused`.

## Impact

Any macOS client whose wait arrived with exactly no time left failed its call with `OsRefused` instead of timing out
and checking its daemon. With the default one-second deadline that needs the spin to end exactly at the deadline, which
is rare; with a short deadline it is likely. Linux and Windows were not affected (Windows waits on an Event in whole
milliseconds).

## Reproduction

`crates/ipc/tests/rings.rs` `a_wait_with_no_time_left_answers_at_once`: over a real region's wake word, a wait with no
time left for the word to leave the value it holds. Before the fix: `Err(OsRefused { call: "os_sync_wait_on_address",
code: Some(22) })`, not `Ok(false)` (this Mac, 2026-10-28).

## Exact edits

- `crates/ipc/src/wake.rs`, the macOS `wait`: a zero timeout reads the word and answers whether it moved, without
  asking the OS.
- The test above: the rings suite 9 of 9 on macOS; the client's one-millisecond deadline test 3 of 3 on macOS.
