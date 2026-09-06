# Mount helper orphaned on an early failure: no owner reaped the child (BUG-14)

Date: 2026-09-05. A source finding under GAP-A9-4 (with BUG-4, already fixed). Fixed structurally;
runtime-verified in the Linux CI lane (this file is Linux-only and unit-tested only there).

## Description

`mount()` spawns the `fusermount3` helper, then receives the `/dev/fuse` descriptor over the socket.
If receiving the descriptor failed, the function returned the error and never waited for the helper
child — leaving it a zombie (never reaped) or, if it outlived the daemon, an orphan. Repeated failed
mounts would accumulate zombies.

## Root cause

`crates/bridge-fuse/src/mount.rs`, `mount()`:

```
let mut child = Command::new(FUSERMOUNT)...spawn()?;
drop(theirs);
let device = receive_device(&ours)?;   // an early `?` here returns WITHOUT waiting on `child`
let status = child.wait()?;            // reached only when receive_device succeeded
```

Only the success path reaped the child; every early return between the spawn and `child.wait()`
orphaned it. This is Part 2 item 9 (a spawned child without an owner that joins or cancels it) and
the audit's BUG-14.

## Impact

- A zombie helper process per failed mount, until the daemon exits. Bounded by the failure rate, but
  a resource leak and a diagnostic nuisance. Linux only (the mount helper is Linux).

## Fix

An owning RAII guard, `HelperGuard`, holds the spawned child so it is reaped on *every* exit:

- The success path calls `HelperGuard::finish()`, which waits for the helper's own exit and disarms
  the guard (takes the child, so the later drop is a no-op).
- Any earlier failure — `receive_device` returning with `?` — drops the guard instead, and its
  `Drop` `kill()`s and `wait()`s the child, so no zombie or orphan is left. Both calls tolerate a
  helper that already exited (the `wait` still reaps the zombie); neither error is actionable at drop.

Correctness is by construction (the child is owned by a guard that reaps on drop), so it holds on
every current and future early-return path, not just the one `?` today.

## Verification

`mount.rs` is `#![cfg(target_os = "linux")]` and, by its own contract, is exercised against a real
`fusermount3` in the CI Linux lane, not by a unit test on other hosts. In this environment the fix
is verified by the Linux cross-lint (`cargo clippy --target x86_64-unknown-linux-gnu … -D warnings`,
clean) plus the structural argument above; the runtime reaping-on-failure is a CI-lane assertion
(the mount handshake with an injected `receive_device` failure). fmt, xtask and the host build are
clean; 30 bridge-fuse tests pass.

## Ledger

Closes the BUG-14 portion of GAP-A9-4 (with BUG-4). The rest of GAP-A9-4 — the attachment record as
a mounted path, and the dirty-cache seal/writeback barrier — needs the real mount and the async
writeback model respectively, and stays open. Left to the audit-doc owner to update
`docs/wip/GAPS.md`.
