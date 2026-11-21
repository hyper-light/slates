# main <- w-fed-move: the drop-site counters are on main (A-800, Slates 9918e6c0; commit 9249a6a7), 2026-11-21

The lost-reply counters are on main, so your 100,000-client rerun (run-requests.md, mem=40g) can now name the site.

## The hop and its drop sites (control shard's answer -> client shard's waiter)
1. `deliver_owner_forward` -> `xshard::send_reply_home`: counted `xshard.reply_home_handed` on the sending shard.
2. `send_back` -> `carry`: sent at once, or kept for room (`xshard.room_waits_started/ended`), or refused
   (`xshard.reply_dropped`, `xshard.keep_refused`).
3. The client shard's admission. Admitted: the task runs, `xshard.reply_home_ran`. Refused at a full arena or a
   shutdown: `RunUnstarted`'s drop runs it, `xshard.reply_run_unstarted`.
   - NEW: dropped on its own shard with the state borrowed. It was silently lost; now it is resent home
     (`xshard.reply_resent_borrowed`).
   - NEW: dropped where no shard can run it: `xshard.reply_orphaned` (a legitimate loss, counted apart).
4. `finish_owner_forward`: `fleet.forward.client.delivered_home`.
   - NEW: borrowed state. It was silently lost; now it is resent (`fleet.forward.client.resent_borrowed`).
   - NEW: a retention refusal after the closure ran. The reply was dropped; now it is delivered
     (`fleet.forward.client.delivered_past_retention`).
   - NEW: no state or a fenced shard: `fleet.forward.client.unservable`.
5. `state::deliver`: client gone or slot reused, `state.reply_undeliverable` (legitimate).
   - NEW: borrowed state, `state.reply_undelivered_borrowed`.

## Red tests (slates-server --lib)
- `a_reply_dropped_unrun_while_its_shards_state_is_borrowed_is_resent_not_lost`
- `a_forwarded_reply_that_meets_a_borrowed_state_is_sent_home_again`
- `a_reply_carried_home_is_counted_handed_where_sent_and_run_where_it_ran`

## What to read in the rerun
The load harness prints all of these from the client node: `lost` rows for the drop sites, `counter` rows for the hop.
- handed > ran + reply_run_unstarted: replies admitted and never run (a lost wake, not a drop).
- resolved > handed: lost on the control shard before the hand-off.
- Any orphaned, unservable or undelivered count names its site.

Leading candidate: a reply admitted and never run. That fits the stalls (every daemon idle, the replies never run)
and their start between e4c2ad76 and 8a18edcc, where 343b741e landed. A-798 (931e52ed, another worker's) has since
fixed a lost kick on a parked shard of exactly that kind. If the rerun shows handed equal to ran and no stuck
`pending` rows, the lost replies are closed by A-798 plus this change.

## Added 2026-11-21 ~01:40 PST: the owner's cost by kind (A-801, Slates 2d079c44)
If r48 builds main at or after 2d079c44, the harness also prints `server_counter <instance> <counter> <count>` rows
for the primary and both replicas: asks of the owner by kind (`merge.work.asked_owner.<kind>`) and the owner's
answering time by kind (`merge.work.answered.<kind>` and `..._ns`).
- On the primary, the largest `answered.*_ns` row names the 100k throughput wall's mechanism.
- At 1,000 clients in-process the owner spent only 185 ms answering in all, so it is not the wall at that scale.
Please send me those rows with the drop-site rows.
