# A read scatter's Overloaded was recorded as its completion (2026-11-19, A-563, §4.9 RIFL)

## Description

The protocol says `Overloaded` means the request was not started and may be retried under the same id. The daemon
leaves such a reply unrecorded where a verb answers it (`recorded_unless_deferred`, since 2026-11-17). So a resend
under the id runs the verb, and an async client holds the id for exactly that resend (`slates_client::Client::begin_again`).

A reply that comes back through `deliver` takes a different path. `retry_deferred` writes it, and it recorded every new
deferred reply as the request's completion. A read scatter's refusal comes back that way: a listing's or the grants'
`Overloaded { shard }` for a part that could not be sent (2026-11-05), or for a part its shard did not serve (A-557).
So the refusal became the request's completion, and every resend under the id was answered from that record, the
refusal again, without running the scatter.

Found while fixing A-557 and reported in its record.

## Measurement

`a_resend_of_a_read_scatter_refused_overloaded_runs_it_again` (`crates/server/src/verbs.rs`, a `--lib` test). The
daemon has two shards, and its control shard is fenced. A client seated on the other shard lists the volumes and
asks for its grants; each request is served as its ring hands it over (`serve`). Once both are answered `Overloaded`,
the client sends each again under its own id.

Before the fix the listing's resend was answered at once from its record, `Overloaded` again, and its scatter
never ran: the test failed with "list's resend was answered from a record" in 0.74 s. That run was on the Mac
Studio, macOS arm64, 2026-11-19, with the test's commit bed2d905f and `verbs.rs` taken from it. After the fix,
both resends go out again and are answered `Overloaded { shard: 0 }`, four parts are counted unserved, and the
test passes in 0.74 s. Next to it, `a_read_scatter_whose_part_its_shard_refused_is_refused_naming_that_shard`,
`a_resend_under_the_id_of_an_overloaded_verb_runs_the_verb` and
`a_green_awaiting_its_readoption_answers_overloaded_then_serves_the_engine_handed_to_it` still pass (5 of 5 with
the filters `scatter` and `overloaded`).

## Root cause

`retry_deferred` recorded a deferred reply whenever its id was new, without the `not_started` check that a verb's own
answer goes through.

## Impact

- A listing or a grants read refused `Overloaded`, because a shard's channel was full or its state refused, could
  never be answered under its id. A client resending under the id, as the protocol invites, met the refusal for
  good, even after the shard had recovered, and `DuplicateRequest` once it had acknowledged the id.
- The same held for any deferred reply that says it was not started. `consensus_on_control` answers `Overloaded` when
  a shard's publication call is given up. A bootstrap's resend now runs again: it answers `Acknowledged` once more (a
  bootstrap already authorized is acknowledged) and publishes again. Before, it met the refusal for good. An
  `Unpublished` from a channel verb's refused record (`refusal_of_db`) means nothing of the verb exists, so running it
  again is what the protocol promises.

## Edits

- `crates/server/src/verbs.rs`: in `retry_deferred`, a new reply that is `not_started` goes through
  `unrecorded_not_started`, which counts it and its `Overloaded` in `verb.overloaded_unrecorded`, but never records it.
  Docs of `retry_deferred` and `not_started`.
- The test above. The A-557 test's reading of answers moved into a shared helper, `answers_by_sequence`.
- The design (§4.9 status, amendment A-563), GAPS (the 2026-11-17 entry's sibling, closed) and the A-557 record's
  sibling line.

## Siblings found

- **`consensus_on_control`'s `Overloaded` (not changed).** It is not "not started": it is answered after the control
  shard's operation ran, when the publication to another shard is given up, or when the control call is given up while
  its closure may still run. A-563 makes its resend run the operation again. A bootstrap is idempotent
  (`bootstrap_authorized`). `Recover`'s second run is judged by its own stale checks, which were not read here.
  Reported, not changed.
