# A presented landing's plan was audited twice (2026-11-18)

## Description

A landing that was presented and then landed under a grant recorded its presentation's plan twice in the durable
audit log. Measured on the Mac Studio (macOS arm64, 2026-11-18, `a_signed_landing_is_judged_recorded_and_read_back`
in `crates/server/tests/signed_work.rs` with the landing's records printed): its trail read `landing_planned` (no
grant), `grant_issued`, `landing_planned` (no grant) again, `landing_planned` (under the grant), `landing_validated`,
`entry_written`, `landing_finished`.

## Root cause

Two writers record the same step. `present` (`crates/server/src/landing.rs`) appends the presentation's
`LandingPlanned` to the durable log itself. The landing engine also pushes a `LandingPlanned` into its in-memory audit
ring (`slates_land::engine::Audit`) when the presentation's plan is whole. A granted run's `finish` then mirrored every
record the ring held for the landing's id, and the granted run lands under its presentation's id (AUD-29-07), so the
presentation's ring record was mirrored a second time. The same mirror would re-record a resumed landing's first
attempt, since an aborted attempt keeps the id and its own finish had mirrored its records already.

## Impact

The audit log, the landing's durable trail (§4.15, §4.14), showed a plan that never happened twice; a consumer
counting a landing's presentations or reading its steps in order read a wrong history. Signed work's verdicts agreed
on every copy (A-321), so no provenance was wrong. Nothing else read the ring.

## Exact edits

- `crates/land/src/engine.rs`: `Audit::next_seq`, the sequence the next record takes.
- `crates/server/src/landing.rs`: `begin_granted` notes the ring's next sequence before the run pushes a record; the
  `Planning` and `GrantedRun` carry it as `audit_from`; `finish` takes a `FinishedRun` and mirrors only the ring's
  records of the landing at or after `audit_from`.
- `crates/server/tests/presentations.rs`: `a_presented_then_granted_landing_records_each_step_once` (red on the old mirror).
- Design §4.15 (a status blockquote); GAPS (the A-321 entry's line for it removed).

## Siblings swept

`finish` is the ring's only reader. A granted run that is refused (`not_landed`) mirrors none of its ring records, so
a refused landing's plan and validation never reach the durable log (from reading `complete_finish`; reported, not
changed here).
