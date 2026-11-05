# A call waited for its acknowledgement's scatter (2026-11-05, §4.9, R9)

## Description

Every `ack_every` replies (half the ring's slots), the synchronous `Client::call` (`crates/client/src/client.rs`)
acknowledged them before sending its own request, and waited for that acknowledgement's reply. The daemon answers an
acknowledgement only after every shard has released the client's records (`verbs::scatter_acknowledge`, a scatter and
a gather), so the call that carried it paid every shard's turn first. The async path had always sent the same
acknowledgement without waiting (`Client::begin_ack_if_due`).

## Measurement

- A variant of `provision_bench` (not kept) split the single-client creates by whether the client acknowledged just
  before them (macOS arm64, 2026-11-05 07:25 local, after a load of 30–46; the daemon's boot probe measured a 2.5 ms
  wake that run, so the absolutes are inflated): **133 of 2,000 creates (6.7%) carried an acknowledgement, median
  2,497 µs, against 169 µs for the other 1,867**. At 6.7% of the samples, they set the p99.
- `crates/client/tests/acknowledge.rs` (`a_call_is_never_held_behind_the_periodic_acknowledgement`): with the other
  shard of a two-shard daemon held for one liveness budget, the status call that carried the acknowledgement **failed
  `Stalled { after_ns: 1000000000 }`**, though everything it asked for was on the client's own shard.

## Root cause

The synchronous path waited for a reply that carries no information the caller needs. The acknowledgement's watermark
covers every earlier one, so a lost or late reply only delays the daemon's release of records until the next.

## Impact

Every synchronous client (the CLI, the MCP server's client, the SDKs' sync calls and `provision_bench`): one call in
every `ack_every` waited for the slowest shard. On an idle daemon whose other shards park, that was a wake on each
of them, and so the single-client provisioning p99 sat at many times its p50 (jupiter: p50 20 µs, p99 80–650 µs). A
shard busy past the reply deadline made an unrelated call fail.

## Edits

`Client::call` sends a due acknowledgement with `begin_ack_if_due`, as the async path does. Its reply is dropped on
arrival and counted (`unawaited_dropped`), and the watermark rule that holds an unpublished id's record back is
unchanged. `Client::acknowledge`, the explicit form, still waits.

## Tests

`a_call_is_never_held_behind_the_periodic_acknowledgement`: red before (`Stalled` after 1 s), green after (1 passed,
3.62 s, macOS arm64). The whole `slates-client` package runs with it.
