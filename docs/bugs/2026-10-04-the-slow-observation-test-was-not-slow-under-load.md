# The slow observation test was not slow under load (2026-10-04)

**Description.** `crates/server/tests/observe.rs`,
`a_budget_that_elapses_first_is_named_and_the_late_reply_is_discarded_and_counted`, failed every run on a loaded
machine (load average 20–64, Apple M5 Max) at HEAD `6832c10`. The test expects the deadline error; the observation
returned `Ok(9)` instead: "a question still running at its deadline ends in the execution stage: Ok(9)".

**Root cause.** Since A-65 (`240013e`), an observation's budget counts the observed shard's own CPU time. At a wall
deadline it goes on while the shard is working and has not yet run the budget, so a starved shard is answered. The
test's slow question spun twice the budget by the *wall* clock. A shard given under half a core finished that spin
having run less than one budget of CPU, and the runtime answered it, as A-65 intends. The test's premise, "late by
construction", held only on an uncontended machine.

**Impact.** Test only; the runtime's behaviour is the designed one. The lane would have been red on any loaded CI
runner.

**Edits.** `observe.rs` gains `spin_shard_time`, which spins until the calling shard has run the requested CPU time
(`registry::shard_cpu` of its own holder), with the wall clock only where the platform has no thread clock (Miri,
matching the runtime's own fallback). `SLOW_QUESTION_NS`'s doc names the clock. 3/3 passes under the same load.
Sibling sweep: no other test expects `ObserveStage::Execution` from a wall-clock spin.
