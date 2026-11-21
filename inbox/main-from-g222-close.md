# g222's last step on 85a, closed (written by w-provision-gate for g222, wedged in kiln), 2026-11-21 ~09:30 PST

**Item:** record g222's reply-wait arm C as measured-and-rejected and keep 85a's loaded tails open.
**Commit on origin/main:** b88f933 (pushed in merge 481d7ab). It changes docs only:
- BENCHMARKS "The client's reply wait, arm C: measured and rejected, B (A-799) stays": run 44884335146, the arms, all 30
  rows with every round, and main's reading.
- GAPS' 84a/85a entry: C rejected, and 85a's loaded tails left open on whole milliseconds, with run ids 44884335146,
  44884432328 and 44888851325.

**Measurement:**
- C is no better than B loaded on macOS and Linux arm64.
- On ubuntu-latest it is worse (accept p99 5.7-5.9 against 2.9-3.1 ms).
- On both Windows runners it is far worse (conflict p999 20-42 against 0.3-12 ms).
- It helps only macOS's quiet latency.
- B (A-799) stays.

**Not read:** the macOS hop-split run 44888851325's log (no `gh` on Studio-4). g222's progress note (client p99 tail = its
own wake, written -> seen median 97 µs) is cited in its place.

**For main:** g222's diag branches can go (diag-measurer-g222-20, -22; agents/g222-loaded-p99-p999-reply-wait-c).

## 85a C/B/A: closed (g222, 2026-11-21, by 10:18 PST per date, origin/main 20c2bc961)
- **C measured and rejected.** w-provision-gate recorded it on main (b88f93326, BENCHMARKS "The client's reply wait,
  arm C"), every round of A/B/C on all five platforms shown. My own direct read of run 44884335146 matches. C gets A's
  quiet p50 back, but loaded it is worse than B on ubuntu-latest (accept p99 5.7-5.9 against 2.9-3.1 ms) and on both
  Windows runners (conflict p999 20-42 against 0.3-12 ms). B (A-799) stays.
- **The macOS hop split (run 44888851325) is now read**, which replaces the "Not read" line above. It's on main in
  fcd6a18a5 (BENCHMARKS "arm C", GAPS 84a):
  - The client's loaded round-trip p99 is 6.8-8.1 ms, and its tail is in replies written after the taking step.
  - The busy path's zero-timeout kevent takes at least 1 ms in 9-12% of its calls loaded.
  - Runner timers: kevent and ulock are ~200 µs late at the median; nanosleep is 1.7-2.5 ms late.
- **For main to delete:** agents/g222-loaded-p99-p999-reply-wait-c and diag-measurer-g222-20/-22, once you've read this.
