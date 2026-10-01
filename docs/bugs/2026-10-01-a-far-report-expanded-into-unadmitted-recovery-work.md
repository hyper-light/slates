# A far window report expanded into unadmitted recovery work

**Date:** 2026-10-01. **Area:** `slates-cluster` (`raft.rs`, the safety explorer), `slates-server`
(`retention.rs`, shard init). **Audit:** AUD-29-37 (P1). **Design:** §4.8 status (A-58),
`docs/wip/research/consensus-enhancements.md` §4.

## Description

When a candidate won, its recovery appended every index from its log's end to the farthest slot any
granting voter reported, in one step. Each index got the decided value or a no-op. There was no admission.

- One report 1,000 indices above the log appended 1,000 entries inside the vote-reply handler.
- A report near `u64::MAX` (a valid wire `u64`) would have looped until the index range ran out.
- Nothing bounded the memory, and the next retention publication of the grown log could exceed the consensus
  region. A failed publication closes the control shard.

Reports far above the log cannot simply be dropped. Discarding reports beyond the new leader's own window
lost a chosen value in the safety explorer (seed 266, 2026-09-29), and a legitimate gap is as long as an
older leader's uncommitted tail, which nothing bounds.

## Root cause

Recovery was eager and unadmitted: its cost was the gap in index space, not the size of the reports. The
Raft core had no log budget at all.

## Fix

- **A plan, admitted before mutation.** `plan_recovery` decides the values and the span without appending.
  `admits` sizes it arithmetically (holes × the smallest entry's bytes, plus the values' bytes) and compares
  log + plan against the node's log budget.
  - Over budget, the node declines the term it won. It goes back to follower, appends nothing, and counts it
    in `WindowCounters::recovery_refused`.
  - The voters keep their windows, so a later election recovers the same values.
- **Materialized a slice at a time.** An admitted plan appends one window's bytes per call, at least one
  entry: once at the win, then once per `replicate_to`.
  - While it runs, the leader refuses proposals, membership changes and reads (`recovering()`). A commit of
    one of its own entries below a recovered value could otherwise authorize a read that misses a value its
    predecessor acknowledged.
  - A plan that took more than one slice ends with the leader's own sync no-op (§5.4.2). Its group's no-op at
    the election was refused while the plan ran.
  - The plan carries its term and acts only while the node leads that term. A node that steps down or
    crashes drops it.
  - A log at the last index has an empty plan, and an exhausted append ends the plan (counted
    `indices_exhausted`).
- **The log budget** is set by retention at every publication, and once at shard start from the restored
  record: each group's log plus half the room the record leaves in its consensus region. Both groups'
  admitted growth together fits the region, and growth since the publication counts against the share.
- **The explorer's Leader Completeness check** for fast-chosen commands now runs when a leader's recovery is
  done, not at the election. That is the design's rule now. State Machine Safety and fast agreement still run
  every step.

## Tests

- `a_far_report_is_recovered_a_slice_at_a_time`. **Red before**: "the win materialized 1000 entries, more
  than one slice (3)". Now:
  - the win appends one slice;
  - proposals and reads are refused mid-recovery;
  - the value lands at its index under the new term, followed by the sync no-op;
  - a proposal is admitted after.
- `a_recovery_past_the_log_budget_declines_the_term`: with half the plan's bytes as budget, the node is not
  leader, its log is untouched, the refusal is counted, its saved state restores, and it follows a newer
  leader.
- `a_leader_that_crashes_mid_recovery_loses_no_reported_value`: B crashes between slices and restores with
  its partial plan. C wins the next term with the same report and recovers the value, and B's log,
  overwritten by C's, holds it too.
- `a_publication_shares_the_records_room_between_the_groups_log_budgets` (server): after a publication each
  budget is the log plus half the room, and both fit the region.
- `a_recovery_reads_every_report_beyond_its_own_reach` now drives its one-entry-window leader's two-slice
  recovery to the end, and expects the sync no-op after the recovered values.
- **The safety explorer at full scale** (`cargo test -p slates-cluster --release --test explore -- --ignored`,
  2026-10-01, Apple M5 Max, 38.1 s): green, with 41 (three voters) and 102 (five voters) multi-slice
  recoveries. A new rare-path floor keeps the path covered in every run.
  - Its first run under the change failed at seed 1 step 860 ("leader HostId(2) of term 10 lacks the command
    a fast quorum chose at index 32: holds None"). That was the old at-election completeness check meeting a
    leader mid-recovery. It is what led to the read gate and to restating the check.
- Every cluster test and every server unit test pass.

## Siblings reported

- **Proposals are still not admitted against the log budget**, so a leader's own backlog can grow past what
  retention can publish. That is AUD-29-30's admission, which this budget now makes possible.
- **The server's log budget is a fixed half of the room per group.** A group that legitimately needs more
  than half while the other is small is refused. A demand-weighted split would need the publication to
  carry each group's recent growth; not measured as a problem.
