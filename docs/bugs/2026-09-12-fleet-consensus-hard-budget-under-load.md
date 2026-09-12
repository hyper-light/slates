# Fleet consensus ran on hard deadlines — the progress-extension mechanism was never wired in, so it thrashed under CPU starvation

- **Date:** 2026-09-12
- **Area:** §4.8 "late work" — `slates_server::fleet` record-plane and configuration consensus
  (`run_record_plane` → `drive_config_council` / `drive_root_group` → replication / election / learner
  fetch → `broadcast`), and the `slates_cluster::CommitBudget` extension mechanism.
- **Severity:** robustness — a fleet under CPU starvation (a noisy shared-tenant neighbour, a hypervisor
  steal, a kernel saturated by another process's syscalls) fails to converge and thrashes, rather than
  degrading gracefully. Surfaced as consensus/takeover fleet tests missing their deadlines under machine
  load (task #31).

## Description

Ada's requirement: slates targets shared-tenant and globally-distributed deployment, so it must stay
correct **and live** when a noisy neighbour steals the CPU. The fleet suite flaked under machine load; the
question was whether that is a test artifact or a real fragility bug. It is a real bug.

Measured facts (this machine, 18 cores, an unrelated set of runaway `yes` processes holding the CPU at
~8% idle / ~90% system time, load average ~25–35):

- A single fleet operation converges fine under that load: `a_council_commits_a_membership_retirement`
  passes 3/3 in isolation at ~5.5 s.
- The full 23-test suite runs ~linearly (~5.4 s/test) — there is **no** cumulative in-process slowdown
  (fds top out ~226 against a 1 048 576 limit; RSS plateaus ~3.8 GB; shard threads are joined and tasks
  cancelled on daemon stop).
- Yet under a load spike the suite fails 1–8 random consensus/takeover tests, and **every** failure is a
  poll-timeout (the operation did not complete in the test's wall-clock deadline), never a wrong value.

Root of the fragility: **every fleet operation ran on a hard deadline.** `CommitBudget::with_extension`
— the §4.8 "late work" progress-extension policy, built precisely so a commit still gathering
acknowledgements as it nears its deadline is granted a bounded extension rather than declared uncertain —
**had zero callers.** `run_membership` built one `probe_budget()` (`CommitBudget::hard(HEARTBEAT_NS)`) and
threaded it through the record-plane coordinator to the configuration council, the root group, record
commits, takeovers and learner fetches. Under starvation a round's replies arrive late but still arrive;
a hard 100 ms deadline declares the round uncertain at the period boundary and re-dispatches it every
period — a false-timeout storm that adds transport and scheduling load exactly when the machine is already
starved, so the round never converges. `collect_acks` / `collect_promises` already contained the
progress-aware `DispatchWait` loop, but with a hard budget (`max_extensions = 0`) its extension was a
no-op — the mechanism was present but switched off everywhere.

## Root cause

The `f > 0` fleet was using the `f = 0` / laptop degenerate budget (`hard`). The extension mechanism the
design built for late work under load was never enabled by any caller.

## Fix

`crates/server/src/fleet.rs`:

- New `consensus_budget()` → `CommitBudget::with_extension(...)`, every parameter derived from the
  protocol's own periods: base deadline = one period (`HEARTBEAT_NS`, so a healthy round is unaffected);
  poll = a tenth of a period (`POLL_PER_PERIOD`); lookahead = 3/4 (hyperscale's measured late-work
  lookahead); extension = one period per grant up to `ELECTION_HEARTBEATS` grants (a progressing round may
  extend up to about the election timeout — the coherent cap, so extension and election do not fight); stall
  window = the SWIM suspicion span (`SUSPICION_PERIODS` periods — no new reply for that long is a stalled
  round, not a slow one). `run_membership` now builds the record plane with `consensus_budget()`; the SWIM
  probe keeps its own hard `probe_budget()` (a single missed probe is absorbed by the suspicion window).
- `broadcast` now polls at the budget's own fine-grained `poll_interval_ns` (a tenth of a period) rather
  than a fraction of the full extended deadline, so a round whose replies have all arrived returns within one
  interval of the last, whatever deadline the extension allows it to wait to.

At `f = 0` (laptop) there are no peers to gather from, so the extender never fires and the behaviour is the
hard budget's (R8, unchanged).

## Test / validation

Under the same machine load: the four representative consensus/takeover tests
(`three_daemons_take_over_a_dead_owners_head`, `a_council_commits_a_membership_retirement`,
`a_provisioned_head_replicates_across_the_fleet`, `a_takeover_successor_serves_...over_nfs`) each pass in
isolation with no speed regression (4–11 s); a full-suite run that previously flaked went green (23/23,
126 s). `cargo test -p slates-cluster` (the `CommitBudget`/`DispatchWait`/`DeadlineExtender` unit tests,
now exercising `with_extension` for the first time) stays green (106 passed); clippy clean.

## Not yet covered (follow-on)

Under a *severe* spike (the machine crawling — a full suite taking 210 s) some tests still miss their
deadlines: the remaining bottleneck is the **probe/handshake path**, still on the hard budget
(`a_root_learner_fetches...`: "node c … sees members []" — c formed zero sessions in 15 s under a saturated
kernel). That path and load-adaptive test deadlines are the next steps (task #31, ongoing).

## Sibling sweep

`with_extension` had no other callers to correct — it had none at all. The record/promotion/content
dispatch paths (`collect_acks`, `collect_promises`, `content.rs`) already used `DispatchWait`, so they gain
full progress-aware extension from the budget change with no further edit.
