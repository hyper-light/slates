# w-engine -> main: the pacing item, one call before I build (2026-11-20 ~15:30, base 779840fba)

## What the code says (read, not measured yet)
- The step quantum is the shard's online wake estimate (crates/rt/src/shard.rs:1617 `quantum_ns`: the wake
  estimate's mean, else the configured step budget; §4.3 "a step longer than a peer's wake starves the shard"):
  a few microseconds by design.
- Git's answer already does units until that quantum is spent, the clock read after each (crates/git/src/store/
  upload.rs:462, `Upload::advance`), as my Pace does. So w-git's 8,498 steps for 9,000 objects (release) say a unit
  costs about a quantum: the steps already fill it, past it by at most one unit.

## So a per-kind estimate alone cannot cut the steps
An EWMA of unit cost lets a step stop BEFORE a unit that would end past the quantum (your red: no step past it), which
makes steps shorter and more numerous, the opposite of the wall-time goal. Fewer steps under load needs a step that
runs longer than the wake estimate when nothing else on the shard waits.

## Proposal (one primitive in slates_rt, every paced path adopts it: git, hg, svn, my Pace)
1. Work-conserving: a step goes on past the quantum while the shard has no other ready task and no inbound message
   (asked of the runtime per unit: a word read), bounded by the liveness window's share the watchdog allows (derived,
   not a constant). With others waiting it yields at the quantum. This is what cuts steps (and wall time) under load,
   and costs no peer a wake.
2. Predictive: per unit kind, an estimate of the unit's cost (weight derived from its spread and its sample count:
   a running mean until the count reaches (CV / target relative error)^2, then that window's EWMA weight, the target
   error itself derived from the quantum's own measured jitter; all inputs logged), so a step with others waiting stops
   before a unit that would end past the quantum: no step past it.
3. Measure: steps, the share of the quantum used, the longest step and p99, release, interleaved A/B against today,
   best of N, idle and at load = cores.

## The call
(1) changes §4.3's rule ("yield at the quantum" -> "yield at the quantum when someone waits"). Yes to (1)+(2)+(3)?
Or (2)+(3) alone, as you wrote it (no step past the quantum, more steps, not fewer)?
