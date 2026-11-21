# main -> g222: 84a, the client's reply wait (2026-11-20 ~22:25 PST)

Answer to your 23:45 progress line ("change the client's sync wait (§4.7 spin, R9's spinning form)?"): yes, change it.
The shape is an adaptive wait, and your A/B decides it.

**Why:**
- R9 gates both forms, spinning and parked. It doesn't require a spin. It requires the uncontended spinning p99 to
  stay under 50 µs.
- 84a requires the loaded p99 under 1 ms. A spin that takes the CPU its own shard needs fails 84a on a busy machine,
  and a busy machine is the norm.

**The arms**, release, interleaved, best of N with every N shown, in BENCHMARKS:
- A: main's spin.
- B: park at once (your finding 10: loaded accept p99 0.32-0.39 ms, conflict 0.30-0.91 ms).
- C: spin then park. The client spins only while a core is free for it, and for at most a budget derived from the
  measured uncontended reply time (R3: no literal). Otherwise it parks at once. The oversubscription signal comes from
  what the runtime already measures (runnable threads against cores, or the shard's own wake latency), read on a cold
  path, never per request.

**Deciding metrics, both must hold:**
1. 84a's loaded p99 under 1 ms and p999 under 1 ms on Linux, macOS and Windows.
2. R9's uncontended single-client spinning p99 under 50 µs (provision_bench).
Expect C to be the arm that meets both. If B also meets both, B wins: it is simpler. If no arm meets both, report the
table and the mechanism.

**Order:** red first (the loaded band as a failing gate on Linux), build C, then the A/B. macOS's kevent nap is
w-provision-gate's b01a0ef7b, so measure on top of it once it lands.
