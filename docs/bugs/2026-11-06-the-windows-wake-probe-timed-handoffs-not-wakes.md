# The Windows wake probe timed handoffs, not wakes (A-223, 2026-11-06)

## Description

On Windows the boot wake probe (`crates/machine/src/wake.rs`) and every shard's online wake estimate
(`crates/rt/src/shard.rs` `note_wake`) measured an on-CPU handoff, not a sleeping thread woken. On jupiter (Windows
x86_64, 32 logical CPUs, 15 shards; main b502cf96, 2026-11-05) the profile's wake mean read 0.56 µs and each shard's
estimate 0.62–0.83 µs. The idle window was then 100 times the estimate (the daemon's idle ratio; A-223's window rule replaced it), so about 58 µs, and owner
shards parked between a single client's creates. Over 9 runs of 2,000 single-client creates the owner parked during
221–546 creates, and 20 or 21 of each run's 21 slowest creates had their owner park during the create. p99 was
5.8–14.2 ms (jupiter at 30–41 % CPU from other work).

## Root cause

- **The probe.** `platform::is_waiting` answered `None` on Windows, so `await_asleep` took the waiter's own
  announcement as asleep. A waker that kicks between the announcement and the waiter's block finds a thread still
  running, and the "wake" is a handoff. On CI's runners the probe read 143–854 ns with `asleep_confirmed: false`
  (`diag-liveness-1`, both runners, every run).
- **The online estimate.** `note_wake` drops a park whose wait never switched the thread out, judged by
  `attribution::voluntary_switches_now()`, which was Linux-only. On Windows a kick that landed after the wait's entry
  stamp but before the thread blocked in `GetQueuedCompletionStatusEx` was learned as a wake of under a microsecond.

## Measured (CI, 2026-11-06)

- **`QueryThreadCycleTime` of another thread is a gate, not a proof.** A blocked thread's count never moved: 0 of
  2,000 read pairs 100 µs apart, on both runners, quiet and busy. A spinning thread's count read the same twice in 2.4–18
  % of back-to-back pairs on windows-latest (each read 6.5 µs) and 0.5–0.7 % on windows-11-arm (3 µs a read).
- **The thread's own switch count is exact per wake.** `ReadThreadProfilingData` after `EnableThreadProfiling` costs
  14–49 ns a read. Every IOCP wait timed after a 200 µs pause switched the thread out (2,000 of 2,000 in each of the
  32 sets, 16 a runner), with wait reason bit 15 (`WrQueue`).
- **The bench at two shards** (`DIAG_SHARDS=2`, `diag-liveness-2`; quiet; three runs each; spinning single client):

  | runner | arm | profile wake | owner parks per 1,000 creates | 1-client p99 | 8-client p99 | CPU, one client paced 1 ms |
  |---|---|---|---|---|---|---|
  | windows-latest | main | 149–255 ns, unconfirmed | 30–88 per shard | 45.5–49.3 µs | 495–503 µs | 0.035–0.058 core |
  | windows-latest | fix | 7.1–7.3 µs, confirmed | 0–1 | 33.3–37.7 µs | 511–519 µs | 0.24–0.25 core |
  | windows-11-arm | main | 603–796 ns, unconfirmed | 2–7 | 32.8–35.0 µs | 474–494 µs | 0.029–0.051 core |
  | windows-11-arm | fix | 8.2–9.1 µs, confirmed | 0–1 | 28.9–31.4 µs | 499–531 µs | 0.21–0.38 core |

  With no client, the daemon used 0–0.005 core in every arm. With two spinners per logical CPU, every arm's tails were
  noise (single-client p99 of 76 µs to 88 ms across all arms).

## Impact

Every Windows derivation from the wake mean was a hundred times too small: the idle window, the step quantum
(`task_step_budget_ns` was 558 on jupiter), the spin before park and the timer tick. A single client's forwarded
creates paid a real wake at the owner whenever its owner parked inside the gap.

## Exact edits

- `crates/machine/src/switches.rs` (new): the calling thread's own context switches on Windows
  (`EnableThreadProfiling` once per thread, `ReadThreadProfilingData`, disabled at thread exit); `None` elsewhere.
- `crates/machine/src/wake.rs`: on Windows the waker holds a query handle to the waiter. It kicks only once the
  waiter has announced its park and two reads of its cycle count agree. A wake is dropped and counted
  (`WakeLatency::woke_running`) when the count moved again before the kick, or when the waiter's own switch count
  did not move across its wait.
- `crates/machine/src/profile.rs`: profile format version 5.
- `crates/rt/src/attribution.rs`: `voluntary_switches_now()` answers on Windows from `slates_machine::switches`, so
  `note_wake` drops a Windows park that never slept, as it does on Linux.
- `Cargo.toml`: the `windows-sys` feature `Win32_System_Performance_HardwareCounterProfiling`.
- `unsafe-budget.toml`: `slates-machine` 53 → 60, each new Windows site named.
- Tests, each red on main on both Windows runners and green with the fix:
  `a_wake_is_a_confirmed_sleepers_wake_on_the_production_placement` (now held on Windows), and
  `a_thread_that_blocked_reads_a_higher_switch_count_where_shards_judge_their_wakes_by_it`.

## Left open

The idle window is now 100 times a real wake: 0.71–0.91 ms at boot on CI. A client paced at 1 ms keeps a shard
spinning (0.2–0.38 core against main's 0.03–0.06). Under two spinners a CPU, each shard's estimate counts the run-queue
wait its own spinning causes (0.2–6.1 ms), and the window follows it. The window's rule is GAPS' open entry.
