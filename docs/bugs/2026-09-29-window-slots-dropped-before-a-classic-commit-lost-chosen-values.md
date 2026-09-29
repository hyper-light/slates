# Window slots dropped before a classic commit lost chosen values

Date: 2026-09-29. Scope: the dialect's design for the fast track and parallel replication
(`docs/wip/research/consensus-enhancements.md` §4, as committed in `1bc85f1`), its model
(`crates/cluster/tests/prefix_model.rs`), and the uncommitted `RaftNode` that implements it. Found by the Raft
safety explorer at full scale (`crates/cluster/tests/explore.rs`), on the first run over the real core with
the fast track.

## Symptom

`the_dialect_keeps_raft_safety_at_full_scale` failed at seed 266, three voters, step 630:

```
leader HostId(2) of term 11 lacks the command a fast quorum chose at index 29: holds Some(LogEntry { term: 11, command: [], config: None })
```

The replay (`SLATES_EXPLORE_SIZE=3 SLATES_EXPLORE_SEED=266 … replay_to_the_first_violation`) shows the
history. In term 7 all three voters voted command 28 at index 29, a fast quorum. Node 2 won term 8, recovered
the command into its log and, as the design said, emptied its window. Node 1 won term 9, recovered the
command too, and truncated node 2's log back to index 28 with an append that stopped there. Node 2 then held
the command nowhere. In term 11 node 2 won with node 3, whose one report fell short of the threshold of two,
treated index 29 as free and put its no-op there.

## Root cause

The design let a node forget a slot before anything protected the value it recorded:

- **At a sync.** A synced node dropped its slots of older terms, and a new leader cleared its window at its
  recovery. The synced log carried the values, but that log was not committed: a later leader's truncation
  could erase it. The prefix model reproduces this in 18 steps at three nodes, three indices, one value and
  four terms. Its full-scope searches had covered three indices with three terms and four terms with two
  indices, never both, so the design was verified at a scope that could not show the fault.
- **At a commit that counted fast commits.** The first correction kept older slots until the node's commit
  index covered its synced leader's no-op, and pruned slots under the commit index. The model found that
  unsafe too (at 39,450,280 classes): a fast commit puts a value in a fast quorum's windows, not a majority's
  logs, so a candidate whose last entry a successor had re-proposed under a newer term outranked the log
  that held the value, and the slots that recorded it were gone.

The rule the evidence supports: a slot goes only once a classic commit covers its index. Raft's election rule
then puts that entry, and every value below it, in every later leader's log. So a node's commit index is
classic: it is what followers learn and what windows prune under. The leader alone counts its fast choices,
to apply and acknowledge them.

## Impact

None deployed. The fast track and the window are not wired into the council or the root group yet, and a
node's window budget defaults to zero, which holds no slot and opens no fast track. The committed design
document and the model's claim of verification were wrong, and are corrected in the same change.

## Fix

- `RaftNode`: the commit index stays classic; a leader's fast choices extend a separate, volatile frontier
  (`committed_through`) that the caller applies, acknowledges and reads at. A new leader keeps its window,
  a sync drops nothing, and a slot is pruned only under the commit index. Tests:
  `a_synced_follower_keeps_older_slots_until_a_classic_commit_covers_them`,
  `a_new_leader_keeps_its_window_until_a_classic_commit`,
  `an_old_leaders_fast_commits_yield_to_its_successors_entries`,
  `a_proposal_a_fast_quorum_votes_commits_in_one_round`.
- The prefix model: each node knows its classic commit index; a slot goes only under it; the append keeps a
  follower's committed prefix, as the code's does; a new leader keeps its window. Both earlier rules are
  kept as rejected variants that fail by their shortest histories: `DropAtSync` (18 steps) and
  `PruneAtFastCommit` (12 steps: a follower keeps a fast-committed entry under the fast leader's term while
  every later leader holds it under its own). The corrected design holds with Raft's strict log matching:
  152,906,020 classes at three nodes, three indices, one value and four terms, and 188,172,261 with two
  values and three terms (`docs/wip/BENCHMARKS.md`). The path that keeps a committed entry under an older
  term than a leader's is never taken.
- The explorer passes at full scale: 1,227 and 1,691 fast choices, 740 and 1,474 recoveries of a fast
  choice, 10 and 27 commands committed under two terms (the leader's fast commit and a successor's
  re-proposal), and seed 266 among them.

## Siblings swept

Every place the dialect drops window state was checked against the rule: `observe_sync` (no longer drops),
`recover` (no longer clears), `prune_committed` (classic commit only, called by the leader's commit, a
follower's append and a snapshot install), and `step_down` (clears only the leader's volatile vote tallies,
never the retained window). Two changes made while chasing the symptom — a consistency check that skipped the
commit index, and a snapshot refused at or below it — answered term differences at committed indices, which
the classic commit index rules out; they were reverted, not kept as a second path.
