# A refused hold release left a drive held, with nobody serving it

**Amendment:** A-206 (corrects A-198).

**Found by:** the audit of 2dc9f3eb..98000d56 (2026-11-03), step 4 of plan 01a1a34bf7e973069e94f773afdea44a.

## Description

A WinFsp mount's three handles are held by the anchor across a daemon's death (A-198). The mount's end asked the anchor
to release them, and then deleted the file system whatever the anchor answered. A refused or unanswered release was
counted and printed. The anchor's copies of the volume's control handle and the drive's link then kept the volume and
the drive letter with no daemon serving them. A `detach` also answered success before the release had been asked for,
since it ended the record and left the rest to the mount's task.

## Root cause

Three defects, each making the next one reachable.

1. **The deadline was not a deadline.** `exchange` computed `clock.monotonic_ns() + RECOVERY_BUDGET_NS` and gave it to
   `xshard::task_within` and `futures::within`, which take a span from now. So the wait lasted the daemon's uptime plus a
   second, and grew as the daemon aged.
2. **Passing it closed the only channel.** Cancellation dropped the exchange's lease, and the lease owned the daemon's
   one pipe to the anchor, on purpose, so a late acknowledgement could not answer a later exchange. After that, every
   hold and release in that daemon's life was refused `Unavailable`. Each later mount was refused. Each later unmount
   deleted its file system under a hold that could no longer be released.
3. **The release's answer was not consulted.** `reap` deleted the file system after a refusal, and `detach` had already
   answered.

## Impact

One anchor stall longer than the daemon's uptime (a suspended laptop's anchor, a paused process, a debugger) ended
WinFsp mounting and clean unmounting until the daemon was next restarted, and left every unmounted drive defined,
hanging its callers, until then.

## The fix

- `anchor_hold_windows.rs`: the budget is a span. The lease returns the pipe however the exchange ends, and a read passes
  over answers that are not its own (each is matched by message kind and id), so a late answer cannot answer another
  exchange. A waiter for a busy pipe sleeps a tick instead of spinning. `HoldRefusal::NoChannel` names a daemon with no
  anchor, which holds nothing.
- `winfsp.rs`: `defer_detach` asks the anchor while the mount serves on. An answer ends the attachment and the mount in
  one recorded step. Past `RELEASE_BOUND_NS` (recovery budget plus liveness budget, 2 s) the detach answers
  `TargetUnavailable`. The drive stays served and held, and the request stands, because it may be in the pipe for the
  anchor's next turn: it goes on repeating, and the anchor's answer ends the mount then.
- `reap` waits for the same answer before it stops the mount thread. That covers a destroy, a revocation, an adoption
  refused and a mount refused at its hold. A refused hold keeps the objects its message names until then, and the
  reply is sent first.
- `slates_client::defers_reply` includes `Detach`: the daemon always answers, and a release may take its bound.

The wait has no bound in time, since an anchor that never answers leaves nothing safe to delete. It holds one task and
one message per mount (a pipe write refused by a full quota stays with the caller) and ends at the answer or the shard's
stop. An anchor that dies takes the daemon with it through its job object.

## Evidence

See the entry at the top of `docs/wip/GAPS.md`, which carries the run ids. The tests are:

- `a_missed_deadline_keeps_the_hold_channel_and_a_late_answer_answers_nothing`
  (`crates/server/src/anchor_hold_windows.rs`), native on any Windows host: a real pipe and shard against a stand-in
  anchor. Red-checked on jupiter 2026-11-04 by making the lease drop the pipe: the test fails at its second exchange.
- `a_release_the_anchor_does_not_answer_leaves_no_drive_held` and `a_refused_mount_time_hold_leaves_nothing_mounted`
  (`crates/cli/tests/winfsp.rs`), live on CI's Windows lanes: the test stops the real anchor's process with
  `NtSuspendProcess`, so the anchor's not answering is the real thing.

## Siblings found, not changed here

- The Windows anchor parks up to half the liveness budget between looks at the hold channel
  (`await_observation`'s `not(unix)` arm in `crates/cli/src/anchor.rs`), where the Unix one wakes on the channel. So each
  mount's hold and each unmount's release can wait up to 500 ms on a healthy anchor. The derived budgets cover it, but
  it is latency a channel wake would remove, and it belongs with the anchor's Windows hold (step3-anchor).
- `Hold::handoff` drops an unread message with the old channel at a restart. That is right for a hold, whose record never
  committed. For a release it is also right: the attachment's record stands, so the successor adopts the mount.
