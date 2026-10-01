# A granted landing held its shard for all of its work

**Date:** 2026-10-01. **Area:** `slates-land` (`engine.rs`, `os.rs`), `slates-server` (`landing.rs`, `daemon.rs`).
**Audit:** AUD-29-25 (P1). **Design:** §4.15, §4.3, R9.

## Description

A granted landing ran as a task on its volume's owner shard. The task called the engine's `land` once:
plan, sweep, validate, write every entry, sync, advance — every host read, write and sync before it
returned. A cooperative executor cannot preempt a function that never yields, so the shard served
nothing else meanwhile: no provisioning, no other client's verbs, no lease checks, no shutdown.

**Measured before the fix** (the test below with the slice budget unbounded; Apple M5 Max, macOS 26.4.1,
load average 4.7–7.0 from other sessions, 2026-10-01):
- a landing of 600 files took one 810.6 ms slice;
- a probe that arrived while it ran waited 831.0 ms, the whole landing.

## Root cause

The engine had no notion of a step. Its phases were loops over a manifest inside one call, and the server
had nothing to resume between them.

## Fix

- **An owned, resumable run** (`LandingRun`). Everything a landing does after its grant and lease are
  checked is taken one unit at a time, in §4.15's order and under the same grant and lease fences (checked
  per entry against the landing's own clock). The units are: a directory swept, an entry validated, an
  entry written, a directory synced.
  - `step(budget)` runs units until the budget passes, checked between units.
  - The finish (the volume's advance and the report) is never run by `step`. The caller runs `finish`
    where the landing's records commit as one atom with the request's completion.
  - `abandon` ends a run that will not be stepped again: it closes its directories and advances nothing,
    leaving the state a crash leaves.
  - The engine's state between slices (`Saved`) is the borrowed `Landing`'s fields, lent back each slice.
    The one-call `land` is the run stepped through; the whole-phase loops it replaced are gone.
- **The server steps the run** inside the landing's task.
  - Each slice takes half the shard's live step quantum (as the archive walk does), then yields.
  - The finish and the completion record commit in one transaction, as before.
- **An overlay volume keeps its base host between slices.** The writer lends its host back to the volume's
  slot (`OsLand::lend`/`resume`), so the volume serves its base while the landing waits.
- **An unnamed landing lands a snapshot of the head** taken when it begins, destroyed when it ends. Writers
  go on between slices without moving what lands; exact snapshot landing is A-48/49's. A destroy that is
  refused is counted (`landing.implicit_snapshot_kept`).
- **One granted landing of a volume at a time.** A second is refused `LandingLeaseHeld`, naming the running
  attempt, since two would advance one overlay.
- **Losing the volume or its base host mid-run ends the run.** A volume destroyed between slices, or an
  overlay whose base host is not in its slot when a slice comes, ends the run as a crash does: its entries
  stay in the overlay for a resume, and the client gets a typed refusal.

## Tests

- `land/tests/oracle.rs`:
  - `a_landing_stepped_one_unit_per_slice_lands_exactly_as_the_one_call`, on four simulated filesystems
    (exchange or not, unnamed temporaries or not): the same disk, host writes, entry outcomes, counts and
    overlay, in more slices than entries.
  - `a_sliced_landing_crashed_at_every_write_instruction_resumes_to_the_reference`: every path old or new
    (or aside), the resume reaching the reference, every sibling swept.
- `server/tests/landing_fairness.rs`, `a_large_landing_leaves_its_shard_serving_between_its_slices`, on a
  one-shard daemon. It lands 600 files written through NFS while a second client probes `list`. It expects
  the landing done with every file, in many slices, with probes answered during it and none waiting for the
  whole of it.
  - With the slice budget unbounded (the old shape) it fails: 1 slice, the probe waited 831.0 ms of 831.5 ms.
  - With the fix: 1.13–1.80 s in 1,093–1,108 slices; the longest slice 22.8–31.7 ms, which is one unit (a
    file's write and `fsync`); 1,021–1,035 probes answered during it; the longest probe 34.6–36.9 ms
    against a 98–163 µs baseline.
  - One run, on the loaded machine, took 6.26 s with a 205 ms slice: a single `fsync` stalled. A unit is
    bounded by the disk's own latency, not by the budget.
  - The landing without the probing client: 596–613 ms in three runs, against 831 ms for the one call.
    Slicing costs no throughput here; the longer time under probing is the shard serving 1,000 probes.
- The engine's 52 tests, the server's landing, lease, recovery and snapshot-landing tests, and the CLI suite
  with a live kernel mount all pass.

## Large files (the same day)

A file past one content window was one unit: its bytes read whole into one buffer and written in one call.
Now:
- **The copy is a window per unit.** A temporary is created, then one window of the volume's content is
  read (`Source::read_at`) and written per unit, then mode, mtime and sync, and the placement. The landing's
  memory and each unit's work are bounded by the window however large the file.
- **The fences are checked again before placement,** since a long copy can outlive its grant or lease. A
  copy that may not be placed has its temporary removed and is reported skipped.
- **One implementation.** The one-call path loops the same units, and `read_overlay_bytes` and `fill_temp`
  are gone.
- **Tests.** The crash scenario now holds a file of three windows and a byte, with position-dependent bytes.
  Both the sliced-equivalence oracle and crash-at-every-instruction (sliced and not) pass over it, so a
  crash inside every chunk write resumes to the reference. The daemon's 2 MiB partly-edited base file lands
  through it too.

## The lease keepalive (the same day)

A landing's target lease had a term of the failover bound and nothing renewed it, so a landing longer
than the term skipped its remaining entries (`LeaseEnded`) and ended partial. Now:
- **Renewal between slices.** Once half its term has passed, the landing's task re-takes the lease on the
  control shard as its own holder, and the run is fenced by the renewed term (`LandingRun::renew_lease`,
  which accepts only the same target and holder).
- **A lost lease stays lost.** A lease another attempt took meanwhile is not renewed; the run's per-entry
  fence then stops its writes as before.
- **Counted either way** (`landing.lease_renewed`, `landing.lease_not_renewed`).
- **Test:** `a_landing_longer_than_its_lease_term_renews_it_and_lands_everything`. Under a 1 s term,
  1,500 files land in 1.57 s with one renewal, all written, `done`. With the renewal removed: 1.07 s,
  1,040 of 1,500 written, `partial`.

## Owed (AUD-29-25 open in part)

- **The p99/p999 shard-step distribution under a landing is not yet recorded.** It belongs with the R9
  provisioning histogram as a lane.

## Siblings reported

- **The synchronous client answers `Stalled` to any reply slower than the liveness budget (1 s),** including
  a landing still at work. `slates land` of a large tree reports `Stalled` while the landing goes on. The
  test waits the lease's term instead. The client needs a long-verb wait that the daemon's liveness, not a
  fixed deadline, bounds.
