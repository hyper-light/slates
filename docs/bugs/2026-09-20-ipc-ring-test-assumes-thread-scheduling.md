# IPC ring test assumes a thread runs within the spin window

Date: 2026-09-20. Design: §4.7 wake strategy, AC-2.1/T-2.6.

## Reproduction and cause

[Linux job 106138990747](https://github.com/hyper-light/slates/actions/runs/35533762695/job/106138990747)
ran `2fb843f`. `a_round_trip_completes_while_the_client_spins` received all three correct
replies but failed because the daemon issued one wake rather than zero. Its newly spawned
daemon thread is not guaranteed to run within the fixture's 200 µs spin window.

The unchanged test failed on the first local trial with three wakes rather than zero:
`rings-19630c121040f236 a_round_trip_completes_while_the_client_spins --exact --nocapture`,
in the approved Linux container constrained to one CPU (`--cpus=1 --cpuset-cpus=0`).
All replies were correct; the test finished in 0.00 s.
Log: `/private/tmp/slates-ipc-rings-red.log`.

The sibling late-reply test has the inverse assumption: sleeping for 20 ms does not prove
the client has started waiting. Even after it arms the parked flag, the protocol deliberately
allows the reply recheck to avoid a kernel wait. Spurious wakes also make exactly one park
an invalid requirement.

## Fix

The fast case publishes the replies before calling `wait`, then verifies ordered ids,
payloads, kinds, zero wakes and zero parks. The delayed case withholds its reply until the
client actually arms its parked flag. It verifies delivery and disarming, allowing the
protocol's recheck race and spurious wakes. Neither case asserts a scheduler deadline as
a performance guarantee. No production spin window or reply deadline changes.

The separate IPC and provisioning benchmarks retain latency measurements and the existing
AC-2.1/T-2.6 performance gates. A functional pass does not establish those measurements.

## Validation

The corrected six-test binary passed **32/32 complete trials (192 test executions)** in
that same single-CPU container. Native macOS passed all six tests in 0.01 s.
Commands: `/private/tmp/slates-run-ipc-rings-green.sh` (build, then 32 bounded binary runs)
and `cargo test --offline -p slates-ipc --test rings -- --nocapture`.
Logs: `/private/tmp/slates-ipc-rings-green.log` and
`/private/tmp/slates-ipc-rings-host-green.log`. Full CI-equivalent validation is tracked
with the status paging and conformance repairs; these results alone do not close it.
