# A recovery read only its own window's reach

Date: 2026-09-29. Scope: `RaftNode::recover` (`crates/cluster/src/raft.rs`), committed in `b20c5cd`. Found by
reading the code against the prefix model while planning how the groups set their windows; no run had met it,
because every node in every test held the same window.

## Symptom

None observed. The failing test written for it, `a_recovery_reads_every_report_beyond_its_own_reach`, shows
the fault: five voters, one of them (`B`) with a window reaching one index past its log. The other four — a
fast quorum of five — vote `y` at index 3, so `y` is chosen there. `B`, elected by `C` and `D`, recovered `x`
at index 2 and left index 3 free: its next entry would have gone where a chosen command was.

## Root cause

The recovery read window slots — its own and its voters' reports — only up to its own reach: its last log
index plus its window's span. The prefix model's recovery reads every slot above the candidate's log, and its
proof rests on that. The two agree only while every node's window is the same, and nothing enforced that; the
groups' window is to be set from each node's measured paths, which differ.

## Impact

None deployed: the groups' window budget is zero, so no node holds a slot, casts a fast vote or recovers one.

## Fix

The recovery reads every slot above its log, its own and reported; each reporter's window bounds what it
reports. The test passes, and a counter (`recovered_beyond_reach`) shows the explorer now reaches the path:
the explorer gives each node one of three windows per history, and at full scale 2 and 25 recoveries took a
value past the new leader's own reach, with no violation.

## Siblings swept

The other uses of a node's own reach bound only what that node does: how far it votes, buffers or accepts a
vote as leader. A vote or a buffered entry it declines stays in the proposer's or leader's hands and in
other windows, so a smaller window costs a fast commit or a resend, never a chosen value. The leader's
pipelining assumes a follower's window is its own; a follower with a smaller one declines to buffer the
excess and refuses, which returns it to probing.
