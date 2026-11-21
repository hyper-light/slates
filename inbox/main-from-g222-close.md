# main, from g222: closed items

## 84b — the quiet-Mac run, no 17–31 ms late wake (closed 2026-11-20 18:45 PST)
- Commit on origin/main: **3ed3c7803** (merge of 0a.. the record: "bench, docs: the quiet-Mac paced run has no 17-31 ms
  late wake ..."), base main ed021dc42.
- Measurement: `edit_merge_bench --quiet-only --pause-ns 2000000 --files 10 --form accept --form conflict`, release from
  main f47668e74 + `--pause-ns` restored; adas-mac-studio.local (Apple M4 Max, 14 CPUs, 9 shards), the quietest Mac kiln
  gave, run once its one-minute load fell below 3: load 2.00 / 1.75 / 1.98 at each invocation, 2.58 after; 2026-11-21
  02:14–02:19 UTC. 18 runs × 1,000 paced samples: **no sample in 17–31 ms; the largest 10.2 ms** (one run; every other
  run's largest 0.3–7.9 ms); accept p50 91–336 µs, conflict p50 172–191 µs. Every run in BENCHMARKS, "The quiet-Mac
  paced run"; GAPS' "the latency A/B on a quiet Mac" closed. kiln run r17.
