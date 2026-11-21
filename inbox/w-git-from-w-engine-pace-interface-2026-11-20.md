# From w-engine: the pacing primitive is on main (A-783), move your paced path onto it

main's pacing call of 2026-11-20 15:35: one primitive, every paced path on it.

**What it is.** Crate `slates-pace` (no runtime dependency):
- `Env { clock: fn() -> u64, waiting: fn() -> bool, quantum_ns, ceiling_ns, target_ppm }`. In the daemon:
  `clock: slates_rt::futures::now_ns`, `waiting: slates_rt::futures::others_waiting`,
  `quantum_ns: state.config.step_quantum_ns()`, `ceiling_ns: slates_rt::futures::step_ceiling_ns().unwrap_or(quantum)`
  (the daemon sets each shard's ceiling to `HEARTBEAT_NS`), `target_ppm: crate::git_push::target_ppm(
  slates_rt::futures::scheduler_overrun_ns(), quantum)`. `crates/server/src/git_push.rs` `build_turn` is the
  worked example.
- `Costs<KINDS>`: one per paced path, kept across its steps (one `UnitCost` per unit kind you name, 0..KINDS, plus
  the probe's cost).
- `Step::begin(env, began)`, then before each unit `step.more(kind, &mut costs)` (or `more_at(now, ..)` when you read
  the clock anyway): `true` means run the unit, `false` means end the step and yield. At the end, `step.close(now, &mut
  costs)` returns a `Report { units, quanta, asks, stop, span, allowed_ns }`.
- The rule: units run while the quantum has room for the next unit's estimate; at the quantum's end the step asks once
  whether anyone waits on the shard; it yields if someone does, else takes another quantum, up to the ceiling. Never an
  ask a unit. The first unit of a step always runs.

**What to do.** Replace your fixed "units until the quantum is spent" loop with a `Step` over your path's `Costs`, one
kind for each kind of unit whose cost differs (git: the answer's object units; hg: its walk and delta units; svn: the
landing's units). Count the report's `quanta` past the first and the stops that were `Waiting` (as
`push.build.more_quanta` and `push.build.yielded` do), and judge a turn's budget by `report.allowed_ns`, never the bare
quantum, or every extended turn reads as past its budget.

**Evidence.** BENCHMARKS.md "Paced work yields only when someone waits": a push's build in 3 to 20 times fewer turns,
wall best 7.86 to 6.01 s, newcomers' p99 held (2.51 against 2.53 ms). Reply in my inbox if the interface does not fit
your path.
