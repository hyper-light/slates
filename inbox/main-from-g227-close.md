# main, from g227: closed items

## 85b, g227's part: the catch-up A/B, winner landed (closed 2026-11-20 PST)
- Winner: the catch-up pair (retain-1 and catchup-1, with the `retains_after` sibling), already on main since
  **59ec78713**. The A/B's record: **2531110bf** ("docs: the catch-up A/B on today's main ..."). The flow fix it
  needed: **ec1f2ab1e** (landed in b80f5699f). The flow itself: 5763febd8.
- Measurement: `SLATES_TEST_CLI=1 SLATES_CATCH_UP_BACKLOG=20000 cargo test --release --locked -p slates-cli --test
  live_chaos -- --nocapture --test-threads=1 --exact a_resumed_holders_catch_up_beside_a_busy_volume`, ABBA, 4 runs
  each. A = main with the flow fixed; B = the same with the pair reverted (837c3be2d, never main).
  - GitHub ubuntu-24.04-arm, 4 CPUs, load about 2, 2026-11-20 23:20 UTC: A caught 20,000 behind (about 180 MiB) in
    2.73, 3.86, 4.19 and 8.95 s, 0-1 installs. B timed out at 1,200 s, 4 of 4.
  - GitHub ubuntu-latest x86_64: A 4.47, 3.43, 3.84 and 3.89 s. B timed out at 1,200 s, 4 of 4.
  - Adas-Mac-Studio, load 9-49, kiln r12: best of 4 is 8.8 s for both. B's 20,000 writes with the holder stopped took
    344-1,134 s against A's 18-43 s.
  - adacachy (load 72-77): no runs; its release build did not finish in kiln's 4 hours.
- The Nov 12 arm gap (9.4-13.3 s against 2.2-2.4 s) does not reproduce on today's main. GAPS line closed; every run is
  in BENCHMARKS, "The catch-up on today's main". Results branches: diag-results/g227-catchup-2-{arm,x86}.
- Noted in GAPS, not worked (no new item): each exchange to a lagging holder is capped at one congestion window
  (`take_batch`).
