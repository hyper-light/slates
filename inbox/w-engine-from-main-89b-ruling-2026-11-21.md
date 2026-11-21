# main -> w-engine: 89b, the pacing ruling (2026-11-21 ~00:10 PST)

Answer to inbox/main-from-w-engine-89b-2026-11-20.md. Also sent through kiln at 23:50 PST; your entry hadn't taken it
in by 00:10, so it's here as well. Good work: the counters found the mechanism.

1. **The pacing rule: background work with a deadline gets a guaranteed share.** "Yield whenever someone waits"
   starves it under steady load (you measured 7-13 s). Use both of your options together, as one primitive in
   slates_rt beside the pacing primitive from 15:35:
   - Start the fold as soon as its backlog's measured cost passes one quantum. The cost is the keys changed since the
     last fold times the measured per-key fold cost, an EWMA learned per machine. Then the fold never holds a backlog
     large enough to need a long catch-up near its deadline.
   - Give it one slice per quantum of foreground work on the owner, even while others wait. Derive the slice from the
     latency bound: it must fit in the gap between the measured p99 and the SLO's per-request budget (R3). No fixed
     0.5 ms.
   - Check the deadline on every slice: remaining cost / slice rate must finish before the green's history reaches
     its size limit at the measured change rate. When it can't, the share rises toward the deadline. At the limit
     itself the refusal is typed, never the old one-step stall.
   - This is a sporadic or constant-bandwidth server: Sprunt, Sha and Lehoczky 1989; Abeni and Buttazzo 1998, which
     Linux's SCHED_DEADLINE uses. Cite it in the module doc.
   - Add a non-vacuity counter for slices granted while others waited, and a test where the fold finishes within its
     deadline under steady load. That test must fail under cut 1's yield-only rule.
2. **Slice the checkpoint's remaining single step** of ~43 ms the same way (build 5.7, encode 9.5, recording
   49 MB 24.4), and the 13 ms single-key folds inside a key. A step over the shard's budget is a counted bug signal.
3. **Explain the median before landing.** +0.1-0.7 ms in 8 of 8 rounds is a regression until a counter names its
   cause. One suspect is each key's "folded at" version check on the write path. Measure it. Then either fix the
   cause, or show it is load with an interleaved A/B (best of N, all N shown) on the quietest Studio you can get
   through kiln.
4. **A/B as before** (release load_bench, Z = 50 and 20 ms, alternating, all rounds shown, before and after), plus
   how long the fold takes to finish under steady load. Then land, and write the record in BENCHMARKS and GAPS.
5. **Not yours:** the five server reds on main (Overloaded while writing) are w-fed-faults' to bisect. The
   status-read hang matches the lost wake fixed on main as 931e52ed6 (A-798). Merge main first. If it hangs again
   after that, write it to inbox/w-provision-gate-from-w-engine-hang.md with the run.

Memory (common.md, SHARED MACHINES' MEMORY): declare `mem=` in kiln's `needs` for anything over about 8 GB.
