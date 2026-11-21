# w-provision-gate -> main: C1 CLOSED (2026-11-20 ~19:20 PST)

**Item:** C1, R9's provisioning gate wired as the permanent ratcheted CI gate (provision_bench: p50/p99/p999/max,
spinning and parked).

**Commits on origin/main:**
- 2c8f89de6: the lane `cargo xtask ratchet --provision`, the bench's loaded form and bound, every run's rows kept in
  target/provision-histogram.tsv, Client::reply_stamp.
- 2157188b4: wired into kiln CI and nightly (`provision-gate-linux`, `provision-gate-macos`, exclusive bare metal) and
  GitHub CI (`provision-histogram`, ubuntu and macos, the histogram uploaded). The loaded form is always run; the gates
  fail where a baseline exists and are informational elsewhere.
- 99c599ce5: the loaded bound made robust (max of the quiet spinning p99 and the loaded parked p99).
- b45a114e3: the first reference baseline, the big Studio (M3 Ultra, 32 cores), 15 ceilings.

**Tests and measurements:**
- xtask `every_provision_runs_rows_are_kept_in_run_order` passes.
- kiln validated both jobs (1 Linux and 7 macOS runners eligible).
- Run by use on this Mac, no baseline: exit 0, 28 rows kept, the floor's FAIL reported.
- The baseline run (kiln r9): 3 runs, 15 ceilings recorded, exit 0.

**Caveats:**
- The ceilings are loose: host load rose from 10-26 to 71-77 during the recording. A quiet exclusive `--tighten` lowers
  them.
- The Linux bare-metal machine (adacachy) has no baseline (load 90-104 when tried), so its job is informational until it
  records one.
- On main the gate's own floor and loaded bound are red on the Studio (spinning p99 188-202 us against 50). That is R9
  itself unmet (P2 and the open macOS floor item), not the gate.

P2 (the loaded p99) is open: both candidates are measured, neither landed (loaded p999 2-10x worse, CPU 2-6x); BENCHMARKS
"R9's loaded p99 on macOS, the candidates".

# P2 CLOSED on data (2026-11-20 ~21:30 PST): main's nap kept, options 2 and 3 measured and rejected

**Item:** P2, the loaded p99 (1.6 ms; the macOS zero-timeout kevent), fixed red first with g222. Main's 20:03 ruling: A/B
option 3 against main on four conditions; land only if all four hold, else keep the nap and close on the record.
**Commit on origin/main:** a38630375 (BENCHMARKS "R9's loaded p99 on macOS: options 2 and 3 measured and rejected", GAPS
8a-r9-gate closed), on top of the measurements in 8edfb763e and 99c599ce5.
**Measurement:** release, 3 interleaved rounds of 3 runs, all shown, macOS arm64 M4 Max, host load 2.7-21.7. Option 3
failed all four: quiet CPU 5.7-10.0 vs 1.6-1.7 cores; loaded p999 10.1-12.7 vs 2.1-3.4 ms; loaded p99 better in 2 of 3
rounds only; quiet p99 82 us in round 2. Option 2 failed alike. Branch agents/w-provision-gate-kqueue deleted.
The gate's loaded bound stays red on macOS: R9 under load unmet there, no daemon lever found, recorded.

# The bench end-of-run lost wake: FIXED (A-798, 2026-11-20 ~22:45 PST) -- unblocks 84a, 86a, 89b's measurements

**Item:** main's order: a bench's read hangs intermittently, Client::round_trip waiting forever with every shard parked
(w-engine's load_bench, twice on the big Studio; provision_bench). Find the lost wake, fix it red first, land.
**Commit on origin/main:** 931e52ed6 (rt: an idle spin harvests before it looks).
**The lost wake:** a shard's idle spin looked at its rings and pollers, then harvested the driver without blocking; the
harvest took any pending kick and reported only completions. A client ring published between the turn's look and its
harvest lost its doorbell kick (kqueue EVFILT_USER / epoll eventfd) to that harvest, and a spin ending on that turn parked
with the ring unseen and no kick left. Not the loom-checked park protocol: the kick was sent and swallowed. Fixed by
harvesting first, then looking.
**Test / measurement:** crates/rt/tests/spin_kick.rs, deterministic (the poller injects a ring + kick right after its own
look and holds the turn past the window): red on the old order 3 of 3 (ring never served, shard parked to the 20 s
backstop), green 3 of 3 (20-40 ms). Reproduced first by use: provision_bench with a 5 us window hung 1 run in 2 on main.
Focused tests green (macOS arm64, Studio-4): slates-rt spin_kick, pollers, idle_spin, readiness, tcp, --lib shard;
slates-server closed_loop, idle_window. Bug record docs/bugs/2026-11-20-a-shards-idle-spin-took-a-clients-kick.md.
**Owed (run requests, not mine under §8):** the same tests on Linux and Windows (epoll and IOCP harvests take kicks the
same way); w-engine's load_bench end-of-run read re-run on the big Studio to confirm the hang is gone.
