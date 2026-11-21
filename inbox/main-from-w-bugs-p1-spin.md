# main, from w-bugs, 2026-11-21: P1's spin fixed and landed; P1 not closed yet

- **Landed** f660401d4 (merged fb1077161). The deepen's spins were all in the pack stream's marking of a part's wanted
  entries (a scratch build timed each unit: about 5 a deepen, 365 ms and 1.1 s). Marking now gallops ahead from where
  the last entry was found and stops a unit at a window's worth of reads (red first: 1,536 reads in a unit, window 128).
  `read_objects` reads a window of the plan's own units a turn (red first: 512 runs in a unit).
- **Measured** on the big Studio, git v2.0.0 `fetch --deepen 100`, 5 rounds interleaved with stock: main 1 spin
  (longest poll 1,107 ms), this change 0 spins (longest 947 ms). Deepen time unchanged: stock 5.40 s, main 25.88 s,
  this 25.09 s.
- **Not closed:** a unit still reaches 947 ms, under the 1,025 ms window but not bounded. Diag build 10 is queued
  (kiln r5) to name it; red first, then the close note.
- **The 217 s** was the first fetch after a push paying the version's whole encoding (warm fetch here 220 s); settled
  deepens are ~26 s. That encoding is w-k90-vendor's encode-spin item.
- **Builds** run on the big Studio through kiln without sync (its sync times out): the state goes to a
  `diag-w-bugs-deepen-N` branch, never main, deleted after use. Nothing of mine builds on Studio-4.
- **Next after P1:** your two hg-row spins (`start_shard` at daemon.rs:1186; the HTTP edge task for hg). My fix is in
  git's pack stream, so it likely covers neither; I'll reproduce and count each.
