# A leader's commit rule scanned its backlog, and every configuration lookup scanned its log

Date: 2026-09-29. Scope: `crates/cluster/src/raft.rs` (`advance_leader_commit` and the configuration
lookups), the Raft core the regional council and the root group run. Found while measuring pipelined
replication on the timed simulation (`crates/cluster/tests/pipelining.rs`), which a first run never finished.

## Symptom

A probe of 120 short simulated runs across five Azure regions ran for more than ten minutes without output.
`sample` on the process put every sample in `RaftNode::advance_leader_commit`, called from `append_command`.
The measurement tool `a_proposal_costs_the_leader_the_same_at_any_backlog` (release, Apple M5 Max) then timed
one proposal at a leader of five whose followers acknowledge nothing:

| Backlog | Cost per proposal |
|---|---|
| 1,000 | 129 µs |
| 2,000 | 774 µs |
| 3,000 | 5.7 ms |
| 4,000 | 12.1 ms |
| 5,000 | 20.0 ms |

## Root cause

Two scans, one inside the other:

- **The commit rule** looked for the highest index a majority holds by trying every index from the log's end
  down to the commit index, counting holders at each: O(backlog × voters) per call, and it is called on
  every proposal and every append reply.
- **The configuration in force** (`effective_config`, and with it `all_voters`, `is_voter` and
  `is_majority`) was found by scanning the log backwards for its last configuration entry — the whole log
  when it held none, which is the usual case. `latest_config_index`, `config_before_latest` and
  `committed_config` scanned the same way. `is_majority` ran inside the commit rule's loop, so a proposal cost
  O(backlog × log).

## Impact

A leader's work per message grew with its backlog. The groups' logs are small and compacted, so in steady
state the cost was microseconds; under a burst — a region lost at once, many retirements to commit — or a
quorum slower than the proposal rate, the leader's own bookkeeping would have throttled it. No incorrect
result: the old rule and the new one choose the same index.

## Fix

- **The commit rule** takes, for each configuration in force, the match index at the place a majority begins
  among its voters' match indices in descending order (the leader's own is its last index), and the lower of
  the two in a joint configuration. That index commits when its entry is of the leader's term: a leader's
  entries of its own term are the end of its log, so when that entry is older, no index of this term is held
  by a majority either. O(voters log voters) a call.
- **The log's configuration entries are indexed**: their indices are kept in an ordered set beside the log,
  maintained by every change to it (append, truncation, compaction, snapshot install, restore), so each
  lookup is O(log of the configuration entries).

After: 61 ns to 102 ns a proposal at every backlog up to 50,000 entries. The core's 209 unit tests (the commit
rule's cases, Figure 8 and joint configurations among them), the cluster suite and the explorer at full scale
pass unchanged.

## Siblings swept

Every other per-message computation over the log was checked: `window_holds_at_least`, `window_fits` and the
window's byte sums iterate the window (bounded by its budget); `replicate_to`'s batch takes a slice from a
position; `room_ahead` sums what is in flight (bounded by the window); `last_index_of_term` and
`conflict_reply` scan only on a refusal. `log_bytes_through` sums the log up to an index, and the fold calls
it once per compaction check, bounded by the compaction threshold — reported, not changed.
