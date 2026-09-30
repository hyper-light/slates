# The kqueue base watcher drained into a zero-length list (2026-09-30)

Contracts: §4.5 (the base plane's watcher hints), §4.15 (hints re-verify kept digests). Found by A-50's
fixture move: the watcher tests had been gated on `SLATES_TEST_RAMDIR`, which only the Linux lane set, so
the macOS/BSD watcher had never run under a test.

## Description

On macOS (and every non-Linux Unix), `OsHost::hints()` never returned a hint. A base directory changed by
an outsider (a file replaced by rename, a file added or removed) produced no `Hint::Changed`, while
`watch()` reported `WatchState::Live` and `slates status` showed the watcher live.

## Root cause

`crates/base/src/unix.rs` drained the kqueue with `kevent(queue, &[], &mut events, ZERO)`, `events` being
`Vec::with_capacity(DRAIN)`. rustix 1.x's `Buffer` for `&mut Vec<T>` hands the kernel the vector's
*length*, not its capacity (`parts_mut` returns `(as_mut_ptr(), len())`, rustix 1.1.4 `src/buffer.rs`),
and the length was zero. The kernel was asked for at most zero events and returned none. The SAFETY
comment's premise ("rustix sets its length from the count the kernel returns") was true only for the
`spare_capacity` form. The runtime's own drivers (`crates/rt/src/kqueue.rs`, `epoll.rs`) already used
`rustix::buffer::spare_capacity` and were unaffected.

## Impact

- **No proactive invalidation on macOS.** A hint marks a changed directory stale for every attached
  transport's kernel cache (`Overlay::take_stale_base_entries`), re-checks the witnessed entries homed
  there, and re-verifies kept digests. None of it happened.
- **Correctness held where a fingerprint could see the change.** A listing's directory fingerprint and a
  kept digest's file fingerprint are both compared on every use (`load_listing`, `digest_begin`). But a
  change inside one timestamp tick, which only the watcher reports, went unseen.
- **A false health claim.** The watcher state said `Live`.

## Exact edits

- `crates/base/src/unix.rs`: the drain passes `rustix::buffer::spare_capacity(&mut events)`, and the
  SAFETY comment states the real contract.
- Test first. `crates/base/tests/host.rs`'s two real-directory watcher tests now run on every host in the
  build output (A-50). On macOS they failed before the edit: "a hint for the root after the rename: []",
  still empty after ten drains 10 ms apart, and "the hint re-verified the kept digest: … hint_rechecked:
  0". Both pass after it.

## Sibling sweep

Every rustix call in the workspace that takes a buffer was checked: `kevent`, `epoll::wait`,
`io::read`, `net::recv`. Only this site passed an empty `&mut Vec`. The others pass initialized arrays
(`[0u8; N]`, `vec![0; n]`) or `spare_capacity`.
