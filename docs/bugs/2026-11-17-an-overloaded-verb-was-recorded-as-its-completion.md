# An `Overloaded` verb was recorded as its completion (2026-11-17, §4.9 "Exactly-once", RIFL)

Found by reading, from the open line GAPS kept under "A resend under its own id was already acknowledged" (the
many-client load test's entry): "an `Overloaded` raised inside a verb is recorded as its completion, so a resend under
its id meets it for good". Goal g232 (100,000 clients against one primary and two replicas).

## Description

The protocol defines `Overloaded` as "the request was not started and may be retried"
(`slates_ipc::protocol::Refusal::Overloaded`), and the client resends such a request under the same id
(`Client::begin_again`). Its contract says the daemon "answers from the verb's completion record once it is durable,
or runs it if it never ran".

A verb run through `run_recorded` had every reply recorded as its completion (`record_completion`), `Overloaded`
included. A resend under the id then met `Seen::Completed` and was answered the recorded `Overloaded`, never the verb.
The client resent until its reconnect budget ran out and then gave the request up.

A verb raises `Overloaded` itself in two ways:
- **A held copy not yet materialized.** `verbs::absent` answers `Overloaded` for a volume this node holds a copy of
  and has not materialized, the takeover's settle window
  (`docs/bugs/2026-10-11-taken-over-scratch-volumes-never-become-servable.md`). Every verb that finds its volume
  through `find` meets it.
- **A deferred verb whose task the full arena refused.** `image::defer` (a work's checkout, a pin, a green over a
  base, a presentation), the granted landing, the direct map's fill, the FUSE and WinFsp mounts and the Shards attach
  each answer `Overloaded` when their task cannot be spawned.

At 100,000 clients the second is reachable on any busy shard: a work's checkout refused so was refused for good.

## Measured

- The failing test, `a_resend_under_the_id_of_an_overloaded_verb_runs_the_verb` (`crates/server/src/verbs.rs`, the
  unit tests): a snapshot of a volume this node holds a copy of and has not materialized is answered `Overloaded`.
  The copy is then dropped, and the same request is sent again under its id through the owner's completion window
  (`run_forwarded`).
  - Before the fix, the resend was answered `Overloaded` again, from the recorded completion.
  - After the fix, it ran the verb and was answered `NotFound`. `verb.overloaded_unrecorded` counted 1.

## Root cause

`run_recorded` recorded whatever reply the verb gave, unless the verb deferred its reply. Nothing told a reply that
completes the request from one that says it was never started. The forward paths already answered their own
`Overloaded` (a full control channel, a task that could not be spawned) without recording it, so the rule was the
codebase's convention everywhere except inside a verb.

## Fix

`run_recorded` (`crates/server/src/verbs.rs`) leaves an `Overloaded` reply unrecorded (`not_started`,
`unrecorded_not_started`). It counts the refusal by its name and `verb.overloaded_unrecorded`, and counts the request
served. The verb's transaction commits what the verb applied before refusing, and a resend meets each as its own:
- nothing, for every verb that refuses before any effect (`absent`, `image::defer`, the granted landing);
- the verb's own undoing of what it began: the Shards attach records its attachment and, when its stream's task cannot
  be spawned, ends it as a recorded operation (`shards::ended`) before refusing;
- the write lease a FUSE or WinFsp mount takes before its task, which the resend's attach renews as its holder's own
  (`take_write_lease`).

## Impact

Every request a verb answered `Overloaded` was lost to its client. Under load these are a work's checkout, a pin and a
green's creation refused by a full task arena. During a takeover's settle window they are every verb on a taken-over
volume. The client resent each one under its id until its reconnect budget passed, then gave it up, so the load
harness's oracle saw it as unknown.

## Siblings (swept, open in GAPS)

- **A live-shipped verb answered `Overloaded` after its effects committed.** A verb on a live-shipped volume commits
  its effects with no completion (A-200), and `live_ship::defer` then refuses `Overloaded` once the held replies reach
  their bound. The resend under the id meets `Seen::New` and runs the verb again over its own effects. The bound must be
  judged before the verb runs, or the reply must be held, not refused.
- **A deferred task that ends on `Overloaded` records it.** A granted landing whose target lease take went unanswered
  (`landing::acquire_target_lease`) ends through `finish_unrun`, which records the `Overloaded` as the landing's
  completion. Its resend meets it for good, as the synchronous verbs' did. Other deferred recorders
  (`fuse::complete`, `winfsp`, `direct_map`, `lease_wait`, `image`) record whatever their task ends on, so each task's
  final `Overloaded` wants the same review.
- **A mount refused `Overloaded` keeps its write lease until its term if never resent.** The FUSE and WinFsp mounts
  take the volume's write lease before spawning their task and do not release it when the spawn is refused. A resend
  renews it; a client that gives the request up leaves it held until the lease's term ends.
