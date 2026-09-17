# Observations answered one silence for six facts

Date: 2026-09-17. Contracts: §4.14 (observability), §4.3 (admission), R5 (tests by use), banned item
9 (a lost error). Design status: §4.3 and §4.8, 2026-09-17.

## Description

Every test- and operator-facing observation of a daemon (`Daemon::fleet_members`, `council_leads`,
`fleet_refusals`, the fault injections, the death injections — thirty-three accessors over
`Daemon::observe`) returned `Option<T>`, with `None` standing for all of: the daemon stopping, a shard
it does not have, a submission refused by a full control channel, an admission refused by a full
arena, a shard starved past the observe budget, a state taken at shutdown, a shard fenced by a
consensus failure, and a retention check that discarded a computed answer. The fleet harness mapped
`None` to "the condition does not hold yet" and kept polling — correct in that a silence never
satisfied a predicate, but blind to *why*: a poll on a daemon that had stopped spent its whole period
budget asking a question nobody would answer, and a diagnosis could not tell a starved shard from a
dead one. The 2026-09-16 diagnosis of three CI-red fleet tests first read exactly such a silence as
"observations failing fast" (`docs/bugs/2026-09-16-fleet-detection-windows-use-a-fixed-scheduler-quantum.md`,
corrected there).

Beneath it, the runtime's `Runtime::spawn_on` reported only a **submission**: its `Ok` meant the
request was in the shard's control channel; whether the shard admitted it was answered to nobody.
`Daemon::spawn_admitted` retried a full channel for the whole observe budget and then
`Daemon::observe` waited a *second* whole budget for the reply — two budgets, no stage named. One
accessor (`fleet_holder_head`) bypassed even that, spawning unretried under the liveness budget alone,
so a full control channel under load read as "holds nothing".

## Design (as directed)

