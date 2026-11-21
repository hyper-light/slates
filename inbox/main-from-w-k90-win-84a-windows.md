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
