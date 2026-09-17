# The fleet's detection windows floored at an assumed 100 ms scheduler quantum; the quantum is now measured at the runtime's waits — and the red tests once blamed on it had other causes

Date: 2026-09-16
Area: `crates/rt/src/shard.rs` (`ShardContext::scheduler_overrun_ns`, the wait-to-step lateness),
`crates/rt/src/registry.rs` (the pulse mirror), `crates/rt/src/futures.rs` (`scheduler_overrun_ns`),
`crates/server/src/fleet.rs` (`scheduler_quantum_ns`, `probe_period_ns`, `ProbeTiming::deadline_ns`),
`crates/server/src/daemon.rs` (`ShardPulse.scheduler_overrun_ns`, `fleet_probe_windows`),
`crates/server/src/state.rs` (`probe_windows`), the fleet trace's `overrun_ms`.
Status (updated 2026-09-17): the measurement and floors are built, measured inert at rest, and now
proven active under Linux CPU pressure. The fixed-floor control also kept its peers alive: preventing
false retirement remains an unproven benefit, not an explanation of the earlier CI failures.

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

## Linux CPU-pressure proof, 2026-09-17

`a_descheduled_observer_uses_its_quantum_and_keeps_its_live_peer` is an opt-in live two-daemon
history. Two finite, joined threads compete for a Linux container's CPU quota for 12 seconds
(four of the existing three-liveness-budget starvation windows). Neither holds a control-shard poll.
The test samples both original member identities and requires both daemons to acknowledge probes
before the pressure threads finish. An observation refusal fails the membership hold.

`Daemon::fleet_probe_windows` exposes four fixed-size, shard-owned counters: acknowledgements,
deadline dilations, interval dilations, and the largest quantum used to lengthen a timer. A dilation
counts only when the actual timer exceeds the identical RTT/backoff or local-health calculation at
the heartbeat floor. Thus an RTT-only increase cannot satisfy the quantum's non-vacuity assertion.
No timer law changed in this proof change. Normal test runs skip this hardware-dependent history
loudly unless `SLATES_TEST_SCHEDULER_PRESSURE=1`.

Hardware: Apple M5 Max, 18 logical CPUs, 128 GiB; Docker Linux aarch64, kernel
`6.12.76-linuxkit`, 18 virtual CPUs, 67,303,636,992 bytes VM RAM. Each test container has a 2 GiB
memory cap, no external network, no capabilities, uid 1000, and 100,000 µs CPU quota per
1,000,000 µs replenishment period. That is 0.1 CPU for two runnable load threads plus the daemons.
Runs were serial, with no concurrent compilation; host load at the end was 7.60 / 7.33 / 6.45.
These are debug-profile functional experiments, not latency benchmarks or a statistical flake-rate
estimate. A supervisor capped each build at 180 seconds and each run at 90 seconds, killing and
removing only its own container on timeout. No timeout fired.

| Run | Test seconds | Complete membership samples under pressure | Largest sampled OS delay | New acknowledgements A/B | New deadline / interval dilations A/B | Result |
|---|---:|---:|---:|---|---|---|
| Preliminary | 16.75 | 2054 | not sampled; largest used floor 899.922 ms | 12 / 13 | 9, 11 / 11, 11 | pass; counters read just after load |
| Final 1 | 16.15 | 1969 | 898.606 ms | 13 / 13 | 12, 10 / 12, 12 | pass |
| Final 2 | 18.75 | 266 | 964.453 ms | 9 / 6 | 3, 6 / 4, 8 | pass |
| Fixed-floor control | 18.06 | 1755 | 926.050 ms | 15 / 9 | 0, 0 / 0, 0 | fails the deadline-dilation assertion |

The final history counts progress only from observations completed before the pressure window ends;
the preliminary history was tightened to exclude a reply after the load ended. Both final runs
passed. The control changes only `scheduler_quantum_ns` to return `HEARTBEAT_NS` in an isolated
Docker build, with its own target cache; no fixed-floor mode is added to the product. Its membership
and progress assertions passed before its dilation assertion failed. **This proves the live floor
is exercised, not that this workload requires it to avoid a false retirement.** Membership holds
are sampled evidence, not a continuous trace of every detector transition.

### Recorded build and run

Source: `96bb82f` plus this proof change. Build from the repository root with this Dockerfile in the
caller's scratch directory; `SLATES_SCRATCH` below denotes that directory. Cached Rust 1.98 image
identity: `620dbcd12449`. The first offline attempt refused a missing locked dependency (`deranged
0.5.8`); the successful build fetched locked dependencies, without installing tools.

```dockerfile
FROM rust:1.98 AS build
WORKDIR /work
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,target=/work/target-quantum-proof,sharing=locked \
    RUSTUP_TOOLCHAIN=1.98.0 CARGO_BUILD_JOBS=2 CARGO_TARGET_DIR=/work/target-quantum-proof \
    cargo test --locked -p slates-server --test fleet --no-run \
    && cp $(find target-quantum-proof/debug/deps -maxdepth 1 -type f -name 'fleet-*' -executable) /quantum-test
FROM rust:1.98
COPY --from=build /quantum-test /quantum-test
USER 1000:1000
ENTRYPOINT ["/quantum-test"]
```

```sh
docker build --progress=plain -f "$SLATES_SCRATCH/quantum.Dockerfile" \
  -t slates-quantum-proof:20260917 .
docker run --rm --name slates-quantum-proof --network=none --memory=2g --cap-drop=ALL \
  --cpu-period=1000000 --cpu-quota=100000 -e SLATES_TEST_SCHEDULER_PRESSURE=1 \
  slates-quantum-proof:20260917 \
  a_descheduled_observer_uses_its_quantum_and_keeps_its_live_peer \
  --exact --nocapture --test-threads=1
```

The final image digest was `6f709b25fe734402dda90de86c6568f0f3711e55ba027e2a9ed60ac62177ce40`.
For the control, insert after `COPY . .`:

```dockerfile
RUN sed -i 's/HEARTBEAT_NS.max(futures::scheduler_overrun_ns())/HEARTBEAT_NS/' crates/server/src/fleet.rs
```

Replace every `target-quantum-proof` with `target-quantum-fixed` and tag/run
`slates-quantum-proof:20260917-fixed`. Separate caches ensure the control compiles its own sources.
The control image digest was `83d1218d2a585cffaa50db102c88ce714e1c59fef0824b357549afd66da6c448`.
Logs are outside the tree under the session scratch directory, named `quantum-pressure-1.log`,
`quantum-pressure-2.log`, `quantum-pressure-3.log`, and `quantum-fixed-pressure.log`.

## What remains unproven

- A live peer retired specifically because its observer was descheduled, prevented by this floor.
  The new pressure history proves active dilation and continued membership/progress; the negative
  control disproves any claim that this particular workload needed the floor to keep those peers.
- `slates_cluster::timing` (election timeout, round budget) still floors at `HEARTBEAT_NS` and derives
  the rest from measured RTT. No evidence from this pressure history calls for changing those laws.

## Checks for the proof change

2026-09-17, serial, Rust 1.98.0, two build jobs: `cargo clippy -p slates-server -p slates-transport
-p slates-cli --all-targets -- -D warnings` passed (3.84 s); `cargo test -p slates-rt --test overrun
-- --test-threads=1` passed all three histories (0.33 s); `cargo test -p slates-server --lib
fleet::tests -- --test-threads=1` passed all seven histories (0.00 s). `cargo xtask check`
passed all structural, literal, unsafe and version gates (1.56 s supervisor time); formatting
and whitespace checks passed. The Linux test compiled and ran in the image above.
