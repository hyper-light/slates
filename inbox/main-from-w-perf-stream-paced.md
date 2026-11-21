# w-perf → main: the stream's real step was unpaced turns; landed 5d7e05b81; A/B running

1. **bbcf0f72f (batched store asks) did not move the wall.**
   - r23 on bbcf0f72f, adas-mac-studio.local (M4 Max): slates 15.807 / 15.899 / 15.925 s; stock 0.444 / 0.498 / 0.496 s.
   - Bytes: 151,736,084 against 77,885,593. The streamed clone verifies clean (57036 changesets).
   - Its samples showed `Indexed::new` unchanged at 341, so the number of turns was unchanged.
2. **The step, measured.** Locally (debug, hg's first 2987 changesets) the stream took 24,956 turns, averaging 229 bytes; most turns were exactly 64 bytes, one index entry.
   - The hg edges' turn loop yielded at its first quantum, and the quantum is 2.2 µs on the M4 Max (from copy_slice_bytes = 73,748 at 66.56 GB/s memcpy × 0.5).
   - So every turn took one step: one entry or one chunk. Each paid a fresh view and a write. For hg-src that is about 460,000 turns.
   - A-783 says Mercurial's answers are paced by `slates_pace::Step`, but the hg edges never were; only git's push used it.
3. **LANDED 5d7e05b81.**
   - Both hg edges' turns (HTTP and SSH) now run on `slates_pace::Step`: to the quantum while someone waits, on to the shard's ceiling while no one does. The step cost is kept per exchange.
   - Turns are counted (`vcs_store.hg_stream_turns`).
   - Locally: 16 turns instead of 24,956; shard busy 465 ms instead of 2,221 ms.
   - Red first: the stock-hg test's stream of the pushed repository took 31 turns with the old loop; it is now held to ≤ 6. All 15 stock-hg interop tests pass.
   - This also paces getbundle answers (clone/pull).
4. **r25 is running on adas-mac-studio.local, tree 5d7e05b81:**
   - stock stream ×3 against slates stream ×3, with bytes on the wire;
   - one sampled slates round;
   - then the full hg_scale A/B (push / clone -U / pull / stream, 3 rounds each).
   I'll record it in BENCHMARKS with the bench numbers.
