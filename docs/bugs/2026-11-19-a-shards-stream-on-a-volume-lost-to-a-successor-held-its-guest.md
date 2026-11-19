# A Shards stream on a volume lost to a successor held its guest, or served it the stale copy (2026-11-19, A-584)

## Description

When a fleet member loses a volume to a successor's takeover, its NFS mounts answer `NFS3ERR_STALE` and its FUSE mounts
end as revoked ones do (A-346). This applies whether the loss was learned at a holder's answer, at the re-admission's
yield, or from a fence kept across a restart. A Shards stream on that volume applied only the lease gate. So it held
its guest's requests while the lease did not hold, or, once the lease confirmed again, answered them from this node's
copy, which the successor's writes had passed. GAPS had recorded it as an owed sibling of A-346.

Measured on the Mac Studio (2026-11-19), with the test alone on main. A stream whose volume this node had learned it
lost held a SAVE sent by its device unanswered past the test's 60 s wait. The device's read gave up with `EAGAIN`, and
the stream neither answered nor ended.

## Root cause

`turn_with` (`crates/server/src/shards.rs`) asked only whether the volume was gone and whether the lease held
(`verbs::live_tree_fenced`). It never asked `verbs::lost_to_a_successor`, the gate the NFS and FUSE routes read. Nothing
ended a stream when the owner learned its fence (`live_ship::learn_superseded`) or yielded the volume
(`readmission::yield_catalogued`), where each of the volume's FUSE mounts is ended.

## Impact

- A guest over a slates stream on a fleet member that lost the volume waited for good, or read and wrote a stale copy
  whose writes no successor would see.
- Its maps' write-back at an ending would have written into that copy.
- Only fleet members are affected: a single node never loses a volume to a successor.

## Exact edits

- `crates/server/src/shards.rs`:
  - `turn_with` ends a stream whose volume this node lost to a successor (`Turned::Ended`, so `ended` runs) before
    anything else, including an ending's write-back of its guest's maps. A stream not yet ending is counted
    `shards.superseded_ended`.
  - `superseded(state, volume)` is new. It ends each stream of the volume as a revocation does (`end_stream`), counted.
  - The module's notes name the loss among the stream's ends.
  - The lib test `a_stream_whose_volume_this_node_lost_to_a_successor_ends_unanswered` covers two streams, one whose
    loss is told and one over a fence kept untold. Each SAVE must meet the stream's end, both records must be gone, and
    two ends must be counted. Its helpers (`Device`, `attached_device`, `Met`, `save_met`) are shared with A-516's test.
- `crates/server/src/live_ship.rs`: `learn_superseded` and `inject_superseded` call `shards::superseded` beside
  `fuse::superseded`, on every platform, since streams are served on every platform. Their notes say so.
- `crates/server/src/readmission.rs`: the yield ends the volume's streams too.
- `crates/server/src/daemon.rs`: `Daemon::inject_superseded`'s notes name the streams.

## Evidence

- **Red**, with the test alone on main: the told stream's SAVE was neither answered nor ended within 60 s
  (120.76 s for the run).
- **Green**, with the change: 0.79 s. `cargo test --locked -p slates-server --lib -- shards:: live_ship::
  readmission::` passed 31 of 31, and `cargo fmt` was clean.
- Both runs were on the Mac Studio, 2026-11-19.

## Siblings

- **Still owed:** the guest device (`crate::virtiofs`) and the WinFsp mount (`crate::winfsp`) apply only the lease gate.
- **Still owed:** a relinquish (A-362) that runs before the stream's next turn still does not wait for its end.
