# A Windows claim waiting under an anchor that let go waited on the anchor's process (2026-11-23)

## Description
A claim a client begins while an anchor holds the rendezvous waits for its answer as long as the rendezvous's holder
lives (A-468). On Windows the holder is the anchor's process, named in the bootstrap header by its pid and creation
time. If that anchor let go of the rendezvous inside its live process (an in-process anchor, as the client tests and an
embedder run, dropping its `HeldRendezvous`) while the claim waited and no daemon answered, the claim never ended. It
waited until a daemon answered it or the anchor's process exited. macOS ended the same claim within the time it had
waited, and Linux at once. w-win reported this beside 51f541f90
(`docs/bugs/2026-11-23-a-windows-anchor-that-let-go-still-read-as-the-rendezvous-holder.md`, "Siblings").

## Root cause
`unanswered_end` decides at the claim's start how an unanswered claim ends. Under a live anchor that is
`Ends::WithAnchor`, which held only the anchor's exit watch. Its poll asked one question, whether the anchor's process
had exited. Its wait (`wait_on_holder`) waited on the claim's Event beside the anchor's process with no timeout, so a
claim blocked in `Claim::wait` (the synchronous `connect`) was never woken to ask again. Since 51f541f90 the anchor's
going clears its mark from the header, but nothing a waiting claim did read the header again. A daemon that stops
inside its process had met the same case and is handled: its claimants look at the start stamp it takes back, on
A-443's doubling.

## Impact
On Windows a client claiming under an in-process anchor that let go, with no daemon to answer, waited for good. Its
`connect` never returned, and a polled claim stayed due. A stopped `slates anchor` exits, which its claimants see, so a
real anchor's clients were not affected (inferred from the code, not measured). The macOS test of this case
(`a_claim_under_an_anchor_ends_at_the_anchors_going`) was kept off Windows, where its unbounded poll would have spun
for good.

## Exact edits
- `crates/ipc/src/rendezvous.rs`: `Ends::WithAnchor` holds the anchor's mark beside its exit watch. Its poll ends the
  claim when the header no longer holds that mark ("the anchor that held the rendezvous let go of it before the claim
  was answered"). Its wait times out on A-443's doubling, as `Ends::WithDaemon`'s does, so a blocked wait looks again.
  The docs of `begin_connect_as`, `AT_ANCHOR_PID`, `Ends` and `wait_on_holder` say so.
- `crates/ipc/src/rendezvous.rs` tests: `a_claim_under_an_anchor_ends_at_the_anchors_going` runs on Windows too, its
  poll bounded by the claim wait. `a_waiting_claim_under_an_anchor_ends_at_its_going_inside_a_live_process` (macOS and
  Windows) blocks in `Claim::wait` on another thread while the anchor lets go. Each expects `DaemonUnavailable` for the
  anchor's going within the claim wait.

## Evidence
- Red: with the fix reversed, both tests failed on windows-latest and windows-11-arm, "the claim outlived its anchor's
  going by a claim wait" and "the waiting claim outlived its anchor's going by a claim wait" (diag run 45238179939,
  jobs 137915395132 and 137915394945).
- Green: with it, the rendezvous's 14 platform tests passed on both runners. The polled claim ended 10 µs (x86_64) and
  12.5 µs (arm64) after the let-go, the waiting claim 67.5 ms and 26.7 ms after it, each for "the anchor that held the
  rendezvous let go of it before the claim was answered".
- macOS arm64 passes both before and after the change: 3.3 ms and 45.6 ms after the let-go, the owner's name gone.
