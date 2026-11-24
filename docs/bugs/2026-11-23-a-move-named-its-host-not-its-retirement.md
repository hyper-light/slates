# A move named its host, not its retirement, so a re-admitted host's later rounds took its successor's own objects over (2026-11-23, §4.8 "Promotion and takeover", A-251, A-309, A-883)

Date: 2026-11-23
Area:
- `crates/db/src/register.rs`: `MovedFrom`, `Acceptor::move_authority`, `PersistedAcceptor`,
  `RegionalConfiguration::retirement_version`.
- `crates/db/src/takeover.rs`: `copy_falls_to`, `promise_object`.
- `crates/server/src/fleet.rs` (`reconcile_held_copy`, `install_on_copy`) and `crates/server/src/takeover.rs` (the
  page's and the round's promises).
- `crates/vfs`: the image version (34) and the delta version (12).
Regressed from: A-251 (2026-11-09), which recorded the owner a move came from and never the retirement that moved it.
Found by: the chaos nemesis's settle failure on CI (seeds 14946995733817154046 and 8173581894251929445), "the fleet
settled, or kept changing". A diagnostic build on adacachy2 then showed the mechanism in runs that passed.

## Description

A takeover's install moves a retired owner's copies to the owner's successor. The holder records the owner the copy
came from (`Acceptor::moved_from`), and a takeover round lists the copy by that owner's line
(`takeover::copy_falls_to`). That is how the successor still learns a copy once routing names it as a live owner.

The record named a host and not a retirement. A host whose retirement completed can be admitted again under the same
id, and it can retire again. A copy moved at its first retirement then fell in every later round of that host, with
the asker as successor. A-251 says the asker's own objects never fall in another host's round. They did, and the
volume was the asker's own live volume, adopted in the first round.

Two symptoms followed:
1. **A spurious self-takeover.** Where every holder's copy carried the stale move, the successor's round learned its
   own live volume, adopted it again at a higher epoch and fenced its own log.
2. **A give-up, and a fleet that never settles.** Where the holders disagreed, the round could not complete. A holder
   whose copy was written after the move has no move recorded, so it refuses the object by name. The object waited
   for that promise, was given up at the failover SLO, and the share was never confirmed. So the retirement was never
   completed and the fleet never settled on one configuration.

## Measured

- **CI (the failing runs).** The settle failure's dump showed the chain: b created V; c took it over; b, re-admitted,
  took it back from c at fence 3; c, re-admitted, was retired again at version 27. b's round then counted:
  - `fleet.takeover.listed_by_moved_from` 371;
  - `unpromised` 246;
  - `named_refused` 233;
  - `given_up` 3.
- **adacachy2.** A diagnostic build at ae43b42ea, in a scratch copy and never landed, logged each round's local copies
  (owner and moved-from), every answer, every by-name refusal and every give-up. Over 14 runs alternating the two
  seeds, nothing was given up and nothing refused by name.
- **The mechanism in a passing run.** Run 11 (seed 14946995733817154046):
  - a took V back from c at epoch 3 when c was superseded (8.29 s).
  - c, a deafened candidate in two later faults (15.9 to 20.1 s), was retired again and re-admitted.
  - In c's next round, a learned V, its own volume since 8.29 s, from its own copy and from b's. Both had owner a and
    moved-from c.
  - a adopted V again: the copies' records went from epoch 3 seq 8 to epoch 4 seq 8.

## Root cause

`Acceptor::move_authority` recorded `Some(previous)` and nothing of the retirement. `copy_falls_to` asked whether the
previous owner's line ends in this round. Once the same host retired again, its line ended in the new round, with the
same successor whenever the configuration ranked it again.

A-309 had already met the same reuse of an id for the configuration's completion record: `admit` drops an earlier
takeover's record, since "it says nothing of the member it names now". The holders' records of a move were never held
to that rule.

## Fix

