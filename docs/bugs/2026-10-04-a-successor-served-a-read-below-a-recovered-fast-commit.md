# A fast-track successor served a read below a fast commit it recovered (2026-10-04)

## Description

hyper-raft's hyper-check S-4 found that fast-group leaders could serve reads below an earlier fast-quorum commit.
I checked slates' own council (`crates/cluster/src/raft.rs`), which runs the same fast track, and wrote
`a_successor_serves_no_read_below_a_fast_commit_it_recovered`. It failed:

```
a read was served at Some(2), below y's fast commit at 3
```

## Root cause

A won election's recovery re-proposes every value an earlier term's fast quorum may have chosen, appending them
**under the current term**, then the sync point. `begin_read` required only that the commit index sit on an entry
of the current term and that recovery had finished appending. With one entry per append (an append budget or a
sliced recovery), the first recovered entry commits before a later one. The read then had a current-term commit
at index 2 while `y` at 3, already acknowledged to its client by the earlier leader's fast quorum, was
uncommitted on the successor. A read at 2 missed it (ReadSafety, §4.8).

## Impact

A linearizable read through the configuration group could return state older than a write a client had already
seen acknowledged, in the window after a leader change on the fast track. The window closes once the recovered
range commits; under load or with sliced appends it can last several rounds.

## Exact edits

- `crates/cluster/src/raft.rs`: a volatile `read_floor`, set at the election to the last index the recovery
  appends (zero when it recovers nothing). `begin_read` refuses while `commit_index < read_floor`. Every earlier
  fast choice lies at or below that index (the recovery scans every reported slot), and log matching makes its
  commit cover them all.
- The test, which failed before the fix and passes after; every cluster suite passes (23/23).
