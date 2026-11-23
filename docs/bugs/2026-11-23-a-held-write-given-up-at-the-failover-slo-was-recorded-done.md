# A held write given up at the failover SLO was recorded done (2026-11-23, §4.10 "Live shipping", A-354; §4.8 "The three mechanisms")

Found by use: CI's nightly `live_chaos` lane (`crates/cli/tests/live_chaos.rs`), red on both Linux runners every night
from 2026-11-21 (runs 44897075250, 45017186325 and 45176536354). It is a safety violation: StaleNeverCommits and
NoLoss.

## Description

A verb on a live-shipped volume answers once its publication is durable at `f + 1` (§4.8 "The three mechanisms",
item 3; A-200). A-354 bounds that hold at the failover SLO: past it the reply is refused `DurabilityPending`. Its
amendment of 2026-11-20 (commit 20fae8df6, main's ruling on g226) recorded the verb's own reply as the request's
completion at that moment and delivered the refusal unrecorded (`live_ship::answer_given_up`). `slates_client` sends a
`DurabilityPending` request again under its id once, by itself, and that resend read the record. So every caller of the
client was handed `FsDone` for a write that no quorum held when it was answered.

An owner cut off from its peers cannot tell whether its held write will ever be held. It never learns that a successor
took the volume over from the holders' head. Its clients were told the writes succeeded, and the successor never had
them.

## Measured

- CI run 45176536354 (main 205d779de), ubuntu-latest, `an_owner_cut_off_while_running_is_superseded_and_serves_nothing_stale`:
  nine writes sent to the cut-off owner were acknowledged after about 2.2 s each (`Outcome { file: 65, sent:
  225.235019ms, took: 2.617909302s, refused: None }` and eight more). The successor answered each one `NotFound`
  ("NoLoss (successor): 9 violations"). The deafened and muted flows failed the same way on both Linux runners every
  night from 2026-11-21. The run of 2026-11-20 (main 4d9de40c9, before 20fae8df6) passed all three.
- adacachy2 (x86_64, 32 cores, load 37 to 55), the release build of main 05e9bb698, the same flow under CI's command
  (`--exact … --test-threads=1 --nocapture` with `SLATES_TEST_CLI=1 SLATES_FLEET_TRACE=1`), on its first run: 8
  StaleNeverCommits and 9 NoLoss violations (chaos-164 to chaos-172, each acknowledged after 2.205 s). The old owner
  logged nine give-ups. The first reads "a held reply was refused after 2678 ms, the failover SLO (shard 1): target
  164; head Some(164); durable Some(163); … holders ["4132901788209896581@Some(163)", "17559489297021206042@Some(163)"];
  … fenced None". The other eight name targets 166 to 180, with the log still durable at 163. The successor took the
  volume over at 1.98 s from the holders' 163.
- The server test, rewritten to the rule
  (`a_verb_held_past_the_failover_delay_is_in_doubt_not_done_until_durable_and_runs_once`,
  `crates/server/tests/fleet.rs`):
  - On main's `live_ship.rs` (red): with nothing durable, the resend under the same id was answered
    `Ok(FsDone { size: 0 })`, and the client's call `Ok(FsDone { size: 0 })`. The test failed at `fleet.rs:15337`.
  - With the fix (green): the resend was refused `DurabilityPending` (held 2.95 s), and the client's call
    `Err(Refused(DurabilityPending))`. Once the refusals lifted, the second ask under the id was answered `FsDone`.
    Each directory was listed once.

## Root cause

`live_ship::answer_given_up` recorded `entry.reply`, the verb's own reply, as the completion at the give-up. A
completion is the request's outcome (§4.9, RIFL). Recording `FsDone` claimed the durability the hold was still waiting
for. The client's resend met that record (`Seen::Completed`) and returned it.

The protocol's own contract for `DurabilityPending` said "never a false success" and "nothing claims them durable". The
amendment broke both. Its goal was right: a verb must never take effect twice under one id. Before it, the recorded
refusal told consumers to call again under a new id, and the verb ran a second time. Its means was wrong: it recorded
an outcome that was not yet decided, as a success.

## Fix

`crates/server/src/live_ship.rs`:
- `Awaiting::told` marks an entry whose client was told `DurabilityPending`. The entry keeps the request's place
  (`awaiting_by_request` and `destroys_by_request` keep it). It delivers nothing more and is never told again.
- `tell_given_up` replaces `answer_given_up`. It delivers `DurabilityPending { held_ns }` and records it nowhere. The
  reply goes to the client's shard marked recorded, so that shard records nothing either (`verbs::retry_deferred`). It
  returns the told entry, unless the request's place is kept already: a joined retry is told and dropped.
- `judge_held` and `judge_held_destroys` judge each reply past the bound in one of three ways:
  - Decided, when its node yielded the log (`lost`, A-409): it never becomes durable here, so it is recorded
    `DurabilityPending`, as `yielded` records one.
  - Already told: it is kept.
  - Otherwise: it is told and keeps its place.
  
  Each reply is logged with its outcome ("decided" or "in doubt") and counted `fleet.live.held_gave_up` once.
- `answer` records and delivers a decided outcome. The `recorded` override and its discarded `let _ =` are gone.

A told place is resolved by the terminal paths that already existed, each of which records the true outcome:
- `resolve` records the verb's own reply once the log is durable.
- `refuse_awaiting` records `StaleEpoch` at a fence.
- `yielded` records `DurabilityPending`.
- `forget` records `NotFound` at a destroy.
- `resolve_destroys` records `Destroyed` once the tombstone places.

A resend under the id meanwhile joins the wait (`join`). It is told `DurabilityPending` at the next judging, judged by
the request's first hold. The told places count among the shard's held replies, so `awaiting_bound` bounds them and
`refuse_over_bound` refuses `Overloaded` at the bound.

Docs changed in the same commit: §4.10's status, A-354's second amendment, GAPS, `Refusal::DurabilityPending`
(`slates_ipc`), `RetryCause::DurabilityPending` (`slates_api`), and the comments of `slates_client`. The client's
behaviour is unchanged: it still resends once.

## Impact

From 20fae8df6 (2026-11-20 12:27 UTC) to this fix, every write or destroy held past the failover SLO on a
live-shipped volume was reported done to every `slates_client` caller, whether or not it ever became durable.
- Where the owner kept its volume, the write became durable later, so the success came true afterwards.
- Where the owner was superseded (cut off, deafened, muted, stopped for long, or lost after the give-up), the
  acknowledged write was lost. That broke NoLoss, and it broke StaleNeverCommits as the clients see it: a superseded
  owner's acknowledgement.
- A destroy no candidate could hold was answered done though its tombstone never placed, so a takeover could serve the
  destroyed volume again.

## Siblings

- The same give-up for destroys (`judge_held_destroys`) is fixed by the same change. Its symptom was
  `a_live_volumes_unheard_destroy_then_its_owners_loss` and `a_plain_volumes_unheard_destroy_then_its_owners_loss`
  failing with "the unheard destroy exited 0 before the owner's loss" (CI 2026-11-21 on both Linux runners, 2026-11-22
  on arm).
- A write held after its node yielded the volume (A-409) was recorded `FsDone` at the bound too. It is now decided and
  recorded `DurabilityPending`.
- Owed, not fixed (GAPS). A told place lives in the owner shard's memory, as every held reply does, so a restart
  forgets it and a resend afterwards runs the verb again. And `live_ship::join` copies the first waiter at the key's
  position, which may be another request's when two wait on one position.
- Not this bug: the arm runner's failure of `an_owner_partitioned_mid_stream_is_fenced_and_loses_nothing` on
  2026-11-20 (before 20fae8df6). File 128 was sent 0.6 ms after the `SIGSTOP` and acknowledged after the resume.
  NoLoss held and the successor served it: its entry reached the holders before the stop took effect, and the samples
  saw the old owner durable at 128 under epoch 1. The stale-write judge dates a partition from the return of the `kill`
  call, not from the process stopping. That is a test fault, reported and not fixed here.
