# A refused granted landing left no trail (2026-11-18)

## Description

A landing a human had granted, and that was then refused when it ran, wrote none of its records to the durable audit
log. Its trail ended at its grant, as if the grant had never been used. Measured on the Mac Studio (macOS arm64,
2026-11-18, kiln run r3614 on main 79aee2a17) with three granted landings, each refused a different way:

- by a conflict, an outsider's write to the target after the grant (`LandingConflict`);
- by a change to the volume the grant did not cover (`GrantMismatch`);
- by another attempt holding the target's lease (`LandingLeaseHeld`).

Each read back `landing_planned` (the presentation, no grant) and `grant_issued`, and nothing after. Found by g90 reading
the code; `docs/bugs/2026-11-18-a-presented-landings-plan-was-audited-twice.md` had noted it as a sibling.

## Root cause

A granted landing's records reached the durable log in one place. `finish` (`crates/server/src/landing.rs`) mirrors the
engine's ring records of the run, from `audit_from` on, when the run finishes. Every refusal went through `not_landed`,
which builds the refusal's reply and writes nothing. So the ring's records of the attempt (its plan under the grant, its
validation, any entry it wrote) were dropped, and no record said how the attempt ended.

The server's own refusals before the engine began were answered with the reply alone (`Begun::Refused`). These are the
target's lease, the volume gone, the head's snapshot, the base host away and the target not opened. The lease was
judged before the attempt even had its id.

## Impact

The audit trail (§4.15 "Audit", §4.14) is the record of what was done under each human grant. It showed a refused
granted landing as never attempted, and lost the steps a refused run had taken. The worst case is a run refused after
it wrote entries, such as a target error mid-landing or its base host taken between slices. Its entries were on the
disk with no `entry_written` record of them.

The design's state machine names the refusal as the landing's terminal state (`Validating → Refused`: a conflict, a
lease held, a mismatched grant), and nothing recorded it. A signed landing's verdict was not recorded either. The reply
was right in every case, and nothing else read the ring.

## Exact edits

- `crates/server/src/landing.rs`:
  - `begin_granted` decides the attempt's id and its ring start before it looks at the lease. It answers each of its
    own refusals as a `NotRun` (`Unrun::Server`), never as a bare reply (`Begun::Refused` is removed). `NotRun` carries
    the grant and the ring start, and `Unrun::Engine` holds the engine's refusals.
  - `record_refused` writes the trail in the completion's atom: the attempt's ring records from `audit_from` on, then a
    `LandingFinished`. That record names `refused_state` (`Aborted` once an entry was written, else `Refused`), the
    manifest the attempt planned (its grant's when it planned none) and its signature judged against that manifest. A
    fresh id is spent, as a finished landing's is.
  - `reply_not_run` and `complete_finish`'s refusal call it. The exception is a plan no grant covers (`ends_refused`),
    which ends in a presentation that records itself.
  - `audit_verb` reads no verdict on a landing's end that names no manifest: it had no statement to judge.
  - Unit test `a_refused_attempts_end_is_aborted_once_it_wrote_an_entry`.
- `crates/server/tests/presentations.rs`: `a_granted_landing_a_conflict_refuses_records_its_steps_and_its_refusal`,
  `a_granted_landing_its_grant_does_not_cover_records_its_plan_and_its_refusal` and
  `a_granted_landing_refused_its_targets_lease_records_its_refusal`, each red on main 79aee2a17 (r3614); and
  `a_granted_landing_refused_before_any_manifest_records_no_verdict`, for an end with no manifest and a fresh id spent.
- Red-checked guard by guard on the Mac Studio (kiln r3639), each mutation red alone at its own assertion: no record
  on a run's refusal turns the conflict test red; none on a refusal before the run, the grant-mismatch, lease and
  no-manifest tests; none on the server's own refusals, the lease and no-manifest tests; no grant manifest for an
  attempt that planned nothing, the lease test; a manifest-less end read as a verdict, the no-manifest test (read
  `Some(Unsigned)`); a fresh id not spent, the no-manifest test (both ends under one landing id).
- `crates/api/src/observe.rs`: the docs of `AuditRecord::outcome` and `AuditRecord::verification`.
- Design §4.15 (a status blockquote); GAPS (the A-321 list's entry).

## Siblings swept

- A refused granted landing's durable `LandingRecord` is unchanged. A presented one stays `AwaitingGrant` (its
  presentation is still grantable), and a fresh one has none. Nothing reads a landing record's state: `present` and
  `finish` write it, and no code in `crates/server` or `crates/db` reads it. Reported, not changed.
- Refusals the `Land` verb answers before its task starts are refusals of the request, recorded as its completion,
  not landing attempts. These are the volume or snapshot not found, the caller's rights, a forbidding policy, the
  target not opened, and a second landing of a volume already running. Unchanged.
