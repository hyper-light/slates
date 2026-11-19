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

Part 2 (owed, Linux-only, main's to write and compile): the ledger in anchor-owned RAM, written before each send and
advanced after it, each record checksummed; the kernel's count at adoption (`tcpi_bytes_acked`, measured for whether it
counts the SYN, plus `SIOCOUTQ`); Linux connections held once both exist, a `Torn` one ended at adoption before it is
read; then the Linux kill run of `crates/cli/tests/nfs_held.rs`.

## Tests

`a_held_connection_is_served_on_only_on_a_records_boundary`: red with today's rule (no connection served on) and green
with the judgment, on the Mac Studio (d1323898, debug). Command: `cargo test -p slates-server --lib held_send`.
