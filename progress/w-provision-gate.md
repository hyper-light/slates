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

- 2026-11-20 ~17:10 (close list: C1 then P2): C1 wiring LANDED 2157188b4 (kiln ci+nightly exclusive bare-metal
  provision-gate-linux/-macos; GHA provision-histogram; xtask lane informational w/o baseline, always loaded form).
  By use: local lane exit 0, 28 rows kept. Baseline: adacachy (Linux, 451397…) at load 90-104 -> not recorded (r2
  stopped); kiln r3 (`macos exclusive`, `--record`) queued — when it ends, commit its ratchets.toml diff to main, then
  write inbox/main-from-w-provision-gate-close.md for C1. Worktree ~/slates-work/w-provision-gate-ci.

- ~18:30: P2 in ~/slates-work/w-provision-gate-land (branch has b01a0ef7b kqueue; uncommitted: shard.rs idle_turn_rest =
  macOS explicit sleep(1ns) per idle-spin turn). Saved variants in ~/.cache/w-provision-gate-fix/p2/ (held_turn.timew.rs +
  shard.timew.rs = time-horizon estimator, unit tests 7/7, red vs old 400 ns). ab5 r1 (kqueue+timew): loaded p99 2.00 ->
  0.46 ms but p999 13.0 -> 37.3 ms. ab6 running: p2main / p2fix(timew) / p2nap (kqueue + explicit macOS nap, old
  estimator), 3 rounds, ~/.cache/w-provision-gate-arms/ab6.txt. r3 (macOS exclusive --record baseline) lost its runner,
  requeued.

- ~21:00: C1 robustness LANDED 99c599ce5 (bound = max(quiet spinning p99, loaded parked p99); main red 3/3, candidates
  green 6/6). P2 A/B ab6 done (BENCHMARKS "the candidates"): time + nap arms meet R9 quiet (28-43 us), loaded p99 3-5x
  better, but loaded p999 5.6-23.4 vs main 2.1-2.5 ms and 2-6x quiet CPU -> NOT landed; P2 open (loaded p999 hop next,
  with g222). Land worktree's uncommitted shard.rs = nap arm; time arm saved in ~/.cache/w-provision-gate-fix/p2/.
  C1 remaining: kiln r4 (`macos exclusive`, ratchet --provision --record from the ci worktree) queued. When it ends:
  commit its ratchets.toml diff to main (area `ratchet:`), then write inbox/main-from-w-provision-gate-close.md for C1.

- ~18:50: C1 baseline still unrecorded. Per main 18:36 resubmitted by needs ("macos,cpus=16,disk=60g exclusive"):
  r7 and r8 both placed on d1323898 (big Studio) and both failed "did not answer the sync in 60s"; earlier r3/r4/r5
  failed the same way on b24c5c95 (draining). r6 (named cec37f60, adas-mac-studio.local) was stopped per main's no-naming
  rule. BLOCKER for main: the Studios' sync. To resume: rerun the same command by needs from
  ~/slates-work/w-provision-gate-ci, commit the ratchets.toml diff, write the C1 close note.

- ~19:20: C1 CLOSED: baseline landed b45a114e3 (kiln r9, sync:false clone on the big Studio, per main 18:35); close
  note inbox/main-from-w-provision-gate-close.md. Remote clone/target removed. Next: P2 (loaded p999 hop, with g222).

- ~20:30: P2 p999 hop named and docs landed 8edfb763e. Candidates' loaded p999 = cross-shard hops to a demoted
  true-spinning shard (7-29 ms); QoS rejected. P2 is a trade-off -> DECISION asked of main/Ada (land immediate poll or
  keep the nap). Land worktree holds uncommitted DIAG (diag_stages, verbs marks, bench stamps, runtime DIAG_QOS) on top
  of the time-estimator arm: strip before any commit. Arms/logs: ~/.cache/w-provision-gate-arms/ab7*, ab8*.

- ~21:30: P2 CLOSED on data (a38630375): option 3 failed all 4 of main's conditions; main's nap kept; branch
  agents/w-provision-gate-kqueue deleted; close note appended; g222 told. QUEUE DONE (C1 b45a114e3, P2 a38630375).
  For main to delete: worktrees ~/slates-work/w-provision-gate-land (local merge commits + option-3 commit, nothing to
  land) and ~/slates-work/w-provision-gate-ci, the clone ~/slates-work/w-provision-gate, ~/.cache/w-provision-gate-*
  (arms, fix, targets).

- ~22:00: per main, deleted my scratch on Studio-4 by exact path: ~/.cache/w-provision-gate-target.noindex (4.8 GiB),
  -arms, -fix, the kqueue .rs copies, and the clone ~/slates-work/w-provision-gate. Nothing of mine left to delete there.

## Queue
1. DONE measure; DONE gate (2c8f89de6); DONE red (bench loaded bound).
2. NEXT: A-387's held-turn estimate forgets by turn count (cheap turns forget a ms preemption in microseconds); weigh
   turns by time (horizon = the hold's length), A/B main vs kqueue+new rule vs kqueue+no-hold (loaded and quiet, macOS),
   then land kqueue + rule together with the bench's loaded bound green. Linux needs its own hop split (run request).

## For main to delete
- ~/slates-work/w-provision-gate (scratch clone, after part 2 lands)
- ~/.cache/w-provision-gate-arms, ~/.cache/w-provision-gate-fix (scratch)
