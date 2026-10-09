# Lease-renewal answers marked every pair a pool misfit, and a dead member was never suspected

**Found:** 2026-10-09, by the fleet suite after A-125 (`a_warm_fleet_restart_recovers_its_root_and_regional_quorums`).
**Status: fixed** (same day, in vendored hyper-swim, pending upstream).

## Description

After a warm restart of a three-member fleet, the test stops the root's leader and waits for the two survivors to
retire it. In 3 of 11 runs with A-125's period floor, they never did: 30 s later both still held the stopped member
alive, its council death watch at 0, and no retirement was proposed. Without the floor (`48496cf8`) it passed 8 of 8.

A new observation, `Daemon::council_death_view`, showed the leader's detector held the stopped member alive with:
- verdict `None`;
- its pair estimator unconfigured;
- no suspicion;
- its last answer 30.06 s before the dump.

The other survivor's pair was configured and judged. The leader's pool had no verdict either. A pair with neither a
verdict of its own nor the pool's is probed by measurement probes only, which judge nothing.

## Root cause

`Peer::answered_unrecorded` (vendored hyper-swim `detector.rs`) marked a pair `pool_misfit`, permanently, for any
answer whose nonce matched no held probe record, whenever the pair had no verdict of its own. Its documentation
names only answers to records later probes reused, and its own helper `reused_round_trip` returns `None` for a nonce
"which no reuse explains". The flag was set anyway.

slates' membership plane hands every acknowledgement to `Detector::on_ack`, including the answers to its own
lease-renewal pings (`lease_renewal`, nonces from 2⁶³ up, disjoint from the detector's by design). So every pair that
answered a renewal before configuring its own estimator was marked a misfit. A misfit pair never feeds the pool, so
the pool was never configured. A misfit's provisional verdict comes from the pool, so it had none. A member that died
in that state was measured for ever and never suspected.

The bug predates A-125. A-125 exposed it: with the period floor the detector probes a lease holder less often than
once per renewal interval, so renewals rose from 1 to 34 per node in the two-node test. Before it, the detector's
RTT-paced probes renewed every holder and almost no renewal was sent.

## Impact

- Detection: a member that died while its pair was unconfigured and the pool unfed was never suspected, so never
  retired, and its seat was never taken over. The window opens at every daemon start, warm restarts included.
- Expired relay records had the same effect: their answers fall through to `on_ack` with nonces from the top of the
  range.

## Exact edits

- `vendor/hyper-raft/hyper-swim/src/detector.rs` `Peer::answered_unrecorded`: mark a misfit, and take the reused
  record's round trip as the pair's path, only when a reused record explains the answer (`reused_round_trip` is
  `Some`). Any other unrecorded answer is evidence of life only, which `on_ack` has already recorded.
- Test (hyper-swim): `an_answer_no_probe_explains_marks_no_misfit_and_a_silent_peer_is_suspected`. Foreign-nonce
  answers to a fresh member's first probes must mark no misfit, the pool must configure, and a peer silenced afterwards
  must be suspected. It failed before the fix (peer A marked a misfit) and passes after.
- Diagnosis: `Daemon::council_death_view` and `MemberPlane::diagnose`. The warm-restart test's dump prints, for each
  council member, the fleet's belief, the detector's belief, verdict, report and wake lateness, and the death watch's
  count.
