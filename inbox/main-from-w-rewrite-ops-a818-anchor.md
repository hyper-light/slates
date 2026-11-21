# w-rewrite-ops -> main: A-803 (c) = A-818, one design question before the engine switch (2026-11-21 ~10:30 PST)

Piece 1 (the pure persistent piece tree and its oracle) is started; it does not depend on this. Piece 2 does.

**The question: what a leaf names.** VS Code's piece tree (and DSST path copying) never re-points a leaf: leaves name
immutable buffers, the original and the adds. Here that means the history keeps a path's whole earlier block the first
time the path changes, beside the files arena's new block: a 64-byte overwrite of a 16 KiB file retains 16 KiB. That is
A-728 part 2's anchoring, which A-793 replaced because it cost ~73 KB of retention per accepted version at 100k clients
over the kernel (goal 86b: 94,877 of 100,000 refused `BudgetExceeded`). One path changed m times hides it in memory per
version (16 KiB / m); many paths changed once each do not.

- (i) **Leaves name immutable chunks** (VS Code as is). No walk ever; old read O(log n + pieces). Retention: one whole
  earlier block per path touched while a reader pins the floor: regresses goal 86b's case.
- (ii) **Leaves name stable byte identities (RECOMMENDED).** Each byte a change adds gets an identity once (a counter per
  path; the first value's bytes 0..len); a version is a persistent tree of identity ranges (path copying, shared
  subtrees). One location index per path says where each identity's bytes are now: in the newest block (the kept bytes,
  found by rank in the latest version's order-statistic tree, so an insert shifts nothing stored) or in an owned chunk
  (bytes a change removed or overwrote, copied out once, as A-793 cuts today). A change: O(log n + bytes removed), no
  walk of history; retention as today; old read O((ranges + later cuts) log n). Evidence: DSST 1989 for the version
  trees; VS Code's piece tree for the latest order; identity-addressed bytes as in sequence CRDTs (RGA, Roh et al.
  2011), tier B.

I'll build piece 1 so both fit (the version tree is generic over its leaf) and add an A/B row for many paths changed
once each, memory per version, so (i)'s cost shows if you choose it. Unless you say otherwise by piece 2, I go with (ii).
