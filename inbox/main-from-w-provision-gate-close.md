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
