# Landing presentations were never consumed, never expired, and never bounded (2026-09-29, AUD-29-07)

## Description

A landing without a grant ends in a presentation (§4.15 step 2). The owner shard holds the plan's
manifest, binding, volume, snapshot, target and principal in `state.landing.awaiting`, so that the human's
grant binds exactly what was presented.

- **Nothing ever removed a presentation.** `present` recorded it under one landing id and advanced the
  counter. The granted landing then allocated a fresh id, and `finish` removed that fresh id — a key never
  inserted. `issue_grant` left the presentation in place. Every plan, with its target string and binding,
  stayed in memory for good, including after its successful landing, and repeated plans grew the map
  without bound.
- **Nothing expired an abandoned plan.** A presentation outlived the client that made it.
- **Nothing bounded the map.** A client presenting in a loop grew it until memory ran out.
- **A resume could not sweep.** Every call took a fresh id, including a resume under the same grant after
  a crash. The hidden siblings the crashed attempt left carry its id, so the resume's sweep, which looks
  for its own id, could not see them.

## Root cause

The presentation and the landing it presented were never linked. The grant knows the manifest and binding
it was issued from, but not the landing id, and the land verb allocated an id per call without asking.

## Impact

- **Memory.** Unbounded retained metadata per shard (ban 8).
- **Records.** Durable `AwaitingGrant` records that never left that state.
- **Resumes.** A resume that left its crashed attempt's siblings on the disk.

## Exact edits

- **`crates/server/src/landing.rs`.**
  - `Awaiting` records its client.
  - The land verb takes a `LandCall` and the caller's client id.
  - A granted landing runs under the id of the presentation its grant was issued from
    (`presentation_for`: the awaiting landing whose manifest and binding equal the grant's). Otherwise it
    takes a fresh id.
  - `finish` consumes that presentation only when the landing is `Done` or `Partial`, and moves its durable
    record out of `AwaitingGrant` (`LandingStateChanged`) instead of recording a second landing. It
    advances the counter only for a fresh id.
  - `present` replaces the same client's presentation of the same volume and target.
  - `abandon_presentations` drops a client's presentations.
  - A landing without a grant is refused `LandingsAwaitingFull` before any host access, id or record when
    the shard is at its bound and the call would not replace one (`presentations_full`).
- **`crates/server/src/config.rs`:** `landings_awaiting_per_shard = clients_per_shard × shards`, derived. A
  presentation lives on its volume's owner shard whichever shard its client sits on, and each seat may have
  one pending there.
- **`crates/server/src/reap.rs`:** retiring a dead client abandons its presentations on every owner shard,
  beside the removal of its attachments.
- **`crates/ipc/src/protocol.rs`:** `Refusal::LandingsAwaitingFull` (appended), and `ShardReport`'s
  `landings_awaiting` and `landings_awaiting_bound`.
- **`crates/server/src/verbs.rs`:** the dispatch passes the client id; the status report fills the counts;
  the refusal is counted `landings_awaiting_full`.

## Evidence

- **Failing tests first.** Run against `c59f3aa` with only the status field added, so the old lifecycle
  could be observed:
  - `crates/server/tests/daemon.rs` `a_granted_landing_consumes_its_presentation` read one presentation
    still awaiting after cycle 0's landing.
  - `crates/client/tests/presentation.rs` `a_killed_clients_presented_landing_is_abandoned_with_it` read the
    killed client's presentation still there after the 5 s bound.

  Both pass now: three cycles each end at zero, and the reaper drops the presentation after the child's
  `SIGKILL`.
- **The bound.** `verbs::tests::a_presentation_past_the_bound_is_refused_and_changes_nothing`, with the
  bound at one:
  - A second volume's presentation is refused `LandingsAwaitingFull`.
  - The status count and the audit log's `landing_planned` records are unchanged.
  - The next presentation takes the very next id, so no id was spent.
  - A re-presentation replaces the first, whose id `issue_grant` then refuses `NotFound`.
- **Test isolation.** The abandonment test has its own binary, because the reaper's counter is
  process-wide: run beside `reap.rs`'s test, the two counted each other's reaped clients.
- **Suites.** server lib 125, daemon 17 (18 on Linux), client presentation 1 and reap 1. Clippy clean on
  macOS, Linux and Windows; xtask check ok.

## Open

- **Presentations are runtime state.** After a restart, a grant for a presentation made before it is
  `NotFound`, and the landing is presented again. Earlier durable `AwaitingGrant` records stay as history,
  tied to the unpruned grant and landing tables (their own gap).
- **One client can take the whole bound.** A client presenting many volumes or targets could use the bound
  for all clients until it lands or goes; per-client shares are owed if that is measured.
