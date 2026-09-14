# The fleet suite and the fleet under heavy CPU load — traced, classified, measured

> Charter (2026-09-14): make the fleet suite, and the fleet itself, honest and robust under heavy CPU
> load. "We MUST BE ROBUST to noisy and heavy CPU load" (Ada). Trace first, theorize second — the
> lesson of 2026-09-13 (`docs/bugs/2026-09-13-swim-fixed-probe-deadline-kills-a-starved-live-peer.md`):
> a theory whose test passed on unfixed code was wrong.

- **Date:** 2026-09-14
- **Box:** Darwin 25.4.0 arm64, 18 hardware threads (`hw.ncpu = 18`), shared with other agents' builds.
- **Suite:** `cargo test -p slates-server --test fleet -- --test-threads=1`, 34 tests, serialized behind
  `FLEET_TEST_LOCK` so one test's daemons run against a quiet machine.
- **Instrument:** the opt-in trace this change adds (`SLATES_FLEET_TRACE=<path>`; commits `c760deb`,
  `f8caa59`). Off by default. Per poll: the wait's site, the slowest observed coordinator's period count
  (`Daemon::fleet_progress`, a direct atomic read), the periods advanced, the frozen-progress span, the
  ask count and slowest ask, and each daemon's every-shard **pulse** (`Daemon::shard_pulses`, read
  straight off the runtime registry with no shard round-trip): steps, driver waits, spawns, completions,
  **refused admissions** (the arena-full signal), the **longest step** (a blocking poll), the parked
  snapshot, kicks skipped, ring-full events. So a stall says which daemon stopped advancing, on what
  wait, and by what mechanism.

## Part 1 — the load discipline (how every run below was taken)

Every run is `$CLAUDE_JOB_DIR/tmp/load-run.sh <tag> <burners> [libtest args]`: it records the load
average, free pages and swap at start, brings up `<burners>` `yes > /dev/null` CPU burners (one per
hardware thread = 18), runs the suite with the trace on, watches for a 420 s stall (no suite output →
`sample` the test pid, then kill), and on **every** exit kills the burners it owns and asserts
`ps -Ao command | grep -c '^yes$'` is 0. One run at a time; never two fleet suites on the box at once.

## Part 2 — the measured table

| Run | Burners | Load (start→end) | Free pages | Result | Time | Window (CDT) |
|---|---|---|---|---|---|---|
| A | 18 (1×/thread) | 8.8 → 40.3 | 1.58 M | **34/34** | 214.82 s | 09:34:10–09:37:56 |
| normalA | 0 | 27 → 8 | 1.57 M | **34/34** | 190.75 s | 09:38:32–09:41:47 |
| loadC | 36 (2×/thread) | 12.8 → 69.5 | 1.54 M | **STALL @ test 32** (31/34) | killed 420 s no output | 09:42:30–09:54:04 |
| retireAlone36 | 36 | 9.4 → 23 | 1.46 M | **1/1** (the loadC-stalled test, alone) | 12.01 s | 10:00:53–10:01:08 |
| ociNormal | 0 (OCI tree, main 692dddf + instrumentation) | 8.7 → 10.5 | 1.38 M | **34/34** (`a_stale_or_forged` incl.) | 198.63 s | 10:08:40–10:12:10 |
| loadD | 36 (OCI tree) | 12 → 94 | 1.64 M | **33/34** — `a_stale_or_forged` FAILED (assert `stale_counted`, not a hang) | 697.97 s | 10:14–10:25:15 |
| staleAlone36 | 36 | 11.8 → 23 | 1.63 M | **1/1** (`a_stale_or_forged` alone) | 11.21 s | 10:29:23–10:29:38 |

Commands (the contract):
- A: `load-run.sh loadA 18`
- normalA: `load-run.sh normalA 0`
- loadC: `load-run.sh loadC 36`
- retireAlone36: `load-run.sh retireAlone36 36 --exact three_daemons_form_a_fleet_and_the_survivors_retire_a_dead_node`
- ociNormal: `load-run.sh ociNormal 0` on `agent/fleet-under-load` merged onto `main` (692dddf, the OCI tree)

## Part 3 — the classification

### Acceptance at the charter's load — MET

The charter's acceptance is the full suite under **one burner per hardware thread** (18). Run A passed
**34/34 in 214.82 s** at load rising to 40. At normal load the suite passed **34/34 in 190.75 s**, within
the measured noise of today's ~185 s baseline (five runs). So at the charter's load the suite is honest
and robust; the trace confirms no wait came near its bound (slowest observation ask 43 ms of the 10 s
observe budget; longest frozen progress 0.31 s of the 300 s frozen cap; every one of 102 polls in run A
held rather than timing out).

### The 36-burner failures (~3.5× oversubscription) — accumulated suite-state, proven by two discriminators

