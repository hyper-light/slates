# w-provision-gate progress (brief: briefs/w-provision-gate.md)

## State
- 2026-11-20 13:30 PST: started. Clone ~/slates-work/w-provision-gate (own clone, local branch main, push HEAD:main).
  Target ~/.cache/w-provision-gate-target.noindex. Base 37c84db43.
- Record found: GAPS §8a-r9-gate (2026-10-20): p99 ~1.7 ms both arms on x86 load 8-27/32 cores; gate absent.

- 14:00-15:45: bench gained PROVISION_BENCH_LOAD (spinners/CPU) and an A-559 split (Client::reply_stamp). Loaded p99
  reproduced here (0.99-2.05 ms). Diag stamps (uncommitted, scratch) named the hop: reply task -> serve task on the
  client's shard, with Shard::run's busy-path harvest_io (kevent zero timeout naps+switches on macOS) between them.
  A/B arms in ~/.cache/w-provision-gate-arms (main, imm, fix[+DIAG_SPIN_YIELD]); results ab1.txt, ab3.txt.
  Uncommitted WIP in ~/slates-work/w-provision-gate: kqueue.rs kevent64 sys layer + Driver::poll_now; DIAG code
  (crates/server/src/diag_stages.rs, DIAG_* statics in rt lib.rs/shard.rs, bench DIAG knobs) must be stripped before commit.

## Queue
1. Measure where the loaded p99 goes (instrument provision_bench), on this Mac (M4 Max 14 cores) quiet vs loaded.
2. Red: loaded-p99 bound derived from quiet p99 + load; fix.
3. Gate: provision_bench assertions + ratchet (floor only tightens), histogram recorded per run; CI wiring = run request.

## For main to delete
(none)
