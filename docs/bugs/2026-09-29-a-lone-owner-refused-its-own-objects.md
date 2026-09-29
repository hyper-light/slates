# A lone owner refused its own objects

Date: 2026-09-29. Scope: the owner lease's rule for how many confirmations it needs (`OwnerLease::holds`,
`crates/server/src/lease.rs`; §4.8 "Leases and reads", AUD-08). Found by CI run 36567187754 on `2c6c034`
(Ubuntu), and by a local full-suite run the same day under heavy load.

## Symptom

Two cross-region forwarding tests failed, each once:
- `a_client_reads_a_cross_region_volume_by_forwarding_to_its_owner` on CI;
- `a_forward_waits_for_the_owners_session_while_it_is_out` locally, while another session's TLC model check
  held about 11 cores.

In both, a client on b (region 1) asked for the status of a volume homed on a (region 0), and b's forward to a
was never served within 60 s. Neither test recorded what b answered. Each passed 5 of 5 when run alone.

## Evidence

Both tests build three regions of one node each, at `f = 1`. A temporary experiment on that fleet (not kept)
sampled owner a every 0.5 s for 10 s after formation. For its own volume it recorded a's lease, a's answer to
a status, and b's forwarded answer:

```
t=0.00s a.lease_holds=Ok(true)  a.status=served  b.status=served
t=0.51s a.lease_holds=Ok(false) a.status=Refused { refusal: LeaseUnconfirmed { version: 0 } } b.status=Refused { … same … }
… every later sample the same, to t=9.61s
```

Every node's regional council had one voter and one member, itself, at configuration version 0.

## Root cause

The lease demanded `f` fresh confirmations from the object's other candidates, whatever their number. The
rule comes from the candidate floor: `2f + 1` candidates, so the owner has `2f` others. A successor adopts
only on `f + 1` promises from those others, and any `f + 1` of `2f` intersect any `f`.

In a region with fewer than `2f + 1` members, an object has fewer other candidates. A lone node at `f = 1`
has none, so the lease held only within the startup allowance after a configuration install (one membership
horizon, 0.9 s), and never again. From then on the node refused every latest-state read of its own
objects, including forwarded ones.

No successor could ever adopt those objects: a takeover needs `f + 1` promises from other candidates, and
there were none. So the refusal protected nothing. The forwarding tests passed only when their first
forwarded read landed inside that first window, which formation under load could miss.

## Impact

- In any region smaller than the candidate floor (fewer than `2f + 1` members), the owner lease needed more
  confirmations than the safety argument does. With `f` or fewer other candidates, it could never hold
  after the startup allowance.
- Affected: a single node per region at `f = 1`, and two nodes per region at `f = 1`.
- Every latest-state read was refused `LeaseUnconfirmed` (`NFS3ERR_JUKEBOX` at the mount): a head read,
  versions, status, a since-a-version query, and the mounted tree.

No stale read was possible either before or after the fix.

## Fix

The number needed is the intersection bound itself (`confirmations_needed`): `others − f`, saturating at
zero. With that many fresh confirmations, any `f + 1` of the others include a confirmer, and that confirmer's
promotion gate refuses to promise until the lease can have lapsed.

| Other candidates | Confirmations needed |
|---|---|
| `2f` (the floor) | `f`, as before |
| between `f` and `2f` | fewer than `f` |
| `f` or fewer | none, since `f + 1` promises cannot be gathered |
| none (the laptop, `f = 0`) | none, by the same arithmetic (R8) |

The rest of the lease is unchanged: the version match, the bound, supersession and the startup allowance.

## Tests

**Failing first:**
- `below_the_candidate_floor_the_intersection_needs_fewer_confirmations` (`crates/server/src/lease.rs`, unit):
  - alone at `f = 1`, and with one other candidate at `f = 1`, the lease holds with no confirmation;
  - at the `f = 1` floor it still needs one;
  - with three others at `f = 2` it needs one, and at the `f = 2` floor two.
  Unfixed, it failed at its first assertion.
- `a_lone_owner_in_its_region_serves_its_latest_state_past_the_startup_allowance` (`crates/server/tests/
  fleet.rs`, by use): the same three-region fleet. Once b has forwarded one status, a is asked directly and
  b through its forward, every poll interval for three membership horizons, and every answer must be
  served. Unfixed, both refused `LeaseUnconfirmed { version: 0 }` from 0.14 s into the span; fixed, all are
  served.

The two forwarding tests now record b's last reply, so a failed wait names what b answered.

## Siblings

- The lease's other callers (the verb gate, the mount gate, the synthetic root) read `holds` and change with
  it.
- The existing unit tests all sit at the floor and pass unchanged.
- The promotion side (`AnswersGiven::promotion_open`, `collect_promises` at `f + 1`) is unchanged; the fix
  rests on it.
