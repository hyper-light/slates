# Snapshot size comparison samples different journal states

Date: 2026-09-20 (local). Design: D-4, §4.5 Journal, AC-1.3, Part 5 measurement discipline.

## Failure and independent evidence

The unchanged macOS gate reported 29 ns at 1,000 files versus 53 ns at one million:
24 ns growth against its 17 ns allowance (`/private/tmp/slates-macos-gates-lifecycle.log`).
No threshold has been raised.

A diagnostic added only counters outside the timed operation. On an Apple M5 Max, 18 logical
CPUs, 128 GiB RAM, the release binary reproduced 31 versus 52 ns (21 ns against 18 ns).
The small-tree measurement ran 4,612 snapshot cycles and reached an all-snapshot journal;
the million-file measurement stopped after 512 cycles, evicting only 306 of its 444 named
records. It was still timing destruction of names allocated during tree construction.
Command: `target/release/examples/vfs_bench`; build:
`cargo build --offline --release -p slates-vfs --example vfs_bench`.
Log: `/private/tmp/slates-macos-identity-and-snapshot.log`, 2026-09-21 00:00:44 UTC.
Load averages before this run: 6.01, 5.71, 4.50; no other agent build or test was running.

The second diagnostic held each tree unchanged, replaced one journal's worth of records,
then repeated the same snapshot/destroy operation. The original samples were 52/53 ns at
the smallest/largest sizes; the steady samples were **29/28 ns**, with the middle size at
28 ns. For the million-file tree, named eviction accounted for 11,618 ns of allocator time
in the first sample; the steady sample's allocator time was 166 ns. Log:
`/private/tmp/slates-snapshot-same-tree-diagnostic.log`. That run's original size gate passed
without a code fix, demonstrating why the stopping point made its verdict unstable.

## Cause and correction

The adaptive timer stops after its interval converges, not after a prescribed amount of
journal turnover. Namespace/content probes run before the small-tree sample, but not before
the million-file sample. Tree size is therefore confounded with the journal's history.
The acceptance criterion asks whether snapshot cost grows with tree size; this test compared
different work on different trees.

Prepare every measured tree with one journal's worth of successful snapshot cycles. The bound
is derived from the journal's byte cap divided by the smallest record size, plus one to evict
the final old record. Verify the prior head has left before measurement. Continue measuring
the complete snapshot/destroy path, including append and eviction, under the original time
budget, tree sizes, convergence rule and acceptance allowance. A refused snapshot or destroy
now fails the benchmark; it cannot be reported as a fast successful operation.

The temporary diagnostics are removed. This does not resolve the separate destroy-slice
failure: the two diagnostic runs reported 296,417 ns and 146,250 ns maximum slices, each
releasing 368 units with zero measured allocator-deallocation time. That failure remains
strict and requires its own diagnosis. The eager preparation walk in `Volume::destroy`
is also outside the old slice measurement and must be included in the destroy proof.

## Validation and remaining diagnosis

With all temporary diagnostics removed, the corrected release benchmark passes: snapshot
29 ns at 1,000 files, 30 ns at one million (1 ns growth, 12 ns allowance), clone 234/208 ns,
and no destroy slice beyond the existing bound in that run. Command:
`cargo run --offline --release -p slates-vfs --example vfs_bench`;
`/private/tmp/slates-snapshot-normalized.log`. Strict workspace Clippy and `cargo xtask check`
also pass (`/private/tmp/slates-lifecycle-current-gates.log`). A single passing destroy run
does not close its intermittent failure.

The separate buffered destroy diagnostic captured 281,417 ns externally and 282,000 ns on
the volume's own clock, with a 279,000 ns gap between clock checks. Another failing run
recorded 149,292 ns, 151 µs of process CPU time, no page faults and no recorded preemption.
A later 137,333 ns slice had no individual release longer than 42 ns. No clock implementation
has been changed: a subsequent query-duration probe did not capture that long event.
Logs: `/private/tmp/slates-destroy-clock-trial-4.log`,
`/private/tmp/slates-destroy-resource-trial-3.log`,
`/private/tmp/slates-destroy-buffered-trial-1.log`.
An intermediate experiment printed inside the timed step and perturbed its duration; those
slice results are rejected. Reporting was moved outside the timed step, and every diagnostic
was subsequently removed from both the volume and benchmark. The cause remains unconfirmed.
