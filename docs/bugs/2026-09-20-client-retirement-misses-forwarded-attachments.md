# Client retirement misses attachments on another owner shard

Date: 2026-09-20. Design: §4.7 failure matrix, D-7, D-16, T-2.3.

The killed-client regression held one volume on the same partition as its client. Extending
the history to hold volumes on both owner shards fails in 5.79 s: the off-shard attachment
remains after SIGKILL. Command: `cargo test --offline -p slates-client --test reap --
--test-threads=1 --nocapture`; log `/private/tmp/slates-cross-shard-reap-red.log`.
The surviving observer owns a separate attachment and must retain it until exact detach.

`reap_client` scans only its local partition, then releases the client's seat and id. An
attach forwarded to its volume's owner leaves its record there. Pending forwards also
survive the old cleanup; a later forward can recreate an attachment after retirement.

## Fix

Mark a dead client's seat retiring and stop draining its ring. Cancel its unsent forwards.
Keep the seat/id reserved until all owner shards have acknowledged cleanup. On each owner,
first gather the exact attachment ids; this read is queued after already-sent synchronous
attach verbs on the same control channel. The runtime admits and first-polls these messages
in producer order. Then remove only those frozen ids in cooperative batches. A refusal
keeps the seat retiring, counts the failure and retries at the existing liveness cadence.
No new timer, unbounded task population or per-write coordination is introduced.

The gather is read-only: a timed-out gather that runs later cannot remove a resumed client's
new attachments. A timed-out removal holds only the frozen ids, which are not reused during
this daemon's lifetime. Removal releases green pins but leaves write leases to expire by
their terms. A late reply must not write to a seat reused by a different client.

Scope correction: cross-node `Attach` is currently excluded by both forwarded-read and
forwarded-write classification. SDK client ids therefore refer to this daemon in this path.
Enabling remote attach will require origin-scoped lifetime ownership; this change must not
claim that unimplemented path is established.

## Validation (2026-09-20)

The same two-owner SIGKILL regression passes in 3.81 s
(`/private/tmp/slates-cross-shard-reap-green.log`). It keeps the original observation budget,
requires cleanup before the original lease expires, preserves the surviving client's attachment,
and requires that survivor's exact detach to succeed. The affected portable suite passes 95 tests,
with two existing ignored tests (`/private/tmp/slates-lifecycle-portable.log`). The Linux
io_uring workspace rerun also passes (1,554 tests, zero failures, 14 ignored;
`/private/tmp/slates-linux-lifecycle.log`). Queued-forward and refusal/cancellation histories
remain open; the passing ordinary SIGKILL history is not evidence for those distinct cases.
