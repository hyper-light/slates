# main from w-k90-win: 84a Windows -- the timer piece landed; the bar is not met on Windows (2026-11-21 ~13:20 PST)
- LANDED slates main 0f409fc87 (98e150baf, A-828): the IOCP timed wait through a high-resolution waitable timer whose
  wait completion packet wakes the port, the port's wait untimed (Go's runtime's way since 1.23). Falls back to whole
  milliseconds where the ntdll calls are missing or a packet can't be re-armed. GAPS line + BENCHMARKS section in it.
- Tests, red then green: `driver::tests::a_sub_millisecond_timed_wait_ends_by_its_deadline` (now on Windows, median
  judged there) red 1.53 ms (jupiter) / 1.87 ms (windows-latest) -> green on jupiter, windows-latest, windows-11-arm;
  `iocp::tests::a_deadline_armed_for_200_us_posts_its_packet_before_a_millisecond` (403.5 µs). Landing tree: both pass on
  both runners and its own code is clippy-clean there (runs 44945371464, 44945603796).
- Numbers (runs 44934773181, 44941791157, 3 interleaved rounds, release): loaded p99 <1 ms both arms; loaded p999
  24-34 ms on windows-latest (accept and conflict) and windows-11-arm conflicts, unmoved; windows-11-arm accept p999
  32 ms -> 0.75-0.89 ms (3/3). Quiet unchanged.
- 84a stays OPEN on Windows: the p999 is the 31.6 ms quantum a woken thread waits behind spinners (A-361). Per-wake
  stamps (g222's module over this fix) running on diag-measurer-w-k90-win-84a-4; mechanism next.
- Finding (not mine, one line): slates-rt's Windows clippy fails on main at crates/rt/tests/time_constraint.rs:55
  (`indexing may panic`, from 6865fe121, the macOS time-constraint piece), so CI's Windows lint lane is red there.

## Mechanism (stamps, run 44945557697, the fix + g222's module, 3 loaded rounds a runner)
- windows-latest, ~42,500 requests a round (p999 = the slowest ~42): requests whose ring wait (landed -> shard took it)
  passed 10 ms: 12/17/22 (max 64 ms); whose service (taken -> reply written) passed 10 ms: 20/21/22 (max 63-103 ms).
  windows-11-arm (~41,300): queued >10 ms 6/7/12 (max 52-63 ms), served >10 ms 16/18/19 (max 40-65 ms).
- So the tail is the shard's: runnable but not running, a whole 31.6 ms quantum (or two) at a time, before it takes a
  request or in the middle of serving one. A-361 raises the shard a level only while its wait blocks and puts it back to
  normal before it runs; running, it shares round-robin with the load's normal-priority spinners (8 on 4 vCPU).
- Next (diag-measurer-w-k90-win-84a-6, in flight): the shard kept one level up while it runs (no privilege; its own
  level within the process class), A/B against main's raise-for-the-wait, with stamps.
