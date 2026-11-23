# A full macOS hold channel refused holds the anchor would have taken (2026-11-22)

## Description
`crates/anchor/tests/anchor.rs`, `a_table_past_the_old_exec_limit_reaches_the_successor_whole` (A-462), fails at
origin/main on the MacBook (w-perf's find while landing 80412e446):

    the anchor held every device the dead daemon sent: left Some(13), right Some(7792)

GAPS recorded nothing about it.

## Where the other 7,779 went
They were never sent. The first daemon (the test's child) panicked at its 14th hold:

    held::send(...).unwrap(): Os { call: "sendmsg", code: 55 }

That is `ENOBUFS`. Nothing was refused by the anchor (`device_refusals` stays 0) and nothing was lost at the handoff:
the anchor held exactly the 13 devices it received and handed them on.

## Root cause
On macOS the hold channel is a connected Unix datagram socketpair (`held::channel`; macOS has no `SOCK_SEQPACKET`).
- When the receiver's buffer is full, a macOS Unix datagram send does not block: `sendmsg` refuses with `ENOBUFS`.
  `net.local.dgram.recvspace` is 4,096 bytes here. With its address and `SCM_RIGHTS`, each 74-byte message takes far
  more than 74 bytes of that accounting, so about 26 messages fit, the 13 devices' hold and session pairs.
- There is no readiness to wait on, because the sender's own buffer is not the one that is full.
- Linux's sequenced packets block in the kernel, which is the behaviour every caller was written for. `held::send`
  passed the refusal back. The daemon's `anchor_hold::send` reads any refusal as "the anchor's end is gone" (its doc
  said the channel is blocking), so its callers neither retry a refused hold nor a refused release.
- The test's child sends 7,792 holds and sessions in a tight loop while the harness drains the anchor's end every
  500 µs. So the channel fills after 26 messages.

It is not the descriptor work since 2026-11-21:
- A-832, A-833 and A-838 change the vfs's base-file and base-directory descriptors; the hold channel is untouched.
- 417c3394e raises the soft `RLIMIT_NOFILE`; the test's `descriptor_room` raises its own limit as before, and still
  asks for what the anchor needs (the devices plus `FULL_SLACK`).
- The test's premise did not change by design.

## Impact
On macOS, any burst of holds faster than the anchor drains its end lost every hold past the channel's room:
- A refused `Hold`, `HoldConnection` or `HoldCopy` left that device, NFS connection or writable copy unheld, so a
  restart lost it.
- A refused `Release` or `ReleaseCopy` left the anchor holding what the daemon had let go, until its next restart
  handed it to a successor that never asked for it.
- A daemon taking many attachments, connections or copies at once meets this, as the test does.
- Linux is untouched.

## Exact edits
- `crates/anchor/src/held.rs`:
  - `send` takes `wait_ns`. On `ENOBUFS` it yields and sends again, until the message goes, another refusal says the
    anchor's end is gone, or `wait_ns` passes.
  - Past `wait_ns` it is refused typed, `HoldRefusal::Full { waited_ns }`.
  - Waits are counted per thread (`full_waits_on_this_thread`).
- `crates/server/src/anchor_hold.rs`: `send` passes the daemon's liveness budget, past which the shard's beat has lapsed
  anyway, and its doc says why a refusal still means the anchor is gone.
- Tests:
  - `held.rs`: `a_burst_into_a_full_channel_waits_for_room_and_arrives_whole` (2,000 releases past a paused drainer:
    every one arrives in order, and on macOS the sender waited), and
    `a_send_into_a_channel_nobody_drains_is_refused_full_at_its_wait` (macOS).
  - The A-462 test, red as found, now passes (anchor 33 of 33).

## Siblings swept
- Every other socketpair in the tree is a stream (bridge-fuse, bridge-virtiofs, ipc, server) or Linux's sequenced
  packets (`server/src/fuse.rs`).
- The UDP sockets in `rt/src/netsys.rs` already handle `ENOBUFS`.
- The Windows hold transport (`held_windows.rs`) is a different channel and was not changed.
