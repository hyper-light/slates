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

Rerun the reduced regression, the NFS transport tests and the original mounted 500 × four
history. Whole-image publication itself remains synchronous and scales with retained bytes;
the full history must establish whether bounding requests alone resolves the observed lane.
This report does not establish that arbitrary image sizes fit one cooperative step.
