# The fault probe reported a zero fault on a loaded machine

Found 2026-11-10 by A-223's gate. `profile::tests::a_profile_measures_within_its_budget_and_round_trips_through_json`
asserts `faults.base_ns > 0`. It failed in 1 of 10 full `slates-machine` suite runs on main 39c1a9e4 on fedora-2
(32 CPUs, load 21 to 34), and in 0 of 10 runs of the test alone. A-223 filed it in GAPS on its own branch.

## Cause

`slates_machine::probes::faults` measured two regions of `FAULT_REGION_PAGES` (256) base pages one after the other:
mapped and unmapped untouched, then mapped, every page touched, and unmapped. The fault cost was the difference of the
two medians, divided by the 256 pages, with `saturating_sub`. The measurements are separate. On a loaded machine the
untouched region can come out as slow as the touched one, or slower, and the probe then reported 0 ns per fault. A
positive difference under 256 ns rounded to 0 too. The populated region (Linux `MAP_POPULATE`) was judged the same way.

## Impact

No division by zero, since consumers take `faults.base_ns.max(1)` (`crates/server/src/config.rs`,
`crates/mem/src/region.rs`). But a 1 ns fault fed every bound derived from it: the arena region's pages
(`100 × 2 × syscall / fault`) and the pre-fault slice. A daemon that measured its profile at the wrong moment sized
those for faults that cost nothing.

## Fix

- A per-page difference that is not positive measured the load, not the faults. So the base pages' probe is refused
  `MeasurementTimeout { probe: "faults" }`. That is the refusal a probe that could not measure within its bound already
  gives (the wake probe's). `MachineProfile::measure_patiently` then measures again within the probe's wall ceiling
  before the refusal stands.
- A populated or huge-page region measured at nothing reads as unmeasured (`None`), as on an OS without the call.

## Exact edits

- `crates/machine/src/probes.rs`:
  - `faults` returns `Result<FaultCosts, MachineError>` and measures through `faults_from`.
  - `faults_from` takes the region, populated and huge measurers, so a test can hand it a loaded machine's timings.
  - The per-page difference is `checked_sub`, divided, and refused or `None` unless positive.
  - `FaultCosts`' field docs say so.
- `crates/machine/src/profile.rs`: `MachineProfile::measure` takes the fault probe with `?`.
- Tests, in `probes.rs`, red on main 29ef4521 in their own commit (ef529d8a):
  - `a_touched_region_no_slower_than_the_untouched_one_is_refused_never_a_zero_fault`. Equal, 100 ns slower over the
    region, and faster touched regions are each refused. Main answered `Ok(0)`.
  - `a_populated_region_no_slower_than_the_untouched_one_is_unmeasured`. Main answered `Some(0)`.

## Verification

See the commit.
