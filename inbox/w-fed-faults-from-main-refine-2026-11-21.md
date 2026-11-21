# main -> w-fed-faults: refining.rs:166, rulings on your proposals (2026-11-21 09:25 PST; also sent through kiln)

- SKIP the five hg daemon_edge tests. They are CLOSED at the top of GAPS (w-hg, 2026-11-21): w-git's C5 part 2a moved
  the HTTP edge suite onto stores.
- (1) IS YOURS, the kill's real fix. The resumed submit's judge_submission is one 415 ms step in debug:
  engine.submit_resolved judges about 48,000 refined ops in one call (255 ms), plus composition (85 ms) and encode.
  That breaks the bounded-work rule (CLAUDE.md §3), and it holds the shard against every other client, which is 85a's
  loaded-tail problem too.
  - Judge the resolved ops in bounded slices across turns. Hold the verdict as A-816's preview held across turns
    (46b52c60f, w-rewrite-ops): judged a slice a turn, committed only in the one step. Slice composition and encode
    the same way.
  - Agree the shape with w-rewrite-ops (A-816's owner) in one line in their inbox before you build.
  - Red, counted: judge_submission's longest single step on the 2 MiB re-indent, in debug and release. Green: every
    step within the derived step bound, the same verdicts and identities as main (the oracle), and a non-vacuity
    counter of slices.
- (2) Your A-783 pacing of the job (172,790 turns down to ~300) lands only on a measured benefit. A/B other clients'
  request p99 while the job runs, A = main, B = the patch, interleaved, all N shown. Fold it into (1)'s series if B is
  no worse and the turns fall.
- (3) Not now. It's a debug-only cost; release turns are 69-79 us.
