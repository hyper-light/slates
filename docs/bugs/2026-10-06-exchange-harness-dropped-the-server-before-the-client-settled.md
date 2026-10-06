# The exchange harness dropped the server before the client settled

**Found:** 2026-10-06, by the session-plane ready-set change (`docs/wip/BENCHMARKS.md`, "Session-plane ready
sets"). Its different service order changed the timing.

## Description

`concurrent_exchanges_past_the_stream_limit_survive_every_hostile_path_and_leak_nothing` failed on every
run at 5 % random loss, seed 1:
- `client_settled: false`, `client_settle_cut_off: true`.
- All 80 exchanges completed, and the server settled holding nothing.
- The client still held 3 send streams, with 3 packets in flight and unacknowledged.

A logged probe-timeout trace showed the client probing 185 times with no acknowledgement ever arriving.

## Root cause

The harness's server (`serve` in `crates/transport/tests/exchanges.rs`) settles when the client says it is
done, drives to quiescence, returns its report, and drops its endpoint. The client settles only after it has
the report. When the acknowledgement of the client's last request frames was lost, nothing was left to
answer the client's probes. The server held the data (it had replied), but its session was gone. The
harness's own comment assumed the client "is still lingering, so it is acknowledged", which covers the
server's last frames, not the client's. HEAD's timing happened to avoid the lost acknowledgement.

The transport itself was correct: against a peer that has left, a sender's unacknowledged data is never
acknowledged. Real sessions end it through their idle timeout or close.

## Impact

Tests only: a harness race that could fail a healthy run whenever the last acknowledgement was lost. No
production code involved.

## Fix

The server sends its report as soon as it is quiescent, then keeps answering until the client says it has
settled (a second channel), bounded by the run bound. The die-after mode still drops its session, so the
dead-peer path stays covered. All 5 exchange scenarios pass, with the ready-set scheduler and with HEAD's.

## Sibling sweep

Every test that calls `settle()` was read (2026-10-06).
- `crates/transport/tests/session.rs`: the server settles and leaves, but the client never settles after it.
  Its exchange ends with the reply, so nothing waits on the departed peer.
- `crates/cluster/tests/{commit,fleet_live,raft_live,config_group_live,root_group_live,ledger_promote}.rs`:
  only the serving side settles, while the requester is alive and waiting for its result. No requester
  settles after its server leaves.
- `crates/cluster/tests/{content,extend}.rs`: bounded settles on the serving side, the same shape.

No sibling has the race.