1. **Typed outcomes.** `Result<T, ObserveError>` from every accessor (`crates/server/src/observe.rs`),
   the error naming the stage — submission, admission, execution — and the cause: `NoTarget`,
   `NoRuntime`, `ShardGone {shard, stage}`, `Submission {refusal, attempts, waited_ns}` (a refusal
   that cannot clear, with the runtime's own), `Admission {..}` (the same at admission),
   `Terminated {stage, attempts}`, `Deadline {stage, budget_ns, attempts, waited_ns, last_refusal}`,
   `State(StateAccess)` — absent, fenced, borrowed, or `Retention(AnchorError)` when the borrow ran
   and the retention check discarded the answer (`state::try_with_state`, of which `with_state` is
   now the `Option` form).
2. **Admission receipts** in the spawn protocol (`crates/rt`): `SpawnRequest::with_receipt`,
   `Runtime::spawn_on_with_receipt`, `runtime::submit_to_holder`; `Admission::{Admitted(TaskId),
   Refused(RtError), Terminated}`; a request dropped undrained answers `Terminated` by itself; a shard
   shutting down refuses new admissions (`Counters::refused_at_shutdown`). Bounded (one message per
   receipt), owned by the submitter.
3. **One absolute deadline** across the three stages; only `ControlFull` and `TooManyTasks` are
   retried inside it, paced at a tenth of a heartbeat; `ShardGone` and every other refusal end the
   observation at once. The submission is pinned to the shard's registration (`SlotHolder`), so a slot
   reused by a later daemon refuses rather than answering for a stranger. A question admitted but not
   run at the deadline is cancelled; a reply arriving late is discarded and counted
   (`observe.late_reply` on the shard's refusal ledger).
4. **Poll verdicts.** The fleet harness (`tests/common/wait.rs`, `tests/fleet.rs`) takes a `Verdict`
   of each ask — `Holds`, `Observed`, `Unavailable(ObserveError)` (the wait continues, paced),
   `Terminal(ObserveError)` (the wait ends, naming why on stderr and in the trace) — and charges the
   wait per daemon (`ProgressCharge`): each daemon's periods since the wait began, the least of them
   charged. The rule this replaced charged the advance of the least *absolute* period count, which
   under unequal starts spent a stalled daemon's budget on a peer that was behind it and kept ticking.
5. **Diagnostics** ride the error: the stage, the attempt count, the time waited, the last capacity
   refusal; the trace's sample lines carry `unavailable=N last_unavailable=..`.

## Acceptance tests (by use, `crates/server/tests/observe.rs`)

1. A full control channel: retried, then the deadline names the submission stage and `ControlFull`;
   after the hold the same question answers.
2. A receptive channel and a full arena: the receipt refuses the admission; retried, then the deadline
   names the admission stage and `TooManyTasks`; after the fillers' release the question answers.
3. A target stopped while a question is pending: the question ends `Terminated`/`ShardGone` inside its
   budget; a question begun on it and run after a later daemon took its slot is refused `ShardGone`,
   and the later daemon never ran it.
4. A budget that elapses first: named at the admission stage (queued behind a hold) and at the
   execution stage (a question still running), the late replies discarded and counted (1, then 2).
5. An unavailable shard told apart from an observed zero, with the three verdicts.
6. The charge under unequal starts with one daemon stalled: `[100, 5000] → [4100, 5000]` charges 0
   (the old rule charged 4,000), `→ [4100, 5001]` charges 1.

Plus the runtime's own: `crates/rt/tests/admission.rs` (a receipt names the admitted task; a full arena
refuses on the receipt and admits once a task ends; a request drained during shutdown is terminated on
its receipt; a submission pinned to a holder is refused once its slot is reused; a shutdown lands
against a full control channel — the first bug found on the way,
`docs/bugs/2026-09-17-shutdown-send-lost-under-a-full-control-channel.md`) and `crates/rt/tests/burst.rs`
(a burst past one drain batch is drained whole and the shutdown behind it lands — the second, found
when the first observation history hung its daemon's shutdown for 13 minutes:
`docs/bugs/2026-09-17-control-drain-forgets-a-burst-past-one-batch.md`).

## Validation (this box, 18 cores, 2026-09-17, load 5–9)

- `cargo test -p slates-rt`: every target ok — the library's 18 tests (the holder-pinned send among
  them), `admission` 5/5 (0.30 s), `burst` 1/1 (0.30 s; **8 of 32** on the code before the drain fix,
  after its 10 s wait), and the nine existing integration targets. The library tests were then run
  five more times, each bounded at 60 s, for the registry stress test: 18/18 each, no hang (the binary
  had hung 10 min before its sibling tests gave their registrations back).
- `cargo test -p slates-server --test observe`: **6/6 in 13.89 s**. The first run of these histories
  found the drain bug: the full-control-channel history's flood left the shard with an undrained
  channel, its final question was refused at the deadline although the shard was idle, and the
  daemon's drop-time shutdown then retried against the full channel for 13 minutes (sampled: the test
  thread in `Runtime::shutdown → join`, the shard thread in `kevent`). With the fix, the same run
  passed 5/6; the sixth (the full arena) reported `Deadline { stage: Admission, attempts: 25,
  last_refusal: None }` because its last attempt was queued when the budget ran out and only the
  earlier attempts had met `TooManyTasks` — the deadline now names the refusal that held the earlier
  attempts up, and the history passes.
- The in-process fleet suite, every call site migrated: **43/43, finished in 278.65 s** on libtest's
  monotonic clock, with **0 unavailable asks** across the suite under the fleet trace. The process's
  wall time was 1,886 s: `pmset -g log` shows the machine entering sleep six times between 12:02 and
  12:24 while the suite ran (a laptop left alone). Recorded because the first, untraced run of the suite
  had looked slow for the same reason and was first read as load from concurrent builds — wrong; the
  test that run was sampled on (`a_restarted_peer_is_learned_on_contact_under_its_fresh_identity`)
  passes alone in 5.43 s with every poll held. A suite's wall time on this box is not evidence until
  the power log has been read.
- fmt, workspace clippy (`-D warnings`), the server library (89/89), the non-fleet integration targets
  and `cargo xtask check` are recorded in the commit message.
