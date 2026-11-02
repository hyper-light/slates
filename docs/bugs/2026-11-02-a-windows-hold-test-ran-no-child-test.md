# A Windows hold test selected no child test

Observed while building A-198, 2026-11-02. This defect was in the uncommitted
Step 9.2 process fixture; it was not a shipped WinFsp takeover failure.

## Description and evidence

The two parents timed out waiting for a hold. Jupiter's captured `t3.log`
reported each child as running zero tests, followed by the parents' failures at
the sender's hold receive. The command was
`cargo +1.98.1 test -p slates-anchor --test handles_windows -- --nocapture`.

## Root cause and impact

`spawn_role` supplied the role value as libtest's exact filter. That value was
`sender`, while the ignored test was named `sender_role`. No child body ran.
The sender timeout exposed the mismatch, but a checker that ran no test could
otherwise return libtest's successful zero exit without verifying inheritance.
The same mismatch affected the checker and no-hold checker filters.

## Exact changes and verification

`crates/anchor/tests/handles_windows.rs` uses the actual test names as its role
values. A completed role exits with `ROLE_EXIT`, distinct from libtest's
zero-test exit. Each child has a guard that terminates and observes its exit
on parent unwinding; its normal wait has a typed deadline refusal.

With the filter correction, diagnostic run 41397356987 ran both parent tests
three times on each of `windows-latest` and `windows-11-arm`. With the real
supervisor added, run 41398854106 ran all three parent tests three times on
each architecture. These runs established process behavior; their job status
remained red until the separate clippy findings were fixed.

## Sibling checks

The three role filters in this fixture had the same mismatch and were changed
together. A search of the exact-filter call sites under `crates/anchor/tests`
and `crates/ipc/tests` found no further instance of this mismatch. Existing
children returning zero still need an observable side effect at their parent;
this change makes no claim that their exit code alone proves execution.

The new channel draft also needed integrity and ownership checks before it
could ship: one-bit changes and aliasing inherited handles were accepted.
`a_changed_message_is_refused_before_a_handle_is_selected` and
`one_inherited_handle_cannot_have_two_owners` failed first (six other format
tests passed). CRC32C validation and rejection of duplicate owners made all
eight pass. Those are channel admission fixes in the same new functionality.

`Supervisor::stop` and the forced branch of `stop_step` still discard errors
from killing or waiting for their child. Those pre-existing cleanup paths are
flagged for correction; this change does not claim to repair their error
reporting. The new direct process fixture reports its own cleanup failures.
