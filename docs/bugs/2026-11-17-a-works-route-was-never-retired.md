# A work's route was never retired after its owner's takeover

Found 2026-11-17 by reading, in the sibling sweep after A-255 landed (goal 241); flagged for Ada, and option A was
chosen by the supervisor the same day. Fixed as A-288.

## Description

A-255 made a work's catalog record, which names its green, the route a resend of the work's submit takes after the
work's owner is taken over: the record leads the resend to the green's owner, which holds the submit's completion. The
survivor that adopted the record keeps it placed (`fleet::owed_adopted_work_catalogs`), and every candidate holds a copy.

Nothing ever retired it. Every work alive at an owner's death was held, by its adopter and every holder, for as long as
the fleet ran: a store with no bound (CLAUDE.md §2 item 8).

## Root cause

- Only a volume's owner writes its tombstone (AUD-29-43), and a work never comes back to have one: its head is never
  placed, so no successor takes the work itself over.
- The adopter kept the record in `pending_catalogs`, which drops an entry only at a materialization or a tombstone,
  neither of which comes for a work.
- The holders kept it in `holder_records` until a tombstone's retirement stage, which never came.

## Impact

- Memory and record-plane work grew with every work alive at every owner death, on the adopter and on each holder. The
  adopter rescanned each record every record period.
- A sibling with the same cause: a successor kept its takeover's acceptors and its placement of an adopted catalog after
  the object's tombstone retired, so a destroyed taken-over volume also stayed held on its successor. The routing kept
  every shard's entry but the control shard's.

## Failing test first

`a_works_route_ends_once_the_greens_owner_holds_none_of_its_completions` (`crates/server/tests/fleet.rs`):

- On main 7c6faee27, after b's client resent, acknowledged and wrote to the green, both survivors still held the
  work's catalog record when the period budget ran out:
  `retired false, counted 0; held at the end [Ok(true), Ok(true)]`.
- Runner adas-macbook-pro.local, load average 109, debug build, 429 s, 2026-11-17. The log is kept at
  `~/.cache/slates-scratch/g241-submit-resent-after/red-2026-11-17-main-7c6faee27.log`.
- The test then gained the work a's own client submitted from. That form is red on main dbf53afa2 too, on the same
  runner: `its route retired false; ... held by both survivors true; ... retired false, counted 0; held at the end
  [Ok(true), Ok(true)]`, 860 s at load 57 to 91, failing at the own client's work.
- With the fix, on the same runner (macOS arm64, debug build, 2026-11-17): `its route retired true; ... held by both
  survivors true; ... retired true, counted 2; held at the end [Ok(false), Ok(false)]`, in 6.91 s at load 11 to 37 on
  7c6faee27, and in 8.56 s at load 108 merged with main b7c294959.

## Exact edits (A-288)

1. **The route's rule.** A route is kept while the green's owner retains a completion that a resend can reach only
   through it. RIFL keeps a completion until its client acknowledges it and moves it with its object (Lee et al., SOSP
   2015, §3).
   - Only a client on another node than the work's owner reaches its completion through the route. A client of the
     owner's own node resends to that node, whose partition still holds the work and the record it made. If the node
     is gone, so is the client.
   - Counting every completion would never end the commonest route: a dead owner's local clients never acknowledge.
2. **The work in the records.**
   - `slates_merge::chain_entry` version 3: a completion names its submitting work when the client was on another node.
   - The merge record's format 2 (`SubmitCompletion::work`), set by `SubmitCompletion::of_current` for a forwarded
     submit only.
   - Anchor `LAYOUT_VERSION` 8.
3. **The green's owner counts what each route leads to, durably.**
   - `CompletionRecord::work` (appended): `record_taken_over_completions` records each adopted completion with its
     work.
   - The partition keeps the work beside the window while the window retains the completion (`routed_completions`,
     `routed_by_work`), dropped by the acknowledgement that releases it.
   - Snapshots carry the work in the record.
   - The query is `Partition::completions_routed_by`.
4. **The question.** `crate::work_route`:
   - Each record period the adopter asks the green's owner, for the works whose routes it keeps, which still have one
     (fleet stream 25, at most 71 works a question).
   - The owner is this node, a cached route, the live creator, or one location round.
   - Only the shard that serves the green answers.
   - The keeper is its own task, parked until a takeover adopts a work's route (`takeover::record_adoption` nudges it).
5. **The retirement.** A work answered "none" moves from `pending_catalogs` to `pending_tombstones` at sequence 1,
   where its own destroy would have closed its registers. The existing stages ship it, and holders drop both registers
   at the retirement stage.
6. **The sibling.** `fleet::retire_shard_tombstones` drops the owner's own copies of a retired object's registers: its
   `holder_records`, the control shard's `placed_heads`, and the routing on every shard
   (`takeover::forget_owner_on_every_shard`).

## Still open (A-288, GAPS)

- A route to a green destroyed, or lost to a second takeover it cannot survive (GAP-A9-7), while the route is kept: no
  owner answers.
- ~~A route to a green its successor gave up on~~ (main's give-up after the failover SLO's attempts, d3f4ddbe2): ended
  the same day by A-288's second piece. The green's shard answers for a green it holds `unrecovered`, from its
  partition's count, since its successor never recorded the adopted completions and never will. A given-up green that
  is then destroyed is neither served nor `unrecovered`, and joins the item above.
- A route whose completion its client never acknowledges keeps it, as the completion record itself is kept. This is
  RIFL's client lease, not built.
- A route its adopter kept across a warm restart (the last sibling below).

## Sibling inconsistencies seen, not changed

- `verbs::materialize_taken_over_green` used `debug_assert!` in production code, which CLAUDE.md §2 item 6 bans. Fixed
  on main the same day by 9b6b35fb3 (a green of another role is refused `AlreadyExists`).
- A restarted owner re-enqueues a recovered version's merge record with no work for the A-255 gate
  (`enqueue_recovered_record` passes `None`). Such a record can ship before its work's catalog is committed, if the
  owner restarted between the two. The chain entry now carries the work for a forwarded submit, so the gate could take
  it there.
- A green's successor keeps adopted completions of clients that will never acknowledge (the dead owner's own clients),
  bounded per client by `completion_bound`: the completion window's lifetime has no client lease.
- An adopter that restarts warm gets its holder copies back (`content_holder::restore_held_registers`) but not the
  routes it kept: `pending_catalogs` starts empty at every start and only a takeover's adoption fills it. So a
  restarted adopter neither re-places a route nor asks about it, and the route stays held by every holder. This is
  A-255's own gap, which A-288 inherits; found by reading, no test yet.
