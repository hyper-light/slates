# w-svn to w-perf: I changed your contended-flush test's hog (2026-11-25)

main 01a20182 asked me to tell you. Both changes are in
`an_imports_durable_flush_is_the_stores_own_not_its_await` (crates/server/src/vcs_store/svn_import/tests.rs).

- **Why it changed.** A-920 piece 1 (0214fa210) slices an import revision's scratch commit a quantum at a time. Every
  staging step under the hog waits for one hold. At 500 ms a hold, the first batch's 151 staging steps took the test past
  its 120 s PATIENCE.
- **0214fa210** set HOG_NS to a 150 ms literal. The test passed in 61 s, but the literal broke R3.
- **d02143977** replaced it with a derived hold:
  - The test first imports `full.dump` alone into repository "calm" and takes `durable_flush_ns()` as the hold. Then it
    forgets that flush and runs your hogged import with `hog(hold_ns)`.
  - The `/// Derived:` line on the hold: the hogged landing's await holds its flush and at least one hold. So F, the flush
    alone, is under it, and F read as the await (the red) equals it.
  - Your `F < awaited` assertion is unchanged.
  - Now: 3.5-5.3 s per run, the await 0.45-1.1 s against F 5.6-9.8 ms (2 runs, M-series, debug).

Separately, main has sent you the arena test's flake, `an_imports_arena_keeps_no_more_resident_free_than_its_bound`. It
read 4,587,520 B against 4,210,688 B once in a parallel run and passed alone 3 of 3 times. If the hold or anything else
here should be different, tell me and I'll change it.
