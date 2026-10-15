# An owner's lease lapsed while far members stretched the probe round

**Found:** 2026-10-07, running two regions on two separate Docker networks joined by a router container shaping 100 ms
± 40 ms one way and 3 % loss each way (`docs/wip/bench/multiregion/run.sh`, condition 10).

## Description

Six daemons, three per region, from one manifest. A volume was created on `a1` (region 0) and 8 MiB written there.
Reads were refused:

- every read from region 1, forwarded to `a1`, answered `LeaseUnconfirmed { version: 4 }` after 0.3–4.6 s;
- `a1`'s own reads of the same volume parked on the lease and were served late (one waited 665 ms);
- `a1` counted `lease.refused.unconfirmed` once per forwarded read.

## Root cause

A temporary diagnostic at every unconfirmed verdict printed the cohort and each confirmation's age. `a1`'s cohort
held itself and its two region-0 holders, both alive and unshaped. Their latest answers were 966 and 1,362 ms old;
the next two samples were 1,369/1,766 and 1,035/1,433 ms. The lease bound is 900 ms: three 100 ms coordinator periods,
each dilated three times (`lease::horizon_ns`), less the clock tolerance.

The answers that confirm a lease came only from the failure detector's probes, and the detector probes one member a
period, in rotation over every member it knows (SWIM §3.1), each period as long as its probe needs. In a six-member
fleet, three members sit across the router. Their periods last a 200–300 ms round trip, or longer when the relays
are asked, so a round took over a second, and a near holder went unprobed past the bound on most rounds.

A forwarded read cannot park on the lease (it carries no client slot to park), so it was refused at once. A local
read parked until the next answer.

The simulated plane reproduces it without a router: an owner, one near holder and four members 100 ms away. The owner
went 9,886 ms without a fresh answer from its holder in a 30 s run (92 answers in all).

## Impact

An owner in any fleet whose members are not all near refuses its own objects' latest state for part of every probe
round, with every member alive. Cross-region reads, which reach the owner as forwards, were refused outright. A fleet
on one LAN never showed it: there, a period is a millisecond, and a round fits the bound many times over.

## Fix

The owner renews its lease at the lease's own cadence (Gray and Cheriton, "Leases", SOSP 1989; Chubby's KeepAlive;
Raft's leader lease on its per-period heartbeats).

- `crates/cluster/src/lease_renewal.rs` (new, pure):
  - Each step, the plane probes every holder it has not probed within the renewal interval; the detector's own
    probe of a holder counts.
  - Renewal nonces start at 2⁶³. The detector numbers its probes from zero, so the two ranges are disjoint. A
    detector nonce that ever reaches the range stops renewals, counted.
  - Each holder keeps only the renewals sent within one bound, since an older one's answer confirms nothing.
  - An answer is credited with its own renewal's send time, and only once.
- `crates/cluster/src/member_plane.rs`:
  - `Fleet::lease_holders` names the holders: the owner's neighbourhood, settled and current, from
    `Configuration::lease_holders_into`.
  - The plane's wake includes the next renewal.
  - A renewal is a `Ping` with no gossip. Its acknowledgement becomes the same `PlaneEvent::Acked` the detector's
    does, so the holder records its answer for the promotion gate exactly as before.
  - Holders are filtered by the plane's joined peers, not by the detector's view. A first cut filtered by the
    detector's view, and the test still failed: the detector had dropped the near holder after other members
    falsely condemned it (`2026-10-07-a-far-member-condemns-the-near-side-by-its-pooled-deadline.md`).
- `crates/server/src/member_task.rs`: the renewal interval is one coordinator period (`HEARTBEAT_NS`), the unit
  the bound is stated in. Eight renewals in a row can be lost before a live, reachable owner's lease lapses.
- Counters `renewals_sent`, `renewals_answered` and `renewals_exhausted` sit on the plane.

Safety is unchanged. A renewal is the same probe: the holder records when it answered, which gates its promotion
of a departed owner, and the owner counts the answer from its send.

## Tests

- `slates-cluster` `tests/member_plane.rs`: `an_owner_is_answered_by_its_holder_within_the_lease_bound_while_far_members_stretch_the_round`.
  - It fails before the fix: the longest gap was 9,886 ms.
  - After the fix, 10 of 10 runs passed, each with its longest gap at the 100 ms interval and every renewal sent
    answered (non-vacuity: `renewals_answered > 0`).
- `lease_renewal` unit tests:
  - the cadence, and the detector's probe postponing a renewal;
  - an answer credited with its own send, once;
  - only one bound's worth of renewals kept;
  - the nonce-range guard;
  - a holder no longer named is forgotten.
- On the two Docker networks, the unconfirmed-verdict diagnostic fired at every forwarded read before the fix and
  never after (a0–b2, three rounds of reads).

## Siblings found, not fixed here

- **False deaths across a far link.** A far member condemns live near members three or four times a second,
  by a deadline its pooled estimator took from its own near round trips. The defect is in the vendored detector
  (`2026-10-07-a-far-member-condemns-the-near-side-by-its-pooled-deadline.md`).
- **Cross-region reads still refused.** After this fix, region 1's reads were refused `HomedElsewhere`: the
  location round found no session to region 0's hosts (`fleet.owner_location.no_session`, `.unavailable`).
- **Same-region non-owners answer `NotFound`.** The design routes a lookup by id to its creator host; the code
  forwards only volumes homed in another region (`docs/wip/GAPS.md`).
