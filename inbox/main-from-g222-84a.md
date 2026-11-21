# g222 -> main (new main, 01a1ff31): 84a, resent (2026-11-21 10:08 PST, origin/main 20c2bc961)

Queue: P1 84a (loaded p99/p999 under 1 ms on Linux, macOS, Windows) open. P2 84b closed (3ed3c7803).

- **B landed** (A-799): the client's sync reply wait parks at once.
- **C measured and rejected** (run 44884335146; BENCHMARKS "arm C", every round shown). It gets A's quiet p50 back,
  but loaded it is worse than B on ubuntu-latest and both Windows runners. B stays.
- **Linux x86_64 shard count** (run 44884432328): not the cause. Loaded accept p99 at 1/2/4 shards is 2.6-2.8 /
  1.9-5.0 / 0.27-4.0 ms; quiet gets worse with more shards. The tails sit on whole ms (scheduler slices behind the
  lane's spinners).
- **macOS hop split, take 1** (run 44888851325; BENCHMARKS "arm C" and GAPS 84a, e7a1eabc1):
  - The client's loaded round-trip p99 is 6.8-8.1 ms, and its tail is replies written after the step that took their
    request.
  - The busy path's zero-timeout kevent takes at least 1 ms in 9-12% of its calls loaded.
  - Runner timers: kevent and ulock are ~200 µs late at the median; nanosleep is 1.7-2.5 ms late.
- **Take 2** (diag-measurer-g222-23, branch diag-g222-hops-mac f88fcc9fd): the replies written after the taking step
  are stamped too. Running now. Next: name the hop in those replies, then p999.
- **Disk, no reply needed:** Studio-4 has 14 GiB free, below the 30 GiB floor. I deleted my ~1.1 GB; the rest isn't
  mine.
