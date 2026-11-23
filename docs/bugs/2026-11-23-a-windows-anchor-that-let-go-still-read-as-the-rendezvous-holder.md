# A Windows anchor that let go still read as the rendezvous's holder (2026-11-23)

## Description
`every_call_ends_and_the_loop_is_never_held_across_restart_and_death` (`crates/client/tests/driver.rs`) failed on both
Windows lanes of CI run 45176536354 (205d779de; x86_64 job 137699123636, arm64 job 137699123949) with
`call 64 after the daemon's death ended DaemonGone: None`. Its second phase stops the daemon, lets the anchor go of
the rendezvous (`HeldRendezvous` dropped, as an anchor that stops for good does), and submits a ring's worth of calls.
Each must end `DaemonGone` within twice the reply and reconnect budgets. On Windows none had ended. The same test passes
on Linux and macOS.

## Root cause
A-468 made a client whose daemon is gone claim again while the rendezvous has a holder, and fail `DaemonGone` at once
when it has none (`Client::refused_reconnect`, `slates_ipc::holder_lives`). Linux and macOS judge the holder by the
hold itself, its listening socket and its owner's bootstrap name, which go with the `HeldRendezvous`. Windows judges it
by the anchor's process, from the pid and creation time `Held::hold` writes into the bootstrap header, and nothing
cleared that mark when the hold ended. The header outlives the hold while any handle maps the object, and Windows
keeps a named object while any handle holds it (A-176). A connected client holds one: its doorbell is a mapping of the
bootstrap object (`Doorbell::Word(Bell)`). So after the anchor let go, `holder_lives` opened the object, read the
anchor's mark, found its process alive (the test's own process), and answered true. `try_reconnect` then claimed
again on every step, each claim refused because no daemon had published, and no call ever ended.

The Listener had met the same trap for the daemon's start stamp and takes it back at its drop ("on Windows a client's
own doorbell kept even an unheld object, and its stamp, alive"). The anchor's mark had no such end.

## Impact
On Windows, a process that held a rendezvous and let go of it while it lived left every client of that instance
claiming for good, instead of failing `DaemonGone` at once. An anchor that exits is judged gone by its process's exit,
so by the code a stopped `slates anchor` was not affected (inferred, not measured). The test's in-process anchor is
the case CI showed. It also kept both Windows test lanes red, hiding later Windows regressions there.

## Exact edits
- `crates/ipc/src/rendezvous.rs`: `impl Drop for Held` (macOS and Windows) clears the anchor's mark from the header,
  its pid and on Windows its creation time, while its owner's token is still held, so the mark cleared is never a
  successor's. `Held`'s object field is named `object` now that its drop reads it. `AT_ANCHOR_PID`'s format line says
  the mark is zeroed at the anchor's going.
- `crates/ipc/src/rendezvous.rs` tests: `an_anchor_that_lets_go_is_no_holder_though_a_client_maps_the_object` (macOS
  and Windows): held while the anchor holds the rendezvous, no holder once it has let go though a client maps the
  object. It passes on macOS before and after the change, and fails on Windows before it.

## Siblings (reported, not changed)
- A claim already waiting under the anchor (`Ends::WithAnchor`, begun before the anchor let go) waits on the anchor's
  process with no timeout. If the anchor lets go inside a live process while such a claim waits, the claim waits until
  a daemon answers it or that process exits. No test reaches this case; the driver test's claims begin after the let go.
