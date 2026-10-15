# A warm restart dropped held register records

**Found:** 2026-10-07, while designing the mirror region's adoption (`docs/wip/mirroring.md` M4). The design needs
mirror holders' records to survive a restart, and on checking how home holders keep theirs, nothing did.

## Description

A holder acknowledges an owner's register record once its `Acceptor` for the object has accepted it
(`fleet::accept_held_record`). A takeover's phase one then counts the holder's report and raises its fence to the
new epoch (`takeover::serve_host_prepare`).

Neither the records nor the fence was ever written to anchor-owned RAM. `Acceptor::recovered` and
`Acceptor::persisted` existed for exactly this, but nothing called them: every rebuilt `ShardState` began
`holder_records` empty. Amendment A-51 (2026-09-30) stated the opposite ("its register records survived the same
restart in anchor-owned RAM"), and no test checked it.

A warm restart keeps the node's member id: the retained consensus record supplies the same boot nonce
(`daemon.rs`, `incarnation`). So a restarted holder came back as the same member holding none of the records it had
acknowledged, and with its fences down.

## Impact (safety)

- **A committed head could be lost.** A record committed at `f + 1` holders, one of which restarted, could be
  missed by a takeover: the restarted holder's empty promise counts toward the `f + 1` promises the successor
  needs, and the successor adopts an older record. This breaks Vertical Paxos II's acceptor durability, which the
  design's takeover rests on, and Raft's Figure 2 rule that persistent state is updated before responding.
- **A replaced owner could write again.** A holder's raised fence was lost on restart, so it could accept a lower
  epoch than it had promised. That one is narrower in practice, since a retired owner's records are refused by
  membership.

## Fix

- **The image.** The shard's held image carries the content hold and every held register: owner, generation, the
  epoch its fence has seen, and its accepted positions (`content_holder::{held_image, split_held,
  restore_held_registers}`). It rides the same double-buffered publish as the content hold (AUD-29-59). An image
  that does not decode is refused and logged, never read as nothing held.
- **Persistence before reply, for records.** `hold_checked_record` publishes the shard before acknowledging, and
  counts a refused publish (`fleet.record.unpublished`) with no acknowledgement.
- **Persistence before reply, for promises.** `serve_host_prepare` publishes before promising
  (`takeover.promise_unpublished`).
- **Recovery** rebuilds each register with `Acceptor::recovered` under the restarted member id.
- **No compatibility path.** An image in the old format is refused, not decoded twice (banned item 7).

## Test

`slates-server` `tests/recovery.rs` `an_acknowledged_held_record_survives_a_warm_daemon_restart`:

- **Before:** the record was held before the restart (sequence 4, epoch 2, fence 2) and `None` after, under the
  same member id.
- **After:** the record is held, value and fence included. The test also asserts the member id is kept.

Suites after the fix: recovery 28/28, daemon 21/21, fleet 73/73, server library 162/162.
