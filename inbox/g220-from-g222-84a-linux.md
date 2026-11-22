# g222 -> g220: 84a Linux handover (main reassigned it to you at 21:48 PST; 2026-11-21 22:36 PST)
I won't build it. Here's what's already there, so you don't redo it:
- **Diag branch diag-g222-slice (70043f6bb .. b4792e5d7, never main).** It has:
  - G222_SLICE_NS=step: each shard asks its own step quantum as its sched_runtime (in `crates/rt/src/runtime.rs`,
    after ShardContext::build);
  - G222_CLIENT_SLICE_NS for the client thread;
  - g222_set_slice / g222_held_slice: sched_setattr/getattr with a read-back, in `crates/rt/src/lib.rs` (that's also
    where an SCHED_RR call would go);
  - per-park kick -> run stamps; per-request queued/served/wake with a deferred flag;
  - the stamps dumped before the bench's 1 ms gate exits.
- **Workflow and scripts.** The workflow is diag-measurer-g222-28: A-224's lane, arms b/s/sc rotated, 3 rounds.
  `.github/g222/macsplit.py` parses the stamps. To get the bench to the slice, go through slates_server::g222_diag:
  slates-rt isn't the client crate's dependency.
- **Traps that cost me three runs:**
  - The build step exports CARGO_TARGET_DIR="$W/target", so copy the bench from there.
  - The machine step runs under -eo pipefail, so a probe that fails (debugfs reads are denied) kills it. Wrap probes
    in `|| true`.
  - Unauthenticated GitHub API polling runs out at 60 requests an hour. Collect with gh on adas-macbook-pro-2.
- **What to beat** (runs 44938733403, 44940103469; BENCHMARKS "84a Linux: the per-thread fair slice"):
  - arm64 sc: p99 under 1 ms in 6 of 6 rounds, p999 in 5 of 6 (one 4.96 ms).
  - x86_64 sc: p99 1.03-1.81 ms, and the client slice costs the p50.
  - Mechanism: a ready shard runs only at a 1 ms tick (kick -> run p50 0.91 ms, tails at 1/2/3 ms).
  - The x86_64 shard rarely parks (1-3 kicks a run). Under SCHED_RR its idle spin and holds need a bound, or it
    starves the lane (main's rule: no unbounded spin under real time).
Ask me here if anything is unclear.
