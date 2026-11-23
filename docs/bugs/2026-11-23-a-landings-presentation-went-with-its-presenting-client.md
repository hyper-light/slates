# A landing's presentation went with its presenting client, so R10's three-step grant flow could not run (2026-11-23)

## Description
CI's conformance lane failed its hermeticity suite on Linux and macOS (run 45163517212 at 4c8b391ec and every run since
at least 6f6e87d82): `slates grant exited 1: {"error":{"kind":"refused","message":"not found: presentation 1
(replaced, landed, or its client went away: present it again)"}}`. The suite presents a landing with `slates land`,
grants it with `slates grant`, and lands it with `slates land --grant`, each a process of its own, as R10's documented
flow runs. The CLI's own test of that flow failed the same way on this MacBook:
`SLATES_TEST_CLI=1 cargo test -p slates-cli --test cli a_session_grant_auto_approves_the_next_landing_and_stops_at_its_target`
answered "the one-shot grant is accepted: slates: refused: not found: presentation 1".

## Root cause
Two rules met.
- A-46 (AUD-29-07, c324c97a0, 2026-09-29): a client's retirement abandoned the landings it had presented and not landed
  (`landing::abandon_presentations`, called on every owner shard by `reap::remove_on_owners`), so "a presentation does
  not outlive the client waiting on it".
- 076746072 (2026-11-18): a dead client is reclaimed at its hang-up: the end of its control socket's stream on Linux,
  its process's exit on macOS and Windows.

`slates land` presents and exits. Since 076746072 its client was retired at that exit, its presentation abandoned with
it, and `slates grant`, the next process, found nothing to grant. Before 076746072 a dead client lingered until a lapse
of its liveness, long enough for the grant: that hid the conflict between A-46's rule and R10's own flow.

## Impact
No landing could be granted through the CLI's documented flow (present, a person's grant, land under it) on any
platform: every `slates grant` after a `slates land` was refused `NotFound`. The hermeticity suite, R1's tracer proof
of zero writes outside a grant, could not reach its granted landing. Nothing was written wrongly: the refusal wrote
nothing. A landing presented and granted from one long-lived client (an embedding host through `slates-api`) was not
affected.

## Edits
- `crates/server/src/reap.rs`: a client's retirement no longer abandons its presentations.
- `crates/server/src/landing.rs`: `abandon_presentations` removed; `forget_presentations_of(state, volume)` drops a
  volume's presentations; `Awaiting`'s docs name the lifetime (A-874).
- `crates/server/src/verbs.rs`: `destroy` drops the volume's presentations once its destroy is recorded.
- `crates/server/src/landing_entries.rs`, `session_log.rs`: docs; `crates/api/src/error.rs` and `instance.rs`:
  `Missing::Presentation`'s words ("replaced, landed, or its volume went").
- Tests: `crates/client/tests/presentation.rs` replaced (it held the old rule,
  `a_killed_clients_presented_landing_is_abandoned_with_it`) by `a_presentation_outlives_the_process_that_presented_it`
  (a real presenter process exits and is reaped; another client grants and lands); `crates/server/tests/presentations.rs`
  gains `a_presentation_goes_with_its_volume` and `another_clients_presentation_still_replaces_and_the_bound_still_refuses`.
- `docs/wip/SLATES_DESIGN.md`: A-874; A-46 and the AUD-29-07 status corrected. GAPS.

## Evidence
- Before (the MacBook, main d1ffa134b): the CLI's session-grant test red as quoted above;
  `a_presentation_outlives_the_process_that_presented_it` red, `Refused(NotFound)` after the presenter's reap;
  `a_presentation_goes_with_its_volume` red, one presentation awaiting after the destroy.
- After: each green; the bound and A-779's replacement unchanged (`another_clients_presentation_still_replaces_and_the_bound_still_refuses`,
  `a_presentation_past_the_bound_is_refused_and_changes_nothing`); `slates-server --test presentations` as recorded in
  the landing.
