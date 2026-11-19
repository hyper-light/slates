# perf → g257: handoff (2026-11-19). Notes only; never to land.

kiln answered `no agent` for g257's id from perf's side, so this branch carries the answer. Commits named "local
only" are in `~/slates-agents/perf` on Ada's MacBook (e30189ea…), unpushed because they never passed the full gates.

## 1. W1 step one

- W1's off-owner works are on main (`crates/server/src/work_home.rs`).
- perf's branch `agents/perf-w1`:
  - **origin c349a42a:** the last gated tip. It holds A-247 step one, the harness filing operations by outcome, the
    BENCHMARKS end-to-end tables, and the GAPS correction (main's drops were acknowledgement parts).
  - **local only e3fdd542:** main 29ef4521 (B11) merged in.
- That merge broke fleet's `a_submit_off_its_greens_shard_is_held_to_its_quorum_and_the_works_next_verb_behind_it`:
  - `fleet.rs:11943`, "the submit answers its version: Some(Err("Overloaded { shard: 0 }"))";
  - 3 of 3 at loads 11–239, once already at the first edit (`fleet.rs:11880`);
  - it passed on the Studio before the merge;
  - suspects in B11: `forward_lane.rs` (309 lines changed), `fleet.rs`, `daemon.rs`.
  - Worth checking whether main still fails it.

## 2. A fair A/B

- **Machine:** adas-mac-studio.local-2 (32 CPUs, 103 GB, 23 shards derived). Check idle cores first: 8–11 of 32 idle is
  not quiet.
- **Arms:**
  - each from its own source copy, patched with the owner-CPU diagnostic;
  - the same harness in both: W1's `crates/client/examples/load_bench.rs` copied in, and the example binaries
    compared with `cmp`;
  - each arm's `slates` and `examples/load_bench` copied to `bins/<arm>-cpu` right after its own build.
- **Shared target dir trap:** every tree gets one artifact hash, so run `cargo clean -p slates-server`, or touch every
  source, before each arm's build. Otherwise both arms run one binary.
- **`rung.sh <label> <bound-s> <harness args>`:**
  - starts `slates --instance $I anchor --quick`;
  - waits for `volume list`, then runs `bootstrap root`;
  - samples the daemon's RSS each second;
  - runs `examples/load_bench --instance $I …` under `perl -e 'alarm shift; exec @ARGV' $BOUND`;
  - keeps `bench.log`, `anchor.err` and `slates status --json` in `runs/<label>`.
- **Harness arguments:** `--tree <linux-v7.4-rc1>/kernel --clients N --hot-files 0.8N --per-connection 32 --rounds 3
  --reply-patience 60 --think-us T`.
- **`A=<arm> B=<arm> tools/ab-e2e.sh <tag> <clients> 5 <thinkA> <thinkB>`:** the arm order alternates each round. Per
  run it parses:
  - wall time, and verdicts a second (the oracle's accepted plus conflicts, over the wall);
  - oracle failures;
  - `lost xshard.reply_dropped`, and the harness's `backpressure` and `resent`;
  - `OWNERCPU` from `anchor.err`: owner µs a verdict and a rebase, and merge-util = (verdict_ns + rebase_ns) / wall;
  - each verb's p99 per attempt (`latency` rows) and end to end (`operation` rows), plus Cycle and Round.
- **Equal load:** think 19,000 µs at 3,000 clients and 6,000 µs at 1,000, to match main's verdict rate. Check verdicts a
  second.
- **Diagnostics,** never committed, in the Studio's `tools/`:
  - `owner_cpu_patch.py`: the owner's verdict and rebase timed by thread CPU, logged at the reap tick;
  - `engine_split_patch.py`: `submit_resolved` and `Green::rebase` timed apart from the rest;
  - `footprint_patch.py`: the green's bytes by kind, logged as GREENFOOT;
  - `drop_site_patch.py`: dropped results counted by sender.
- **Raw logs,** if not reaped: the Studio's `~/.cache/slates-agents-scratch/perf/runs/` (`e2eb3000`, `e2eb1000`,
  `eqb1000`, `eqc3000`, `dc3000`, `w1dc3000`, `split-*`, `fp-*`, `prof-w1`, `prof-dbg`, `dropse-*`). perf's local scratch
  was deleted during the week.
- **Results** are in BENCHMARKS on main: "End to end and at equal offered load", "Why main's two figures differ", "What
  a verdict compares and copies".

### Key numbers (W1 against main db9c514e)

- **3,000 clients, unpaced:** W1 better end to end on every verb. Accepted submit p99, main 65.6–84.9 ms end to end
  against W1's 56.4–64.7.
- **1,000 clients, unpaced:** W1's submit and rebase worse at 2.0–2.3 times the verdicts. Accepted 34.9–43.9 against
  32.3–37.5; rebase 35.7–44.0 against 20.7–37.4.
- **Equal load:** W1's verdicts faster at both sizes; its rebase at 1,000 slower (rounds' medians 30.7 against 21.2).
- **Owner at 3,000 clients:**
  - a verdict 21.4–21.7 µs, of it the engine 13.0–13.1;
  - a rebase 12.3–12.4 µs, of it the engine 9.5–9.6;
  - merge work 51–53% of the owner's wall.
- **The supervisor's last W1 rule:** W1 lands if no worse end to end at 1,000 and better at 3,000, measured against main
  with perf-replies.

## 3. perf-replies

- `agents/perf-replies` 2a5e10dd is on main, with no dependencies left.
- What it does:
  - replies wait for room while their shard lives (`send_back`, `given_room`, `carry_home`);
  - waits are counted (`xshard.room_waits_started`, `_ended`, `_peak`);
  - the bound is the requests in flight and the shard's task arena, where a spawn is refused typed;
  - `reply_room.rs` holds its precondition.
- Its Linux suite never ran: kiln's sync failed.
- Main's 3,000-client drops were acknowledgement-scatter parts (4,364 of 4,364), costing about 27,000
  `AcknowledgementOwed` resends a run.

## 4. A-273 memory

- Main built A-273's replicas, then measured, rejected and removed them (33223939). Read that first.
- perf's measurement: W1 at the 3,000-client rung's end, head 9,000, two runs alike to a few hundred bytes.

  | part | bytes |
  |---|---|
  | current file bytes | 16,438,721 |
  | history chunks | 18,859,529 |
  | content history index | 8,991,756 |
  | deltas (9,000 versions, 44,997 ops) | 4,642,109 |
  | path, last-changed and kind indexes | 613,120 |
  | idempotency and rejected caches | 4,183,041 |

- A metadata-only replica is about 14.2 MB. That's 313 MB at 22 shards, and 3.8% of the Studio's `table_bytes`
  (373,475,417; `reserve_per_shard` 1,493,901,668; 23 shards).
- perf's draft text is on local-only `agents/perf-a273` b2b46f83, superseded.

## Also local only

- `agents/perf-decide` 1a4c6646: a verdict makes no file's bytes until it commits or is restated (`Effect::Splice`), and
  `apply` reserves exactly the length it writes.
- Why: in W1's profile, `apply` was 91 of 283 engine-verdict samples.
- The merge crate (177) and the server suites pass. No daemon A/B. It predates a week of main.
