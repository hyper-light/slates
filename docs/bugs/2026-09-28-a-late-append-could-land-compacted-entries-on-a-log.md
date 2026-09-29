# A late append could land compacted entries on a log

Date: 2026-09-28. Scope: `crates/cluster/src/raft.rs`, the Raft core under both consensus groups (§4.8
mechanism 2). Found while building compaction for the groups (`docs/wip/research/consensus-enhancements.md`,
slice 5). Severity: latent. The groups never compacted before this change, so no deployed log was corrupted,
but every defect below became reachable the moment they did.

## Symptoms (each a failing test against `HEAD` `90874fc`, run in a scratch export, 2026-09-28)

| Test (old-API form) | Before | After |
|---|---|---|
| `a_late_append_below_a_compacted_prefix_leaves_the_log_whole` | the follower's log grew from 5 to 7 entries: two compacted entries appended at its end | log unchanged; reply matches through the commit index |
| `an_empty_follower_is_found_in_one_refusal` | 20 refusals to find an empty follower behind 20 entries | 1 |
| `a_stale_terms_run_is_skipped_in_one_refusal` | 8 refusals to skip 8 entries of a stale term | 1 |
| `late_replies_never_move_progress_back` | a late success and a late refusal moved the next append from 10 back to 3 | stays at 10 |
| `a_snapshot_reply_credits_what_the_follower_holds` | the leader credited the follower with its own later snapshot (4) when the follower held 3, so it sent no second snapshot | credits 3; sends the snapshot at 4 |

## Root causes

1. **An append below a follower's snapshot.** The consistency check looked up the entry at the leader's
   previous index. For an index inside the follower's snapshot there is none, so the append was refused and
   the leader backed up one entry. Eventually it anchored at zero, which skips the check. The append loop
   then took `entry_term(index) == None` for indices below the snapshot to mean "beyond my end" and pushed
   those entries onto the log. A late or duplicated copy of an earlier append is enough to start this.
2. **One entry per refusal.** A refusal carried nothing, so the leader backed up by one entry per round trip.
   Thesis §4.2.1 names this as "the dominant factor" in adding a server, and §3.5 describes the conflict-term
   hint.
3. **Progress overwritten.** A success reply *replaced* the follower's match index, and a refusal decremented
   the next index, so a late reply to an earlier request moved progress back.
4. **The snapshot reply had no content.** `InstallSnapshotReply` carried no index. The leader credited the
   follower with the leader's snapshot index at the time the reply arrived, which moves when the leader
   compacts again. A follower that declined a snapshot had no way to say so.

## Fix

- **Commit-prefix rule.** An append anchored below the follower's commit index is taken from the commit index
  on. The entries at or below it are skipped, because every committed entry is the same on every server. An
  append that brings nothing new is answered "matched through my commit index". etcd's
  `handleAppendEntries` answers the same way but drops the append; taking the new entries saves a joining
  member a round trip.
- **Conflict hints.** A refusal carries the conflict hint of Raft §5.3: the follower's term at the previous
  index and the first index of that term's run (never below its snapshot), or no term and the index after
  its last entry. The leader backs up past its own last entry of that term, or to the hint.
- **Monotonic progress.** Progress is monotonic within a term: the match index is the maximum seen, the next
  index is at least `match + 1`, and a back-up never goes below `match + 1`.
- **Snapshot reply states what the follower holds.** `InstallSnapshotReply` carries `match_index`: the
  follower's commit index once the snapshot is handled, or zero when it declined
  (`RaftNode::decline_snapshot`, used when the group cannot decode the state). The leader credits exactly
  that.
- **Bounded batches.** `replicate_to` takes a byte budget (`LogEntry::encoded_len`, pinned to the codec by a
  doc-truth test) and always sends at least one owed entry.
- **Wire.** `AppendReply` gains `conflict_term` and `conflict_index`; `InstallSnapshot` (tag 8) and
  `InstallSnapshotReply` (tag 9) join the wire. Golden vectors pin all nine kinds, and hostile tests cover the
  snapshot messages.

## Proof

- The five tests above, in their current form, pass. The explorer (`tests/explore.rs`) now compacts any node
  at any step to any legal point, ships snapshots, corrupts a snapshot's state in flight (which the recipient
  must decline), and uses a two-entry batch budget. At full scale (400 seeds × 4,000 steps, three and five
  voters, release, 17 s) there was no violation of Election Safety, Log Matching, Leader Completeness or
  State Machine Safety, with these counts (three voters / five voters):

  | Counter | Three voters | Five voters |
  |---|---|---|
  | compactions | 32,954 | 38,192 |
  | snapshots installed | 2,917 | 2,405 |
  | snapshots declined | 1,419 | 978 |
  | bounded batches | 20,616 | 34,839 |
  | conflict hints | 15,309 | 17,072 |

## Sibling sweep

- `InstallSnapshot` had no wire encoding at all, so a compacted leader could never have caught a follower up
  over the transport. It is added here.
- The explorer's checks indexed `saved.log` with `index − snapshot_index − 1` in `u64`. Compaction would have
  underflowed that; the checks now run over the whole reconstructed log.
- Two group tests built a candidate's last index as the log's length, which holds only before a compaction
  (the root group's small configuration compacted within the test). Both now use the snapshot index.
