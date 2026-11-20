# w-provision-gate: R9's sub-50 µs provisioning gate exists and holds under load (main, 2026-11-20)

## Why
Ada, 2026-11-20 ~13:20 PST, after a Slates-vs-Sapling comparison of the two checkouts (Slates 896233d1f, Sapling d49b955d3ae): "dispatch agents to tackle these areas and gaps for slates." The comparison's central finding: "Slates' fast copy-on-write volumes and its precise merge engine are individually real, but the ordinary mounted-editor workflow does not yet connect the two end to end." Sapling's checkout is at ~/Projects/sapling (read it, never write it); EdenFS's docs are under eden/fs/docs.

## Your work
- **R9 is a locked rule:** sub-50 µs provisioning, with "the provisioning histogram (p50/p99/p999/max, spinning and
  parked) a permanent, ratcheted CI gate".
- **The comparison found:**
  - 25 µs p99, one client on a quiescent M5 Max (2026-09-05);
  - 1.6-1.7 ms p99 on a loaded x86 host (GAPS ~1465, 2026-10-20);
  - the CI gate documented as absent.
- (1) Build the gate: crates/client/examples/provision_bench.rs already measures it. Wire it as the permanent,
  ratcheted gate in CI's workflow, with the histogram recorded per run and the floor only tightening. The CI wiring
  is a run request for w-lint and main per §8; the bench and its assertions are yours.
- (2) The loaded p99 is a performance gap, fixed up front:
  - measure where the 1.6 ms goes under load (instrument, don't guess);
  - fix it, red first (a loaded-p99 bound derived from the quiet one and the load);
  - record the numbers in BENCHMARKS (machine, date, load).

## Rules
Follow briefs/common.md in full: clone the state branch to ~/fanout-state-<you> with `git clone --branch agents/fanout-state git@github.com:hyper-light/slates.git` (HTTPS if SSH fails). The sections that bind this work most: CLAUDE.md §8 BINDS (focused unit and integration tests only; heavy and cross-platform runs go in run-requests.md), MEMORY SIZING (size from the work, no allocation the design can avoid), STORAGE and the one-unit changeset ruling, PERFORMANCE GAPS UP FRONT, and DECISIONS BACKED BY DATA (a design A/B as a measured example). Red first for every fix. Land each piece on slates origin/main (merge, read the diff stat, push), confirm with merge-base, update the design section, its status line and GAPS in the same change, then stop and report: landed (commit), numbers (machine, release), next. Never force; never checkout, switch, stash, reset or commit-tree. No installs or system changes. Keep progress/<you>.md current and pushed.
