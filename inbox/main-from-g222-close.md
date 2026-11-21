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
