# One malformed hold message ended the Windows anchor

Found 2026-11-03 by the audit of `2dc9f3eb..98000d56` (kiln plan 01a1a34bf7e973069e94f773afdea44a, step 3).
Design: A-198 (the anchor's Windows hold channel), §2.5.

## Description

The anchor drains its Windows hold channel at every supervision step (`Supervisor::step`, `crates/anchor/src/supervise.rs`).
The Windows hold (`crates/anchor/src/supervise_windows.rs`, `Hold::drain`) passed any refusal of the channel straight up
with `?`:

- a message shorter than `MESSAGE_BYTES` (`held_windows.rs`: "a short read of a hold message");
- a message whose checksum or shape does not decode (`handles::decode`);
- a reply the channel could not write.

`step` returned the refusal, and the anchor's run loop (`crates/cli/src/anchor.rs`) returns on a step error. The anchor
then exits. Its kill-on-close job ends the daemon, and every volume handle and drive link it held closes with it: every
WinFsp drive the anchor was keeping across daemon deaths disappears. The Unix device hold counts the same refusals and
goes on (`Supervisor::drain_devices`).

## Root cause

`2dc9f3eb` and its successors gave the Windows hold a fallible drain and propagated its error through `step`, where the
Unix hold's drain counts refusals and cannot fail. Nothing a daemon sends should be able to end its supervisor.

## Impact

Windows. A daemon bug that writes a malformed hold message, or a write the pipe delivers short, takes down the anchor,
the daemon and every held drive at once: the opposite of what A-198 is for. No run is known to have hit it; the live
tests send only well-formed messages.

## Fix

- `Hold::drain` cannot fail. A message the channel refuses is counted and dropped, and the drain reads on, since the
  next message may be whole. Any other refusal of a read is counted and ends that turn's drain. A reply the channel
  cannot write is counted and dropped, and its daemon's own wait ends at its deadline as a typed refusal.
- `Supervisor::drain_handles` returns the number applied, and `step` no longer fails on it.
- `Supervisor::handle_refusals` counts the channel's refusals with the holdings' own.

## Failing test first

`crates/anchor/tests/handles_windows.rs`, `a_malformed_hold_message_is_refused_and_counted_and_supervision_goes_on`:
a real supervised child (`garbling_role`) writes a short hold message and one with a corrupted checksum before its real
hold. The test requires every step of the real supervisor to succeed, both messages counted as refusals, the real hold
kept, and the successor to inherit the same live object.

Evidence is recorded in GAPS and A-198 with the CI runs that show it: the test fails without the fix and passes with it
on CI's Windows x86_64 and arm64 runners.

## Siblings

- The server's side of the channel (`crates/server/src/anchor_hold_windows.rs`) already turns a refused exchange into a
  typed mount refusal.
- A hold whose reply is dropped leaves its daemon's mount refused typed; the holding itself, if applied, is released
  with the next successor's handoff or the anchor's exit.