- **A move names its retirement.** `MovedFrom { owner, retired_at }`, where `retired_at` is the version of the
  retirement whose install made the move (`Retirement::version`, read with
  `RegionalConfiguration::retirement_version`, as the lease gate's `DepartedOwner::since_version` already is).
  - `Acceptor::move_authority` takes that version, `None` for a move from a live owner.
  - `promise_object` passes it through.
  - The install walk and the page's and round's promises read it from the configuration they hold.
- **The rule.** `copy_falls_to` counts the move only while the configuration keeps that same retirement of that owner.
- **The format.** `PersistedAcceptor` carries the record, so a warm restart keeps the version. The image goes to 34
  and the delta to 12. No reader of the old layouts is kept: an older image or delta is refused by its version, and a
  register whose record is the host alone is refused as truncated (CLAUDE.md §2.7).
- **Rejected.** Clearing the move at the install that admits the host again. A lagging holder can install straight from
  the first retirement to the second without seeing that admission, and it would keep the stale move.

## Tests (adacachy2)

- `a_copy_moved_at_a_hosts_earlier_retirement_falls_in_none_of_its_later_rounds` (`crates/db/src/takeover.rs`). It
  goes through the real configuration: retire, move, every survivor confirms, admit again, retire again. It runs on the
  copy and on its twin rebuilt from its encoding.
  - Red, with the version ignored in a scratch copy: "a move made at host 1's first retirement says nothing of its
    second".
  - Green: the db library's 185 tests pass.
- `a_volume_taken_from_a_host_is_not_taken_over_again_when_the_host_retires_a_second_time`
  (`crates/server/tests/fleet.rs`, three daemons). Expect the same node shipping the volume at the same fence after its
  first owner's second retirement, and no live takeover or give-up counted meanwhile.
  - Red, with the version ignored in a scratch copy: the successor's write was refused `StaleEpoch { current: 3 }` and
    then `Overloaded` to the end of its wait. The second round's promises had fenced the volume's holders above the
    successor's own epoch, 2. The same scene with a plain end-of-scene check had passed once, because the write was
    sent until it was answered.
  - Green: one write, answered `FsDone` at once, and the successor still at fence 2 before and after it. That held in 3 of
    3 runs: one with the final assertions, two with the earlier end-of-scene check.
- The register nemesis (`crates/db/src/register_nemesis.rs`): each takeover's install is now a move from a retired
  owner, the owner hosts taking turns as a re-admitted id does. Every holder back from a restart must name the move it
  named before, through the encoding. It counts its restarts across a move.
- Updated to the record and its retirement:
  - the daemon's three moved-copy tests: the page, the delta over a checkpoint, and the held image's warm restart;
  - the register's persisted-state tests (round trip, cut, hostile);
  - the lineage model's own copy of the rule.
- The whole fleet suite file: 238 of 239. The one failure, `a_successor_restarted_warm_takes_a_write_to_the_volume_it_took_over`,
  is GAPS' open load flake. A diagnostic build of this change logged its failing runs: A-883's version check refused
  no copy in them. CI's run at ae43b42ea, without this change, failed it the same way on Windows x64: "the takeover of
  the creator's objects gave up 1 of them".
- Focused suites, green: the db library (185); the lineage model (4); vfs `checkpoint_log` and `recover` (10 and 50); the
  server library's takeover, move and restore tests (141).

## Impact

- Any host the fleet retired, re-admitted and retired again (a false death, a partition healed, a node that kept its
  id across a restart) could make its earlier successor take its own volumes over again at a higher epoch. That fenced
  the volume's own log and refused the successor's writes under its own fence until it moved on.
- Where the holders disagreed, the object was given up, verbs on it answered `NotRecovered`, the retirement never
  completed and the region stayed split.

## Siblings (found by reading, not changed)

- `ShardState::host_takeovers` is keyed by the retired host's id. A lagging node that still owes a confirmation of a
  host's first retirement, and installs that host's second retirement directly, restarts the old round. It keeps its
  `adopted` set and its `began_ns`, so the failover estimate is timed from the first round's start. Reported to main.
- `ShardState::departed_owners` already carries the retirement's version (`since_version`) and is keyed by object.
