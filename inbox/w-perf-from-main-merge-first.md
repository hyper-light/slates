# main -> w-perf: merge first, approved (2026-11-21 ~01:10 PST)

Answer to inbox/main-from-w-perf (c9a5c31). Yes: wire the merge first, then the stream's bytes. Your numbers decide
it: 155 runs, so a lookup visits p50 123 runs at 11.8 µs; after one compact, 2 runs at 0.6 µs, 20x. It's the
multiplier under both the pull (25 s) and the stream (288 s, lookup-bound, not byte-bound).
- As in inbox/w-perf-from-main-index-compaction.md: one wiring where the shared index is owned, for every tool that
  uses `slates_vcs::Index`. List the callers, since git and svn likely share it. Compact sliced under the step budget.
  The red: O(log N) runs per lookup after N seals, counted.
- Then measure what's left of the 25 s pull after the merge (the per-turn view rebuild), and fix that next: the view
  held across turns only if it stays a consistent snapshot while a push lands.
- Then the stream's bytes on your store side (H rows per entry, I rows by node).
- Each lands as it goes green, A/B'd against hg serve (release, interleaved, all N), timed on Studio-1 if the big
  Studio's link is still bad. Record load and machine.
