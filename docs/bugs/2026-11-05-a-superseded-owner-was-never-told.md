# A superseded owner was never told, and held a write unanswered

**Found** 2026-11-05 by `a_superseded_owner_resumed_after_its_takeover_is_fenced`
(`crates/cli/tests/live_fleet.rs`). The flow passed, since the write was never acknowledged. But it took 43 s, all of
them the stale write waiting out the 40 s wait unanswered, and under parallel load it timed out a later wait.

**Root cause.** After the takeover the configuration names the successor as the volume's acting owner. A holder
checked authorization (`admitted`) before the fence, so it refused the resumed old owner's entries `Unauthorized`. The
shipper treats that as a passing refusal, so the old owner never learned it was superseded. Its write stayed deferred
for good. Had a holder answered `StaleEpoch`, the old owner would have dropped the shipper (`shipping.remove`) but left
every waiting verb unanswered. And a parked stable NFS write would have read the missing shipper as nothing to wait
for, answering "durable" on its next wake.

**Impact.** A client of a superseded owner hung until it gave up. No write was acknowledged wrongly in any run, but
the NFS path could have been.

**Edits.**
- `crates/db/src/live.rs`: `epoch_of` and `epoch_of_install` read a message's epoch from its header (hostile test
  `a_messages_epoch_is_read_from_its_header_alone`).
- `crates/server/src/live_ship.rs`:
  - `not_served`: a holder answers a message it will not serve `StaleEpoch` when the message's epoch is below the fence
    its copy promised.
  - `LiveVolume::fenced`: the fence is kept rather than the shipper dropped. `refuse_awaiting` refuses every waiting
    verb `StaleEpoch` (recorded as its completion), `defer` refuses later ones, `published` ships nothing, and
    `wait_durable` answers "not durable".

**Test.** The flow now requires the stale write to be refused within the wait. It was red (outcome `None`) before the
holder's answer and green after: exit 1 at once, and the flow takes 3.9 s.

**Sibling found and fixed.** A destroyed live volume's shipper was never dropped on its owner, along with a whole copy
of the volume. `live_ship::forget` now runs at the teardown's end and in the orphan sweep. The destroy flow was red
first (the owner still listed the log at head 4).
