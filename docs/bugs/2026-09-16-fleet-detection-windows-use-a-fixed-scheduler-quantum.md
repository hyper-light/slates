# The fleet's detection windows floored at an assumed 100 ms scheduler quantum; the quantum is now measured at the runtime's waits — and the red tests once blamed on it had other causes

Date: 2026-09-16
Area: `crates/rt/src/shard.rs` (`ShardContext::scheduler_overrun_ns`, the wait-to-step lateness),
`crates/rt/src/registry.rs` (the pulse mirror), `crates/rt/src/futures.rs` (`scheduler_overrun_ns`),
`crates/server/src/fleet.rs` (`scheduler_quantum_ns`, `probe_period_ns`, `ProbeTiming::deadline_ns`),
`crates/server/src/daemon.rs` (`ShardPulse.scheduler_overrun_ns`), the fleet trace's `overrun_ms`.
Status: the measurement and the floors are built and proven **inert at rest by measurement**; the
benefit they exist for — under oversubscription — is **not yet demonstrated** (see "What is owed").

## What the design asks for, and what the code assumed

§4.8 "Derived constants": `SWIM period = max(k × RTT p99, scheduler quantum)`. The 2026-09-13/14 work made
the probe deadline and the election timing RTT-derived, "floored at the heartbeat as the scheduler
quantum" — `HEARTBEAT_NS` (100 ms), the finest cadence a control-shard task is scheduled at on a quiet
host, taken as the quantum by assumption. On a host oversubscribed several-fold an idle shard is left off
its core for far longer than that, and a fixed floor lets a live-but-descheduled peer be aged out inside
the window. That is a design gap, not a measured failure: no history in the suite reproduces it (below).

## The change

- **Measured where descheduling is unambiguous.** A shard records the absolute deadline it last waited
  for — a driver park (`park`) or the idle spin that ended on a timer (`spin_until_work`) — and the first
  `step` after that wait measures how late it runs against it (`note_wait_overrun`). Only an idle shard
  waits: a busy shard never reaches either, and its late timers fire from the step's own expiry without
  passing here, so a busy shard's task latency is never mistaken for descheduling. The value is an
  exponentially-forgetting maximum (`OVERRUN_FORGET_SHIFT` = 3: a spike not renewed decays by an eighth
  at each later wait), so a transient does not pin the windows open. Read on the shard through
  `futures::scheduler_overrun_ns`; mirrored into the registry pulse (`Pulse::scheduler_overrun_ns`,
  `ShardPulse.scheduler_overrun_ns`, the fleet trace's `overrun_ms`) so a stall dump on another thread
  can tell a shard the operating system is not running from one held inside its own work.
- **The floors.** `fleet::scheduler_quantum_ns() = max(HEARTBEAT_NS, measured)` floors the probe period
  (`probe_period_ns`: the Lifeguard dilation or the quantum, whichever is longer) and the probe deadline's
  base and cap (`ProbeTiming::deadline_ns`), and thereby the suspicion window those probes count. The
  liveness-signal cadences — the record plane's period, the probe and record-link idles — keep
  `HEARTBEAT_NS`: a starved node must announce itself no less often, not less.

## Evidence

- `crates/rt/tests/overrun.rs`, three histories on the real driver (0.24 s): a wait stepped `HELD_OFF`
  past its deadline reports at least that and never more than the shard clock shows; a **busy** shard's
  late timer reports zero with no wait entered (`waits == 0`, the timer fired from a step); an idle shard
  over 40 sleeps never reports more than the lateness those sleeps accrued, and the pulse mirrors it.
- **Inert at rest, measured.** The whole in-process fleet suite, run alone with the trace (43 tests,
  285.35 s, load average 5–6 on 18 cores, 2026-09-16 22:59–23:04): over 830 shard samples the measured
  overrun was 0 ms in 823 and 1 ms in 7 — maximum 1 ms against the 100 ms floor — so every window took its
  unchanged heartbeat-derived value throughout. (`longest_step_ms` peaked at 3,000 in the same run: the
  deliberate 3 s hold of `a_starved_but_live_peer_is_not_retired`, which still passes, 8.60 s alone.)
- `cargo clippy --workspace --all-targets -- -D warnings` clean; `cargo xtask check` ok.

Two tests failed in that suite run, neither this change's: `a_peers_re_dial_burst_…` (a stale test
assumption, `docs/bugs/2026-09-16-redial-burst-assumes-a-per-peer-session-limit.md`) and
`a_whole_ram_replacement_joins_as_a_fresh_voter_and_commits_after_another_loss`, which is a
**pre-existing flake on pristine `b2f1ef7`**: run alone, interleaved on the two binaries (23:14–23:18,
load ≈ 4.5–5), this tree went ok / FAILED / ok and pristine HEAD ok / FAILED / ok, every failure the
same shape — at `audit_wait`'s 30 s wall-clock bound the replacement voter still reports the
pre-transition voter set while both survivors report the new one, the observation itself succeeding
(`Some(..)`). The fresh voter not receiving its own committed admission within 30 s is a convergence
question in the learner path, not investigated here; owed.

## What was misdiagnosed, kept on record

This record's first version made four claims that were wrong, and the code was twice "fixed" against
the wrong target:

1. The three fleet tests red on the CI gates lanes were attributed to observer starvation retiring a
   live peer. Two were a merge-inputs identity bug (`04d2ea0`: the record named its inputs by the tree's
   identity while the content plane keyed by the archive's) and one a test asserting a per-peer session
   quota the demultiplexer never enforced (`67fae2d`, the record above). Their coordinators ticked every
   period of their budgets with every peer alive.
2. "At rest they pass in 5–12 s" was never verified on the tree it was written about; on HEAD they failed
   at rest in 496 s.
3. The first cut measured each fleet loop's *sleep* overrun and was blamed for "496 s at rest" — that
   figure was the identity bug, identical on pristine HEAD. (The sleep measurement was still wrong in
   principle: a fired timer runs only after the busy shard serves its other ready tasks, so it conflates
   task latency with descheduling — which is why the measurement lives at the wait.)
4. "Under the reproduced load the timing tests recover" was written before any such run; none was made.

The lesson that cost the most: two "HEAD" runs actually ran the parent's libraries — cargo judges
freshness by mtime and hashes path sources relative to the workspace root, so two extracted trees sharing
one target directory reuse each other's compiled crates when the later tree's files are older than the
earlier fingerprints. One tree per target directory, first build only.

## What is owed

- **The oversubscription proof.** No test starves the *observing* node's shards at the OS level and shows
  the windows dilating to keep a live peer; `a_starved_but_live_peer_is_not_retired` holds the peer's
  control shard busy (the Lifeguard case), which this measurement deliberately does not count. Until a
  by-use history under real descheduling exists, the floors are a design item implemented and inert, not
  a proven remedy.
- `slates_cluster::timing` (election timeout, round budget) floors at `HEARTBEAT_NS` and derives the rest
  from the measured RTT, which starvation inflates; threading the measured quantum in as its floor waits on
  the same proof.
