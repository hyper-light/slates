# A delivery unit test probed a freshly closed descriptor number, which a parallel test's pipe reused

Date: 2026-09-14
Area: `crates/ipc/src/delivery.rs` (the `descriptors` unit tests of the consumer-capability carrier)
Severity: test-only — an intermittent false failure of two tests, never a shipped defect; found while
integrating `agent/consumer-capability` onto main.

## Symptom

In the wiped-target validation of the consumer-capability merge (2026-09-14 08:26), `cargo test -p
slates-ipc --lib` failed 2 of 17:

```
delivery::tests::descriptors::a_whole_record_takes_once_and_the_descriptor_is_closed ... FAILED
delivery::tests::descriptors::a_non_number_and_a_closed_number_are_typed ... FAILED
```

Both passed 17/17 on the next run, in parallel and with `--test-threads=1`, and the branch's agent had
never seen them fail.

## Root cause

`a_non_number_and_a_closed_number_are_typed` opened a pipe, remembered its read end's number, closed
both ends, and asserted that `take_named(number)` is `NotInherited`. The kernel hands the lowest free
number to the next `open`/`pipe`, so under libtest's parallel execution the just-closed number was
reused at once by `a_whole_record_takes_once_and_the_descriptor_is_closed`'s pipe. The first test then
took *that* pipe's record (a live descriptor of the right kind, so no `NotInherited`), and the second
found its record gone. The race needs the two tests to interleave inside a few microseconds, which the
integration run's load supplied.

## Fix

The unopened number is now `i32::MAX` — the largest a descriptor can have, which no process table
reaches (this box: `kern.maxfilesperproc` 245,760) — so the case is deterministic regardless of what
other threads open. The victim test needed no change: it owns its own pipe. Verified: 10 consecutive
parallel runs of the suite, 17/17 each.

## Sibling sweep

The other descriptor tests (`pipe_holding`, `pipe_still_open`, the directory and terminal cases) each
open their own descriptor and hand its number to the take without closing it first, so none depends
on a number staying closed. `crates/ipc/tests/delivery.rs` (the cross-process test) spawns a child
per case and is unaffected.
