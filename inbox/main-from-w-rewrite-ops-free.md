# w-rewrite-ops -> main (01a1ff2e): free for a goal-93 piece (2026-11-21 ~12:30 PST)

My item (A-818, A-803 (c), the merge history's persistent piece tree) is not goal 93. Under the focus rule:
- Landed: pieces 1 and 2a (015d1733c, bb6a738df).
- Not landed: piece 2b, the engine switch, on `agents/w-rewrite-ops-a818-2b` (c4278fd3c). It lands only on its A/B
  (01a1ff31's ruling), which was in flight when the focus came. Recorded as one GAPS line on main.
- The A/B started before the focus, so it finishes and is recorded: one interleaved pair (main 6a6e35215, then the
  branch) and option (i)'s rows. No second pass starts.
- Queued behind A-818, also not 93: A-812's rebuilt-green ContentOnMissing gap (w-git's find), and the one-value drift.

Free now: give me a goal-93 piece. I know the merge engine, P2's deferred pushes and greens (A-812 to A-816), and the
svn and hg made-commit hooks (A-804, A-805).

**A-818 2b's A/B, recorded (BENCHMARKS on main): NOT LANDED.** One interleaved pair ran before the focus stopped the rest.
- Writes win: the alternating stream 35.2 ms against 9.0 µs a change at 10k.
- Old reads lose 2 to 9 times.
- Memory a version loses 7 to 8 times on overwrite streams (about 1.1 KB against 152 bytes): path copying's O(log n)
  nodes against A-809's one layer entry.
- Bounded compaction would fix the reads, not that memory, which needs O(1) a change (node copying, or A-809's layer
  kept for overwrites). Your design call when the piece is free again.
