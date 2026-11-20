# Linux NFS loopback connections were dropped at every daemon restart

Date: 2026-11-18. Area: `crates/server/src/nfs.rs` (the loopback listener's holds and adoption), `crates/rt/src/tcp.rs`
(`make_sends_whole`), the anchor's hold channel; for this change's part `crates/server/src/held_send.rs`. Design: §4.6
(the loopback listener), A-113 (a connection outlives its daemon), A-128; amendment A-550. Goal 85 (B12). Recorded open
in GAPS since A-113 (2026-10-06).

## Description

On macOS the anchor holds each NFS loopback connection across a daemon restart, so a restart is a slow reply and never
a dropped connection (A-113). On Linux no connection is held: every daemon restart closes every NFS client's
connection, and each client reconnects, with whatever its kernel does about requests in flight. A-113 recorded the
Linux half as owed.

## Root cause

Holding a connection is safe only when no reply can be torn on the wire: a successor answers every request still queued,
and a reply sent part-way would put the next reply's bytes after it. macOS makes every send all or nothing
(`SO_SNDLOWAT` at a record's size). Linux keeps `SO_SNDLOWAT` fixed and copies what fits, so `make_sends_whole` answers
`NotOffered` there and the connection is not held. Nothing could tell a successor whether the stream it inherited ends on
a record's boundary.

## Impact

Linux kernel NFS clients of the loopback export (the Linux NFS mounts in the tests, and any host that mounts a volume
over NFS rather than FUSE) saw every restart as a dropped connection. The network export (TLS) is not held on any
platform, by design.

## Exact edits

Part 1 (A-550, this change): `crates/server/src/held_send.rs`, the adoption's judgment: the bytes the kernel says the
writer handed it against the ledger of the send in flight (`resume`: `Untouched`, `Whole` or `Torn`), with its unit test.

Part 2 (A-550, 2026-11-19):
- `crates/rt/src/netsys.rs`, `crates/rt/src/tcp.rs`: `TcpStream::bytes_handed`, Linux's `tcpi_bytes_acked` plus
  `SIOCOUTQ`, settled against acknowledgements arriving between the readings (`settle_handed`); none elsewhere. No SYN
  is counted on an accepted connection (the kernel source, cited in the design's A-550, and a check at admission).
- `crates/server/src/held_send.rs`: the ledger, a `memfd` the anchor holds as a copy; a slot per held connection with
  two records and a selector, each update made as an explicit list of stores; the record (`NFSLEDG\x01`); the start's
  handover (judge by the newest inherited ledger, fill a fresh one, hold it, let the old go, then serve).
- `crates/server/src/nfs.rs`: `admit` holds a Linux connection under the ledger; `send_ledgered` is the only writer of
  a connection's stream; `adopt_held` judges and ends a torn connection before it is read; the slot moves with a
  migrated connection. `crates/server/src/daemon.rs`, `crates/server/src/config.rs` (the copy bound + 2 on Linux),
  `unsafe-budget.toml` (the `SIOCOUTQ` ioctl).
- Owed, main's: the Linux kill run of `crates/cli/tests/nfs_held.rs`.

## Tests

`a_held_connection_is_served_on_only_on_a_records_boundary`: red with today's rule (no connection served on) and green
with the judgment, on the Mac Studio (d1323898, debug). Command: `cargo test -p slates-server --lib held_send`.

Part 2, every host (`cargo test -p slates-server --lib held_send`, `cargo test -p slates-rt --lib handed_tests`): the
crash oracle `a_successor_judges_a_writer_stopped_after_any_store_rightly`, the slot, record, copy id, newest-ledger and
adoption tests, and the count's settling oracle over every schedule of acknowledgements. Linux only (`cargo test -p
slates-rt --lib netsys`, `cargo test -p slates-server --lib held_send` on adacachy, in a capped unit):
`an_accepted_connection_counts_every_byte_written_and_no_syn` and `a_ledger_object_outlives_its_writer`.
