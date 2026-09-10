# Timer-wheel `cancel` orphans a reused slot's live timer

Date: 2026-09-10
Area: `crates/rt/src/timer.rs` (the hierarchical timing wheel)
Severity: runtime correctness — a live timer silently never fires; a task waiting on it hangs forever.

## Description

Under a three-node fleet's normal load, a survivor's SWIM probe of a dead node would hang past its
deadline: the probe's `sleep(deadline)` never fired, so the detector never aged the dead node to death,
so the survivor failed to retire it. The `three_daemons_..._retire...` test failed **0/12**; the two-node
retirement test flaked ~1/3. It was *not* a "noisy machine" artifact — a three-node fleet's own task and
timer churn triggered it every time, while the lighter two-node load usually escaped it.

## Root cause

`Wheel::cancel` disarmed a timer by unlinking it from its slot list **before** validating the id's
generation:

```rust
pub fn cancel(&mut self, id: TimerId) -> Result<(), RtError> {
    self.unlink(id.index())?;          // (1) unlink by BARE INDEX, at the slot's CURRENT generation
    let removed = self.entries.remove(id)?;  // (2) THEN validate the id's generation (stale → Err)
    ...
}
```

`unlink(index)` reads the entry at `index` using `self.generation_of(index)` — the slot's *current*
generation — not the generation carried by `id`. When `id` was **stale** — its slot had already fired and
been reused by a **later** timer — step (1) unlinked the **reused, live** timer from its slot list, then
step (2) failed the generation check and returned `Err`. The damage was already done: the live timer was
now **orphaned** — still present in the arena (still `armed`), but in no `heads[]` linked list — so
`expire_tick`, which walks the head lists, never fired it.

The trigger is routine: the SWIM probe (`crates/cluster/src/swim.rs` `probe_once`) spawns a deadline child
that `sleep`s, and cancels it the instant a healthy probe is acknowledged (`Sleep::drop` →
`disarm_timer` → `Wheel::cancel`). Fired-timer slots are reused constantly under a fleet's load, so a
cancel frequently arrived stale and spliced out whichever timer had reused the slot — including a
*different* probe's deadline timer, stranding that probe.

## Diagnosis method (reusable)

Instrumenting `Wheel::advance` to scan, after advancing to the target tick, for any armed entry whose
`deadline <= now_tick` (a due timer the wheel failed to fire) immediately surfaced the stuck entry, its
`level`/`slot`, and that its head *was* being processed by `expire_tick` yet the timer stayed there — the
signature of an entry orphaned from its list. `SLEEP-ARM-FAIL` and the run-queue's `refused` counter were
both zero, ruling out slab exhaustion and lost run-queue wakeups.

## Impact

Any two callers sharing a timer slot across a fire+reuse boundary where the earlier id is cancelled late.
The fleet's SWIM probe is the exercised case; the register/promotion dispatch and any `sleep`-with-timeout
select are equally exposed. The failure is a silent hang of the waiting task, bounded only by the caller's
own outer timeout (which, here, was the very timer that got orphaned).

## Fix

Validate the generation **before** touching any list: remove from the arena first (which refuses a stale
id), then unlink using the removed entry's own recorded position (`next`/`prev`/`level`/`slot`) — it is
gone from the arena, so the unlink needs no second read of it.

```rust
pub fn cancel(&mut self, id: TimerId) -> Result<(), RtError> {
    let removed = self.entries.remove(id)?;   // validates generation FIRST; stale id → Err, no mutation
    let head = usize::from(removed.level) * SLOTS_PER_LEVEL + usize::from(removed.slot);
    self.unlink_at(removed.next, removed.prev, head);
    self.armed -= 1;
    if self.earliest == Some(removed.deadline) { self.earliest_exact = false; }
    Ok(())
}
```

`unlink(index)` was replaced by `unlink_at(next, prev, head)`, which mends only the former neighbours and
the head pointer.

## Regression test

`crates/rt/src/timer.rs` `a_stale_cancel_does_not_orphan_the_timer_that_reused_the_slot`: A fires and frees
its slot, B reuses the slot, the now-stale `cancel(a)` is refused, and B must still fire. Fails before the
fix (B orphaned, never fires), passes after.

## Sibling scan

`unlink` was called only by `cancel`; no other path unlinked by bare index. `link` (insert and cascade)
and `expire_tick`'s removal already operate on entries known-current, so they were not exposed.
