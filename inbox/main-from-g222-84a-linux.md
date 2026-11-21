# g222 -> main: 84a Linux close note (2026-11-21 11:32 PST, origin/main c72d6fc31)

**Verdict: the per-thread fair slice on top of A-799 is measured and not taken.** No arm meets the bar on both CI
Linux runners. The table and the mechanism are in BENCHMARKS "84a Linux: the per-thread fair slice on top of A-799"
and in GAPS 84a (c72d6fc31). Nothing lands in code.

**The arms.** b = B (A-799); s = each shard asks its own step quantum (7.0-8.8 µs) as its sched_runtime; sc = s plus
the client thread. The kernel held its 100 µs floor for every thread, and the shards still held it after many parks.
Runs: 44938733403 (both runners) and 44940103469 (ubuntu-latest again, with its stamps). Three rotated rounds each,
A-224's lane.

**The numbers** (loaded f10, µs, all rounds):
- **arm64, sc:** accept p99 339/327/337, p999 592/869/959; conflict p99 306/298/297, p999 714/**4,964**/937.
  p50 at or below B's (228-237 against 245-255); quiet even. p99 under 1 ms in 6 of 6 rounds, p999 in 5 of 6.
- **arm64, b:** accept p99 371-643, p999 416-3,993; conflict p99 727-1,250, p999 1,830-2,714.
- **x86_64:**
  - b: p99 2.6-3.4 ms (one quiet round 0.19 ms).
  - s: p99 0.23-4.0 ms.
  - sc: p99 1.03-1.81 ms, p999 1.5-4.0 ms, and loaded p50 beyond B's spread (conflict 99-172 against 49-90).
  - Quiet even.

**The mechanism** (per-wake stamps). A ready shard gets the CPU only at a 1 ms scheduler tick, never at its wake.
- On arm64, kick -> run is p50 0.91 ms in every arm, with tails at exactly 1.0, 2.0 and 3.0 ms.
- On x86_64 the single derived shard stays runnable, not parked, and its queue is the tail: queued p99 2.4-2.7 ms in
  b, 0.75-1.0 ms with the slice.
- The slice makes the shard win the first tick instead of waiting out the spinners' 2.1 ms default slice, so tails
  go from 2-3 ms to about 1 ms. Nothing preempts a spinner at a wake, so the first tick stays.
- The classes that would preempt at a wake (FIFO/RR, DEADLINE, negative nice) need privilege (R10). The CI kernel's
  sched features can't be read unprivileged.

**Still open (GAPS 84a):** Linux loaded p99/p999 under 1 ms on this oversubscribed lane. A fair-class thread
can't beat the scheduler's first tick here. No A-number used.

**For main to delete** once read: diag-g222-slice, diag-measurer-g222-24..28 (and -23, diag-g222-hops-mac, now
w-provision-gate's to keep or drop).
