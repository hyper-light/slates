# Straggler dispatch tasks leak their runtime slots under the perpetual record plane

- **Date:** 2026-09-12
- **Area:** `slates-cluster` fleet dispatch (`crates/cluster/src/lib.rs`, `crates/cluster/src/content.rs`)
- **Class:** banned item 8 — unbounded task growth (§4.8; CLAUDE.md §2)
- **Severity:** a slow resource leak that exhausts a shard's task arena on a long-running fleet node.

## Description

Every fleet dispatch to a remote holder — a record commit (`commit_record`), a phase-one promotion
(`promote_over_holders`), the ledger's committed-prefix promotion (`promote_ledger_over_holders`), and a
content put (`content::dispatch`) — spawns one child task per holder, each sending its reply and recovered
session back over a channel. The four dispatch drivers run **only** under the record-plane coordinator
(`slates_server::fleet::run_record_plane`), which loops forever. Over time the runtime's per-shard task arena
grows without bound: one leaked slot per dispatched holder per period.

## Root cause

`slates_rt::futures::spawn_child` creates a **joinable** task. A joinable task's arena slot is reclaimed only
when the task is joined, is detached, or its parent finishes (a finishing parent cancels and joins its
children — `crates/rt/tests/differential.rs::a_parent_finishing_cancels_and_joins_its_children`). The four
dispatch drivers did none of these on the success path: they spawned the children, collected replies through
an `mpsc` channel (recovering each holder's endpoint), and then **dropped the `TaskId`s** without joining or
detaching. Because their parent — `run_record_plane` — never finishes, the completed children's slots were
never reclaimed. A single-shot caller (the `commit` integration tests) hides the leak: when the caller
finishes it reaps its children, so the slot count returns to zero.

Confirmed by a failing test that stands a caller in for the perpetual coordinator — it runs a commit and then
parks forever (`futures::idle`), so it never reaps its children — and observes the shard's live-task count:
before the fix it was three (the parked caller plus two leaked dispatch slots), where it should be one.

## Impact

On a laptop or a healthy short-lived process the leak is invisible. On a long-running fleet node the shard's
task arena grows by one slot per dispatched holder per record period across commits, promotions, ledger
takeovers and content puts, and eventually refuses new tasks (its bound), degrading the node. No data-plane
correctness issue: the dispatched tasks complete correctly and their sessions are recovered as before.

## Fix (exact edits)

Detach every dispatch task, so each slot is reclaimed when the task **terminates** rather than when the
(perpetual) parent does. Detaching does **not** cancel: the early-quorum and timed-out stragglers still run to
completion and hand their sessions back over the channel the caller drains (`Stragglers`), so the recovered-
session behaviour is unchanged.

- `crates/cluster/src/lib.rs`: `use slates_rt::futures::detach`; after the collection round in `commit_record`,
  `promote_over_holders` and `promote_ledger_over_holders`, `for task in &tasks { let _ = detach(*task); }`
  (the `tasks` vector already held for the spawn-failure path). A completed task is reaped at once; a straggler
  self-reaps on completion.
- `crates/cluster/src/content.rs`: `use slates_rt::futures::detach`; the content dispatch helper does not keep
  a `TaskId` vector, so it detaches **at spawn** (`Ok(task) => { let _ = detach(task); }`), which is
  equivalent — the task self-reaps on termination and still delivers its reply.

The behaviour that caused the bug (a joinable task under a perpetual parent) is removed, not preserved.

## Test

`crates/cluster/tests/commit.rs::a_commits_dispatch_task_slots_are_reaped_under_a_perpetual_parent`: an
`f = 1` commit whose owner then parks forever; the shard's `live_tasks` must be one (only the parked owner),
proving both dispatch task slots were reaped on completion. It fails (`live_tasks == 3`) without the fix.

## Sibling sweep

All four dispatch drivers shared the defect and are all fixed. The council/root Raft fan-out
(`slates_server::fleet::broadcast`) already **joins** each child once terminal, so it never accumulated slots
and is unchanged.
