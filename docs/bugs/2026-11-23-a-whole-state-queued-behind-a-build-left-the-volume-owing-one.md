# A whole state queued behind a build left the volume owing one (2026-11-23, §4.10 "Live shipping", A-377, AC-8.9)

Found by use: CI's conformance lane, the live-shipped fsstress over `native-linux-fuse` on `ubuntu-latest`. It failed
intermittently: nightly 45176536354 and run 45163517212 (at 4c8b391ec) failed, runs 45177206583 and 45181584785 passed.
It is a liveness and cost defect. No register invariant was at risk: every entry was a valid link of the owner's chain,
and every whole state carried the volume's true state.

## Description

A live volume's whole entry of more than one piece is sealed in turns (A-377). The publications made meanwhile are
captured and queued behind it. A delta is queued while the deltas queued stay within the credit. Past it, the delta is
refused for credit and "follows in a whole state the volume owes": one whole state, made at the next publication.

That whole state was queued, but the volume still owed one. `live_ship::queue_behind_build` cleared nothing, and only a
build's start or a whole entry's append cleared `owes_whole`. So every later publication in that build was captured
whole and queued too. Each was a full copy of the volume, made in its publication's turn. Every acknowledgement that
found the debt within the credit queued another (`owes_whole_within_credit` calls `published(None)`). Each queued whole
state later became a build of its own, sealed in turns while the next publications queued behind it.

## Measured

- CI run 45163517212, the owner's `fleet.live` counters: 1,810 entries appended, 174 of them whole, 1,885,852,875 whole
  bytes in 102 builds (59,515 turns), 16 refused for credit, 135 installs built, `held_back.whole` 457. The candidates
  held 1808 against the owner's head 1809, durable 1808, at the end of the 40 s fleet wait.
- Nightly 45176536354: 202 whole entries, 2,309,870,334 bytes, 4 refusals. The two passing runs: 80 whole entries of
  48 MB from 3 refusals, and 113 of 217 MB from 5.
- adacachy2 (x86_64, 32 cores, load 21), main 8216c1fce with log lines added at each whole publication, refusal and
  whole append: `cargo xtask conformance run --suite fsstress --live-ship --transport native-linux-fuse` passed, with
  5 refusals and 104 whole entries of 300 MB. Of the 104 whole publications, 102 were made while a build was in flight,
  each owing a whole state (88 with a record, 16 from the acknowledgement path with none). The owner's log reads, in
  order: the first whole entry (620 bytes), a refusal for credit at head 1, a whole capture that began a build, a second
  refusal, then dozens of `published whole: owes_whole true … building true` in a row.

## Root cause

`queue_behind_build` queued the owed whole state without paying the debt. It also counted the deltas behind that whole
state together with the deltas ahead of it, so even with the debt paid, the next delta would meet the bytes ahead of
the whole state, be refused again, and owe yet another whole state. The design states the rule per whole state: "the
deltas queued stay within the credit the whole entry ahead of them sets".

## Fix

`crates/server/src/live_ship.rs`:
- `queue_behind_build`: a whole state queued clears `owes_whole`. It carries every change up to its publication, the
  refused delta's among them. The deltas ahead of it stop counting against the credit, and the count starts again
  from it.
- `Queued::counted` marks a delta behind the newest whole state in the queue, or behind the build while the queue holds
  none. `WholeBuild::queued_bytes` sums those alone. `append_ready` subtracts or adds a delta's bytes only when it is
  counted, and so does `advance_capture`'s capture body at the queue's head.

Test, red first: `a_delta_refused_behind_a_build_owes_one_whole_state_and_the_writes_after_it_queue_as_changes`
(`live_ship`'s unit tests). On a live volume with no whole entry yet, it writes two 512 KiB files and publishes at a seal
quantum of zero, so its whole state is captured in turns. Behind the capture it rewrites a file whole and publishes
until a delta is refused for credit, then writes 192 KiB and publishes, four times. Then it runs the build and
acknowledges every entry until nothing is queued.
- On main: 1 refusal, then 5 whole entries of 6 appended (every page write a whole state).
- With the fix: 1 refusal, 2 whole entries of 6 appended (the first, and the one the refusal owed).
- Red-checked on a copy of the tree, each half of the fix removed in turn:
  - Without the debt paid: 1 refusal, 5 whole entries of 6.
  - With the debt paid but the deltas behind the whole state counted with the rewrite ahead of it: 2 refusals and 2
    whole entries of 5. The last page write was refused again, and its whole state was owed with nothing left to pay it
    until the next publication.

Surface check on adacachy2 (load 21 to 23), the same lane with the fix, 5 of 5 passed: each run caught up, its whole
entries one more than its refusals. Whole entries were 24, 22, 23, 22 and 22, of 28.5, 31.4, 31.4, 9.8 and 25.1 MB,
from 23, 21, 22, 21 and 21 refusals. Installs fell from 61 to 17 to 26. The suite took 6.1 to 7.3 s, and 42.5 s in the
first run, where main's run took 57.6 s. Once more on the final tree, with the diagnostic log lines removed: 22 whole
entries of 12.7 MB from 21 refusals, caught up. More refusals are counted than before, since the debt no longer hides
them: a publication owing a whole state is never refused.

## Impact

Since A-377 (2026-11-18), any live volume whose deltas outran the credit while a whole entry or a large delta was being
sealed shipped the whole volume for every publication until that build's queue drained. Each copy was taken in its
publication's turn on the owner shard, held in the queue, sealed, hashed and sent to every holder. Holders fell behind
the retained tail, which a whole entry fills, and were installed whole again. On CI's runner this outlasted fsstress by
more than the fleet wait.

## Siblings

- Not a defect: the candidates' `durable=none` in the record. Every held copy reports no durable position
  (`live_ship::reports` sets `durable: None` for a held copy), and the harness reads only the candidates' heads.
- The harness, reported and not changed here: `Session::alive_and_shipped` (`xtask/src/conformance/slates.rs`) returns
  "the daemon answered `volume list`" and "the log shipped" as one flag. So a run whose log did not catch up is recorded
  as "the daemon answered `volume list` after the run: false" and "the daemon stopped answering during the run", though
  `status` answered throughout.
- To be measured again: the cost GAPS recorded on 2026-11-18 for the live-shipped pair (fsx on the Studio, 407 whole
  states shipped past the live credit, `refused.no_credit` 411) may share this cause.
- A-377's own owed item, "a second whole entry followed the first in every run of the test", is not this. Its eight small
  deltas stay far inside a 3 MiB build's credit, so nothing is refused there.
