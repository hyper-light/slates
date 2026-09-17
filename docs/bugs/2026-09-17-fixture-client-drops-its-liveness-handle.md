# Fixture clients close their Linux liveness socket while retaining the rings

Date: 2026-09-17
Design: §4.7 failure matrix, R5, D-8 ownership.

## Failure and evidence

Ubuntu job [105312670699](https://github.com/hyper-light/slates/actions/runs/35253899553/job/105312670699)
failed the holder-mismatch history in `AwaitPlaced` and the inputs-placed history in `Destroy`.
Both were reproduced independently in the Linux aarch64 image over `a26a5a6`: holder-mismatch
10.24 s (again 10.25 s with instrumentation), inputs-placed 10.24 s. The holder's mismatch
was counted after 0.246 s, so it had already refused the version correctly. Immediately before
`AwaitPlaced`, a traced observation found **zero clients** on the owner. Its request then spent
its entire 5-second reply budget waiting on an orphaned ring. Scheduling overruns were 0–1 ms.

## Root cause and impact

The fixture decomposed `Connected` and called `ClientEnd::with_doorbell(region, doorbell)`.
That dropped `Connected.liveness`. Linux's liveness handle owns the control socket: its peer
then sees EOF, so the daemon correctly reaps the idle client after the liveness budget. Retaining
the RAM ring does not retain a server-side client. macOS probes the still-live process id, hiding
the mistake. The production Rust client already uses `ClientEnd::connected(connected)`.

The same partial constructor occurred in five server fixtures (fleet, daemon, attach_forms,
nfs_mount, virtiofs) and the IPC cross-process rendezvous fixture. Any sufficiently long idle
window could cause a false product failure on Linux.

## Fix

Move the complete rendezvous result into `ClientEnd::connected` at all six sites. Remove the
partial `with_doorbell` constructor, so the obvious public constructor for a rendezvous retains
its liveness and completion resources together. `ClientEnd::new` remains the raw-region seam
for tests and transports that explicitly own their other resources; it is not a rendezvous adapter.
Remove the temporary `[DEBUG-ci-reap]` observation after confirmation.

## Reproduction

Build the Linux fleet test as recorded in the scheduler-quantum report, then run each exact test
with `--nocapture --test-threads=1` in a nonroot container (4 CPUs, 2 GiB, no external network,
no capabilities), supervised at 90 seconds. `SLATES_FLEET_TRACE=/dev/shm/fleet.trace` records the
request and membership timings in RAM; captured logs are `ubuntu-holder-1.log`,
`ubuntu-holder-diagnostic.log`, and `ubuntu-inputs-before.log` in the session scratch directory.

- `a_holder_whose_recomputation_mismatches_refuses_the_version_loudly`
- `a_merge_record_is_issued_only_once_its_inputs_are_placed`

With complete connection ownership, the holder-mismatch history passed in 6.24 s and the
inputs-placed history passed in 5.23 s. The formerly stuck verbs both replied in less than
1 ms (trace resolution). This diagnosis does not explain the separate fresh-member
takeover or conformance failures.

Strict Clippy for IPC/server and all their targets passed (2.01 s); the five IPC rendezvous
histories passed (0.02 s). The sibling sweep found no remaining partial-constructor call.
