# A holder's complete page that left a copy out read as holding nothing

**Found.** Review of A-251's takeover rule (raft-chaos, 2026-11-08), after raft's
`2026-11-07-a-holder-restarted-warm-lost-its-copies-routing.md`, whose holder "answered the successor's page empty and
complete, so the round counted its promise while its copy was never fenced". No run has shown a lost record.

**Root cause.** A successor counted a holder's complete paged answer as a promise of every object it did not list.
`takeover::list_page` lists a copy only when the holder's own routing and lineage (`object_owner`, `departed_owners`) say
it falls to the asker, and that routing is not persisted. So a holder's silence about an object said nothing about what
it held. A successor could reach an object's recovery quorum on such an empty promise and adopt an older record over a
committed newer one. Step (c) closed the same hole for a copy the holder could not promise
(`2026-11-08-a-holder-that-could-not-promise-a-copy-read-as-holding-nothing.md`).

**Impact.** A takeover could lose a committed record (NoLoss). `crates/db/tests/takeover_model.rs` shows the rule doing so:
over 2,000 generated histories, 216 committed records lost with holders leaving copies out of complete pages, none
without.

**The same hole in the successor's own copy.** It learns its own copies through its routing (`promise_local_copies`), and
counted its open lease gate as its promise. A successor whose routing lost its own newer copy could then adopt a member's
older record: with the nemesis leaving out the successor's own copies too, that rule lost 86 committed records over the
same histories. Found while designing step (d)'s process test.

**Edits (A-251 (b)).**
- `crates/db/src/takeover.rs`: `CountingRule::NamedAnswer`, and `answer_by_name`, the holder's answer from its own records.
- `crates/db/src/register.rs`: `HostPrepare::named` and `HostPromise::nothing`, appended to the wire.
- `crates/server/src/takeover.rs`: the successor asks each member whose pages are complete about what it did not list
  (`begin_round`, `unanswered_by_name`), folds the answers (`fold_answers`), and counts under the named rule
  (`ready_objects`). A holder answers through `answer_named`. The successor answers itself by name, from its own records,
  while its lease gate is open (`answer_own_copies_by_name`); the named rule no longer counts its gate alone.

**Tests.**
- `a_complete_page_promises_nothing_of_an_object_it_did_not_list` (a real shard): after a complete, empty page the object
  is not ready and the next round asks about it by name; after the answer it is ready.
- `a_holder_answers_by_name_for_a_copy_its_page_leaves_out` (a real shard, the copy's lineage forgotten): the page lists
  nothing and is complete; the answer by name reports the record.
- `a_successor_whose_routing_lost_its_own_copy_still_adopts_it` (a real shard): the successor's own newer copy, its
  lineage forgotten, is adopted over a member's older record. Red with the successor not answering itself.
- `a_holder_answers_each_named_object_from_its_own_records` and `a_named_answer_keeps_to_its_budget_and_reports_a_higher_fence`
  in `crates/db/src/takeover.rs`.
- `a_named_question_and_its_answer_round_trip_and_a_page_keeps_its_bytes` and `a_hostile_named_list_is_refused`.
- The model, `crates/db/tests/takeover_model.rs`.

**Not closed here.** An object that every holder leaves out of its pages, and the successor does not hold, is never learned,
so nothing asks about it by name, and a round can confirm without it (A-251, GAPS). The process test is step (d).

**R3.** A question names the ids that fit one page after the request's fields (`named_per_question`, from `page_budget`). An
answer is bounded by `page_budget`.
