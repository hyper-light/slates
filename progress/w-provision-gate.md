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

- 2026-11-20 ~18:40: LANDED 2c8f89de6 on main (A-786 part 1: the gate's lane + bench assertions + loaded bound, red
  1.62 ms vs 0.87 ms; docs: design R9 note, GAPS head + 8a-r9-gate, BENCHMARKS). Run request filed (CI wiring, clippy,
  Linux measurement).
- Part 2 on branch agents/w-provision-gate-kqueue (b01a0ef7b): kqueue -> kevent64, zero timeout IMMEDIATE; driver test
  red on main (14.6 us vs 14.9 us sleep), green. NOT for main alone: p999 regresses via A-387 holds (BENCHMARKS table).
  Worktree ~/slates-work/w-provision-gate-land (branch agents/w-provision-gate). Scratch clone ~/slates-work/w-provision-gate
  has DIAG-free tree + local wip merge (do not push it). Arms/logs ~/.cache/w-provision-gate-arms.

## Queue
1. DONE measure; DONE gate (2c8f89de6); DONE red (bench loaded bound).
2. NEXT: A-387's held-turn estimate forgets by turn count (cheap turns forget a ms preemption in microseconds); weigh
   turns by time (horizon = the hold's length), A/B main vs kqueue+new rule vs kqueue+no-hold (loaded and quiet, macOS),
   then land kqueue + rule together with the bench's loaded bound green. Linux needs its own hop split (run request).

## For main to delete
- ~/slates-work/w-provision-gate (scratch clone, after part 2 lands)
- ~/.cache/w-provision-gate-arms, ~/.cache/w-provision-gate-fix (scratch)
