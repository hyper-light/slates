# The fleet suite is made robust to noisy, heavy CPU load: waits are charged against per-daemon progress, not wall-clock

> **Supersedes the #32 framing.** Task #32 was filed as a "post-takeover reseal stall." That was a
> misdiagnosis: the reseal test is not the flaky one (13+ back-to-back suite runs, zero reseal failures).
> The real flake is a **whole class** — fleet operations that are correct but *slower under CPU load* failing
> the suite's **fixed wall-clock deadlines**. The prior retirement-flake doc
> (`2026-09-12-retirement-tests-gate-on-real-swim-detection-under-load.md`) fixed two members of the class
> with death-injection and generous deadlines, and closed by saying a genuinely robust mechanism "would need
> continuous in-operation load tracking … not justified by the evidence (R4)." Ada's directive —
> **"we MUST be robust to noisy and heavy CPU load"** — overrides that; this change builds the robust
> mechanism.

- **Date:** 2026-09-13
- **Area:** the fleet integration suite (`crates/server/tests/fleet.rs`) and the daemon observation surface it
  polls (`crates/server/src/daemon.rs`, `crates/server/src/fleet.rs`).
- **Severity:** test reliability — a correct, progress-aware system intermittently reported *failing* under
  accumulated / concurrent CPU load. No product defect: the daemon converges correctly; the tests judged it
  against the wrong clock.

## Symptom and reproduction

