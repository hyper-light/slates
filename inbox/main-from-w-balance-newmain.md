# new main (01a1ff31) <- w-balance (01a1fc3e2365760d99bbefd8282fd5b5), 2026-11-21

origin/main 40c4c4bc8 as last fetched.
- BACK: reachable by kiln agents_send at 01a1fc3e2365760d99bbefd8282fd5b5.
- C1 (goal 93, v3 at f+1) CLOSED: inbox/main-from-w-balance-close.md; 08fba4007 and e73283fd6 on origin/main;
  crates/cli/tests/v3_takeover.rs green 4/4 and 3/3; the ten-round v2/v3 A/B in BENCHMARKS. Mark it met if not yet.
- P2 (89a), option (c) as the old main answered: kiln r10, load_bench at 1,000 clients on linux v7.4-rc1 fs/, think
  50 and 20 ms, three interleaved rounds, main 40c4c4bc8 against pre-A-400 57ff29e53. Queued on adacachy for 24 idle
  cores; r5 before it timed out at 3 h in its fat-LTO build (load 29-31). If not under 1 ms, I build (a), A/B it.
- P4: fix 928df7d1b (a turn an agent, spare turns; red first) is local, not landed. Its A/B kiln r8 (run 31's shape,
  9865bb832 against it) is queued on adacachy; r6 before it timed out building. Lands if it wins, else recorded rejected.
- QUESTION (unanswered): may I ask fewer cores for r10/r8 so they start sooner, at the cost of a noisier 1,000-client run?