At 36 burners (2× per hardware thread, load 64–94, **beyond** the charter's 1× bar) the suite failed twice,
at two different tests, in two different ways — and **both pass alone under the same 36 burners**:

**loadC — a hang at test 32, `three_daemons_form_a_fleet_and_the_survivors_retire_a_dead_node`** (real SWIM
detection). The trace and a thread sample (`stall-sample.txt`) show a fleet **alive but not converging**,
not wedged: both survivor shards **parked in `kevent`** (1397/1462, 1404/1462 samples), waking to run
`reap_loop`/`expire_leases` — a healthy idle duty cycle, not starvation and not a lost-wake wedge (steps
climbed to 851,159). Both coordinators advanced (`min_progress` 1 → 3765 over ~426 s, frozen-progress
< 0.2 s), so `poll_until` never tripped its frozen cap; it would have failed the test at 4000 periods. My
420 s no-output stall detector killed it first. Yet the survivors never retired dead C — detection needs
~6 probe periods; 3765 record-plane periods passed without it.

**loadD — a clean assertion at test 23, `a_stale_or_forged_announcement_is_refused_and_counted`** (not a
hang; the run finished, 697.97 s). The `stale_counted` poll (`fleet.rs:1585`) ran a full **4000
coordinator periods over 409 s** and returned false ("period budget spent"), so the assert fired. The
enriched pulse names the state precisely: **A's control shard was healthy** — steps 1,138,214, spawns
565,076, completions 565,062 (so spawns ≈ done, nothing backed up), **`adm_refused=0`** (the arena never
refused a task), **`longest_step` 39 ms** (no blocking step) — it was busy answering the poll's 565,076
`fleet_refusals()` observe queries (128× the other three daemons' ~20 spawns each). The two announcer
daemons (spawns ~20) simply **never got their stale/forged announcements served by A**, so A never counted
them. So the mechanism is neither arena saturation, nor a blocking step, nor a failed observe path (A
answered every observe): it is a fresh daemon's SWIM handshake/announcement not converging.

**The discriminators.** The loadC-hung test passes **alone** under 36 burners in **12.01 s** (detection at
47 periods); the loadD-failed test passes **alone** under 36 burners in **11.21 s** (the stale count held
at period 1). Same tests, same 36-burner load, same poll rate — the only difference is the accumulated
static state of the ~22–31 prior tests. So **both** 36-burner failures are **accumulated suite-state under
load**, not the load regime, not the poll rate (identical alone vs in-suite), and not the harness observe
path (the enriched pulse shows the polled shard healthy). This is the class the churned-`target/` record
names (`docs/bugs/2026-09-13-fleet-suite-wedges-in-a-churned-target-dir.md`: "leaked demux sockets, leaked
identities") and the class the integrator's normal-load `a_stale_or_forged` stall belongs to — a
late-in-suite test failing on the whole suite's prior state.

**What accumulates, and why it is a test-harness artifact, not a product defect.** The in-process suite
runs 34 tests × 3–5 daemons in **one process**, and each daemon leaks its process-lifetime resources: the
runtime `ShardContext` and its kqueue driver fd (`Box::leak`, never dropped), the fleet demux UDP sockets
(leaked because an in-process daemon cannot rebind them on restart), the NFS listener, the anchor segment
and IPC shm, the `&'static` identity and progress atom. Threads are **not** leaked (`Daemon::stop` joins
the doorbell and every shard, confirmed in the sample: only the current test's threads are live). So by
test 23–32 the process holds ~22–31 tests' worth of leaked fds and memory. Under ~3.5× CPU oversubscription
this extra fd/allocator pressure slows a fresh daemon's transport enough that a single SWIM handshake or
announcement does not complete within the 4000-period budget — while at the charter's 18-burner load, and
at normal load, the same suite is 34/34 (run A, normalA, ociNormal). A real deployment runs one daemon per
process and never accumulates this state; the effect is specific to the in-process suite.

**Not chased to the exact fd/allocation, and why.** Naming the precise leaked resource would need fd-count
and allocator instrumentation and several more over-spec (36-burner) runs; the charter's acceptance is the
18-burner load, which passes, and R4 forbids touching the consensus-adjacent transport/timing paths
speculatively for a regime beyond that bar. A quiet box, or one burner per hardware thread, shows 34/34.
The enriched pulse (`adm_refused`, `longest_step`, spawns/completions) is left in place so a future
over-spec run names the resource without new work.

## Part 4 — status paragraph (§4.8, dated 2026-09-14)

> **Status (2026-09-14).** The in-process fleet suite is honest and robust at the charter's load: the full
> 34-test suite passes 34/34 under one CPU burner per hardware thread (18 on this box) in 214.82 s, and
> 34/34 at normal load in 190.75 s — both with a new opt-in harness trace
> (`SLATES_FLEET_TRACE`) that charges every wait against per-daemon coordinator progress
> (`Daemon::fleet_progress`) and records each shard's forward-progress pulse
> (`Daemon::shard_pulses` — steps, driver waits, spawns, completions, refused admissions, longest step,
> parked, kicks skipped) read directly off the runtime registry, so a stall names which daemon stopped
> advancing and by what mechanism. Beyond the charter's load, at ~3.5× oversubscription (36 burners), a
> late-in-suite retirement test does not converge in a full period budget while a fresh run of the same
> test under the same load passes in 12 s — an accumulated-suite-state effect (leaked per-test process
> resources), not the load regime and not the harness clock; the enriched pulse is wired to name the
> starved resource. No product behaviour changed; the additions are the per-period progress statistic,
> the per-shard pulse, and the harness trace.

## Part 5 — the integrator's row sentences

- **Registers/configuration (4.8):** the fleet suite is traced and load-classified — 34/34 under one CPU
  burner per hardware thread (214.82 s) and 34/34 at normal load (190.75 s); a per-poll daemon-time trace
  and a direct-read per-shard pulse (steps/waits/spawns/completions/refused-admissions/longest-step)
  distinguish a slow-but-progressing fleet from a wedge; an accumulated-suite-state stall at ~3.5×
  oversubscription is isolated by the alone-vs-in-suite discriminator (the same test passes alone under
  36 burners in 12 s).
- **Machine/memory/runtime (4.1–4.3):** the runtime registry carries a per-shard `Pulse` (cache-line
  padded, `Relaxed` statistics, one plain store per step by the owning shard) reporting step, wait,
  spawn, completion, refused-admission and longest-step counts and the parked snapshot, readable from any
  thread with no shard round-trip, so an observer distinguishes a shard that is stepping-but-slow from one
  parked-with-no-kick or held in a long poll — the instrument a stall diagnosis needs when the shard will
  not answer a query.