Running the full 25-test fleet suite **back-to-back after a cold `cargo clean` build** (so the first runs
execute into the compiler's residual CPU load) failed intermittently — **2 of 8 runs** in one reproduction,
each on a *slow* run (~130 s vs ~100 s quiescent):

- `a_root_learner_fetches_...` — at `assert_fleet_forms`: node `a` saw only 4 of its 5 peers meshed at the
  30 s deadline (a *nearly* complete mesh — slow, not stuck).
- `the_root_group_commits_a_region_retirement_...` — at `assert_fleet_forms`: node `b`'s `fleet_members()`
  returned **empty**. The seeded alive set is populated at boot, so empty is impossible for a live fleet
  daemon — it means the 1 s liveness-budget accessor **timed out**: the control shard was too CPU-starved to
  answer within a second.

A control run with a **trivial (incremental) build** — no residual CPU load — was **6/6 green**. So the flake
requires concurrent/preceding heavy CPU load; on a quiescent machine the suite is reliable.

## Root cause

Fleet convergence is **period-driven**: formation, election, retirement, promotion and takeover each complete
in a bounded number of the coordinator's own heartbeat periods (probe/commit rounds). Under CPU load a daemon
runs *fewer periods per wall-second* but still converges in about the same number of periods. The tests,
however, gated each operation behind a **fixed wall-clock deadline** (30–60 s). A load spike stretches the
periods, the wall-clock deadline expires before the (correct) operation completes its normal number of
periods, and the test fails a system that was working. The empty-`fleet_members` case is the same root cause
striking the *observation* rather than the operation: the accessor's fixed 1 s round-trip budget expired on a
starved shard, and the test read that timeout as a negative.

By inspection the daemon **cannot wedge** here (ruling out a product bug at ordinary load): formation is a
perpetual per-peer loop that re-attempts `establish_session` every heartbeat and re-dials on drop; promotion
/ retirement commit through the root/config Raft with **PreVote** (§9.6) and a **randomized, per-attempt**
election timeout, apply unconditionally and idempotently; and `Daemon::stop()` **joins** the shard threads,
so a stopped in-process daemon leaves no spinning threads to starve its peers.

## Approaches measured-and-rejected (each falsified by an instrumented run)

1. **Poll-thread responsiveness** (prior doc): charge only wall-clock where the *poll thread* stayed
   scheduled. Failed — the poll's cheap query stays responsive while the daemon's *background* work is starved.
2. **A load factor sampled once at formation** (prior doc): scale the deadline by formation latency. Failed —
   a transient spike arrives *after* the sample.
3. **A process-wide coordinator-period tick counter** (first attempt here): charge the wait against a global
   count of coordinator periods. Failed the maximal-correctness bar: with several daemons in one process the
   global count keeps advancing even while **one** daemon is starved, so a single starved daemon is *masked*
   by peers that keep ticking — the wait degrades to a fixed backstop for exactly the case that matters.
4. **Charging observed-negative time** (`Some(false)` from `Option`-returning accessors): fail after N seconds
   of *observed* not-met. Failed — a daemon that is merely **slow but progressing** answers "not yet" the
   whole time, so this fails a correct slow operation. Observation is *observability*, not *progress*.

The common lesson: robustness to CPU load requires charging the wait against the **daemon's own continuous
forward progress**, measured **per daemon**.

## Fix

**Per-daemon forward-progress, gated on the slowest observed daemon.**

- `crates/server/src/fleet.rs` — the record-plane coordinator (`run_record_plane`, which loops from boot to
  drive the council) bumps a `&'static AtomicU64` **once per period** (a §4.14 liveness statistic; `Relaxed`,
  R2 permits atomics for statistics). The atomic is leaked once at boot (like the identity — D-8, no `Arc`),
  shared between the coordinator task and the daemon handle.
- `crates/server/src/daemon.rs` — `Daemon::fleet_progress()` reads that atomic **directly** (no shard
  round-trip), so progress is reported even when the shard is too starved to answer a query. A private
  `Daemon::observe()` helper factors the shard-round-trip observation the other accessors share, and waits a
  **generous** `OBSERVE_BUDGET_NS` (= 10 liveness budgets) rather than 1 s, so a merely-slow shard's answer is
  not lost to a false timeout and misread as a real change (the empty-`fleet_members` failure).
- `crates/server/tests/fleet.rs` — `poll_until(daemons, within, condition)` charges the wait against the
  **minimum** `fleet_progress` across the daemons the condition observes. It returns `false` only when:
  - the slowest observed daemon executed a whole `PERIOD_BUDGET` (4000) of *its own* periods with the
    condition never holding — a genuine non-convergence, judged in daemon-time (so CPU load cannot cause it);
    or
  - no observed daemon made **any** forward progress for `FROZEN_CAP` (300 s) — every observed coordinator
    frozen or dead. This is the sole wall-clock bound; it is generous on purpose, because under heavy load a
    live coordinator can be starved of the scheduler for tens of seconds while perfectly alive, so a tight
    window false-declares it dead.

  There is deliberately **no wall-clock deadline on a progressing operation**. Gating on the **minimum** (not
  a sum) keeps one starved daemon holding the wait open rather than being masked. `poll_head_placed` gates on
  **all** the survivors (successor *and* its candidate holders), not just the successor, because a head places
  only when a holder acknowledges it — so if a holder is the starved one, the wait is charged against its
  progress too. All 36 poll sites pass the daemons their condition reads.

Safe in both directions: it passes a correct-but-slow operation under any progressing load, and still **fails
a genuinely stalled fleet** (no progress, or progress-without-convergence past the budget), so it cannot mask
a real regression.

## Test / validation

- **Gates:** `cargo clippy -p slates-server --all-targets` clean; `cargo fmt --check` clean; `cargo xtask
  check` clean (structural 26 crates, literals, unsafe budget).
- **Realistic heavy load — green.** This is a shared box: baseline load-average ~34 from other users on 18
  cores (~2× oversubscription) *before* any test load. Under that realistic heavy load the suite passes: a
  full `cargo clean` → cold build → **6× back-to-back run went 6/6 green** (~97–103 s each) — the exact setup
  that flaked 2/8 on the fixed-deadline design. Quiescent, 25/25.
- **Instrumented confirmation of the mechanisms.** Under deliberate `18 CPU spinners` (stacked on the shared
  box's baseline), an instrumented run confirmed, by logged bound, exactly the failures the fix targets:
  `assert_fleet_forms` tripping the frozen-window with `min=0` (the coordinator task starved of the scheduler
  for tens of seconds — fixed by `FROZEN_CAP`), and `poll_head_placed` tripping the period budget because it
  gated only on the successor while the holder was the bottleneck (fixed by gating on the holders too).

## Honest residual: pathological starvation exceeds root-group consensus, not the test gate

Stacking 18 busy spinners onto the shared box's ~34 baseline drove ~26× slowdown (a single suite run took
**43 minutes**). There, four *promotion/takeover* tests failed with `advanced 4000 periods, condition never
met` — i.e. the coordinator ran a full period budget while re-issuing the promote, yet the root-group
promotion **never committed**. This is **genuine consensus non-convergence, not a test artifact**: a
Raft-family group cannot hold a stable leader long enough to commit when every node is starved to ~1/26th of
a core, so leadership flaps and the commit never lands — physics, not a deadline bug. It is far beyond the
~1.5× load of the original flake, and the period budget *correctly bounds* it (the run fails rather than
hanging). Making consensus converge under ~26× sustained starvation would be a change to the consensus timing
itself — consensus-adjacent code R4 forbids touching speculatively, and arguably impossible — so it is out of
scope for this test-robustness fix and recorded here as a known daemon limit under pathological CPU
starvation.

## Sibling sweep

- The prior retirement-flake fixes (death-injection, generous deadlines) remain correct and unaffected; the
  per-op `within` values now serve as a floor under the `FROZEN_CAP` bound, not the operation deadline.
- The reseal test (`a_takeover_successor_serves_the_dead_owners_content_over_nfs`) — the original #32 subject
  — needed no daemon-side change; its `reseal_places`/`poll_snapshot_placed` waits ride the same
  progress-gated poll.
- `holds_for` (stability windows) is made robust by the generous `observe` budget: its accessor reads return
  the shard's true state under load instead of a timed-out default that reads as a break.
- The only production additions are the per-period progress statistic, its direct-read accessor, and the
  generous observe budget; no product code path changed behaviour.
