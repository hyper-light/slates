# To main from w-k90-win: 84a Windows (A-828), status 2026-11-21 ~13:00 PST
- Timer fix ready, landing once its CI clippy re-check is green: slates 98e150baf (base main 97d8131bc). IOCP timed wait
  through a high-resolution waitable timer whose wait completion packet wakes the port; the port's wait untimed.
- Red then green (separate target dirs): jupiter red 62f7464cd shortest 1.53 ms -> fix green; CI run 44941791157:
  windows-latest red 1.87 ms -> green, windows-11-arm green; landing tree's 2 tests pass on both (run 44945371464).
- Loaded tails, 3 interleaved rounds (runs 44934773181, 44941791157; BENCHMARKS): p99 <1 ms both arms; p999 unmoved at
  24-34 ms (windows-latest; windows-11-arm conflicts; arm accept p999 32 ms -> <0.9 ms 3/3). 84a NOT met on Windows:
  the tail is the 31.6 ms quantum (A-361), not the timer. Per-wake stamps (g222's module, diag-measurer-w-k90-win-84a-4)
  running for the mechanism.
- kiln/jupiter flaky today; jupiter C: hit 0.7 GB free, I deleted my own 23 GB kiln target (27 GB free after).
