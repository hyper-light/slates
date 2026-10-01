# A proposal could outgrow the consensus record after the protocol had moved

**Date:** 2026-10-01. **Area:** `slates-cluster` (`raft.rs`), the publication cost measurement
(`tests/publication_cost.rs`). **Audit:** AUD-29-30 (P2). **Design:** §4.8 status, §4.9.

## Description

Every Raft transition is published, whole, before its acknowledgement: the retained state is cloned,
encoded, hashed and copied into the anchor's consensus region. A leader appended every proposal it was
given. A log that grew past what the region holds made the next publication fail after the protocol had
already moved, and a failed publication closes the control shard. Nobody had measured the cost of the
publication itself, so whether it needed to become incremental was unknown.

## Root cause

Retention's capacity was checked only at publication, after mutation. The Raft core had no log budget to
admit against until A-58 added one for recovery.

## Fix

- **Admission before mutation.** `leader_append` (proposals, membership entries, the recovery's sync no-op)
  refuses an entry that would take the log past its budget: it returns `false`, counts
  `RaftNode::budget_refused`, and leaves the log unchanged. The budget is the one A-58 derives: each group's
  log plus an equal share of the room its published record leaves.
- **The size kept incrementally.** The log's size is maintained as entries are pushed, truncated, compacted
  or replaced by a snapshot (`log_bytes_held`), so the check costs nothing per proposal. A per-proposal walk
  cost 20 ms at a 5,000-entry backlog in 2026-09-29's measurement.
- **Measurement.** `a_publication_costs_the_clone_the_encoding_and_the_hash_of_the_whole_state` times the
  three steps separately (`docs/wip/BENCHMARKS.md`, 2026-10-01):

  | Log | Total per publication | Share that is the hash |
  |---|---|---|
  | 1,000 entries | 70 µs | three quarters |
  | 10,000 entries | 614 µs | three quarters |
  | 50,000 entries | 3.1 ms | three quarters |

  The cost is linear, about 0.8 ns a byte. Compaction keeps a group's log within one to two times its encoded
  configuration, so at today's sizes a publication costs tens of microseconds.
- **Incremental delta publication: measured and rejected** on those numbers. It would add a second recovery
  path to save a cost the compaction rule already bounds. Revisit if a configuration grows into the megabytes.

## Tests

- `a_proposal_past_the_log_budget_is_refused_before_the_log_changes`: a lone leader with a budget for its
  no-op and four commands appends four, refuses the fifth with retained state unchanged and the refusal
  counted, and admits again once compaction frees the room. Before this change every proposal was appended.
- The generated admission test now recounts the log's size after every step and requires it to equal the
  kept count.
- Interrupted publications at every byte were already covered
  (`retention::tests::an_interrupted_publication_never_erases_an_acknowledged_vote`).

## Siblings reported

- **Followers do not check appends against their own budget.** A follower whose budget is smaller than its
  leader's (a smaller region, or a record with more fixed state) could still overflow. Budgets derive from
  the same layout on every node, so they differ only by the fixed parts of the record; not observed.
- **The cost of the record's non-log parts** (authorization, recovery, members, root homes) is not measured
  separately. It is proportional to their bytes at the same rate.
