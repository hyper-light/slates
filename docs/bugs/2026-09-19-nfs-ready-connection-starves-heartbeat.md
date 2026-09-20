# A continuously ready NFS connection starves the heartbeat

Date: 2026-09-19. Design: §4.3 bounded cooperative work, §4.6 NFS, §4.8 D-18.

## Failure and reproduction

Linux CI job `105974090627` runs fsstress at 500 operations × four processes, seed 1.
Its anchor repeatedly kills a daemon whose heartbeat lapses. The same command reproduces
inside the authorized local container (Rust 1.98.0, Debian 13, aarch64 Linux 6.12.76-linuxkit,
four CPUs, 4 GiB, real kernel NFS client, io_uring permitted). The 50-operation prefix passes.

Temporary logs at NFS dispatch, image capture and publication boundaries show completed
operations followed by successful whole-image publications. Before the first kill in the
timed run, 449 requests completed; the slowest took 154,029 µs. The last completed image
was 6,408,798 bytes. An individual completed request did not spend the one-second heartbeat
budget. The connection task served successive ready requests without returning to the executor.
An await whose operation is already ready is not a scheduling point.

The reduced test `nfs::tests::queued_rpc_calls_yield_between_replies` queues two complete
NULL RPCs and EOF over real loopback TCP, then polls the production connection future once.
The original future sends both replies and reaches EOF in that poll, failing the assertion
that another task gets a turn between requests. Command:

```sh
cargo test -p slates-server --lib nfs::tests::queued_rpc_calls_yield_between_replies -- --exact --nocapture
```

Evidence is in `/private/tmp/slates-nfs-queued-fairness-red.log` and
`/private/tmp/slates-mounted-evidence/fsstress-duration-diagnostic.tar.gz`.

## Second cause confirmed after adding the yield

The first full 500 × four run with the request yield still restarted repeatedly and was
stopped after nine heartbeat kills. A second run timed capture, encoding, publication and
heartbeats separately. At 19,469,939 frame bytes, one publication took 486,913 µs,
including 442,423 µs of encoding. Two successive publications stretched the heartbeat gap
to 982,567 µs, almost its entire one-second budget. Image capture took only 2,672 µs.
`Vec<u8>` uses the generic element codec, dispatching a scalar encoder for every byte;
this explains the measured encoding cost. Evidence:
`/private/tmp/slates-mounted-evidence/fsstress-fairness-timing.tar.gz`.

## Fix

Yield after each complete RPC reply and after each accepted connection. This is one protocol
work unit per task poll, with no new timer, threshold or retry. The sibling async connection
loop in `bridge-nfs` needs the same boundary. Durability still precedes a successful reply;
no heartbeat budget changes. Remove the temporary diagnostic logging.

Teach the `Wire` sequence seam to encode and decode byte slices with one checked bulk copy.
The generic sequence implementation still applies to structured elements; `u8` supplies its
own sequence operation. Preserve the length prefix, canonical bytes and schema identity,
and check hostile lengths before allocating. Test-only thread-local scalar-call counters
prove byte vectors use neither scalar loop, with explicit scalar probes proving the counters
are live. The mounted history remains the acceptance test for the combined change.

## Validation and remaining scope

The queued-RPC regression is red without the yield and green with it. The byte-vector
regression is red before the bulk operations (256 scalar encodes for 256 bytes) and green
afterward; the final wire unit suite passes 32 tests with one ignored. Both regressions finish in
0.00 s. A sibling real-socket regression drives `bridge-nfs::serve_connection_async`.

The original mounted history passes with both changes: 500 operations × four processes,
seed 1, all 2,000 logged, daemon alive afterward, zero heartbeat kills. The instrumented
run observed 1,559 publications; the largest frame was 50,538,682 bytes, the slowest
publication was 109,277 µs, and the maximum heartbeat gap was 293,805 µs. At about 20 MB,
encoding took 1,463 µs instead of about 442 ms at 19 MB before the bulk path. These are
single-run debug-build diagnostic timings, not a release benchmark or a performance floor.
The suite record reports 120,636 ms including setup, upstream build and teardown.

Command: `/build/debug/xtask conformance run --suite fsstress --fsstress-ops 500
--fsstress-procs 4 --records /scratch/slates-records --scratch /scratch/slates-scratch --keep`.
Container: `slates-ci-local:20260919`, Rust 1.98.0, Debian 13, aarch64 Linux
6.12.76-linuxkit, four CPUs and 4 GiB, executable RAM scratch, no concurrent test/build.
Record date UTC 2026-09-20, local 2026-09-19. Evidence:
`/private/tmp/slates-mounted-fsstress-bulk.log`,
`/private/tmp/slates-mounted-evidence/fsstress-bulk-timing.tar.gz`,
`/private/tmp/slates-byte-codec-{red,green}.log`.

The diagnostics are removed. The second, uninstrumented mounted history passes all 2,000
operations in 93,309 ms with the daemon still responsive. The final Linux workspace passes
1,506 tests, zero failed, 14 ignored (183 result groups), including all 49 fleet histories,
the six recovery histories, both queued-RPC proofs and the real FUSE coherence test.
Strict workspace/all-target Clippy and `cargo xtask check` pass on Linux and macOS.
Evidence: `/private/tmp/slates-ci-conformance-retry.log`,
`/private/tmp/slates-linux-workspace-current.log`, `/private/tmp/slates-final-host-clippy.log`.
The container uses the explicitly required io_uring backend. Twenty isolated rounds each of
the five reclamation tests and warm-voter vote-preservation history pass; the epoll retirement
leg also passes five tests. Pjdfstest remains red at the same 3,595 cases as CI and is recorded
separately in `2026-09-19-pjdfstest-special-files-and-nfs-limits.md`. Whole-image publication remains synchronous
and scales with retained bytes; this does not establish that arbitrary image sizes fit one
cooperative step. Incremental retained storage and cooperative publication remain owed.
