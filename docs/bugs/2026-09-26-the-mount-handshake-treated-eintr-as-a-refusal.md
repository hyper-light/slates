# The FUSE mount handshake treated an interrupted receive as a refusal

Date: 2026-09-26. Contracts: AC-3.12/T-3.15 (the mount descriptor handshake), Part 2 item 9.
Found by CI run 36276675391 (`a9f703e`, Linux gates):
`a_helper_that_never_answers_is_killed_at_the_derived_deadline_and_reaped` panicked, with an
assertion that did not say what it got.

## Symptom and reproduction

- The test's trivial-helper probe (`sh -c true`) expected `NoDevice`. Once the assertion printed its
  outcome, a Linux container (Docker, 4 CPUs) reproduced the failure in 1 of 40 runs of the test
  binary: `Err(Recv { code: Some(4) }) after 424.417µs`.
- Code 4 is `EINTR`.

## Root cause

- `receive_device`'s `recvmsg` mapped every error except `EAGAIN` to a refusal. `EINTR` (a signal
  delivered to the receiving thread; here the test binary's other threads are spawning and reaping
  helpers) means "retry", not "refused".
- The same code created the socket pair, and received the device descriptor, without close-on-exec,
  setting it with `fcntl` afterwards. A helper another thread spawned inside that window inherited
  the socket, which could hold it open past this helper's exit. That hazard is found by reading the
  code, not shown by the reproduction.

## Fix

- `receive_within` retries an `EINTR` receive with the socket timeout re-armed to the time left. An
  interruption is neither a refusal nor an extension of the deadline.
- On Linux the pair is created with `SOCK_CLOEXEC` and the received descriptor arrives with
  `MSG_CMSG_CLOEXEC`, closing the inheritance window atomically. macOS has neither flag and keeps the
  `fcntl`; FUSE mounts are Linux-only in use.
- The test's assertion now reports the outcome and elapsed time.

## Evidence

After the fix, 0 of 200 Linux runs failed, against 1 of 40 before.

## Edits

`crates/bridge-fuse/src/mount.rs`, `crates/bridge-fuse/tests/handshake.rs`, this record.
