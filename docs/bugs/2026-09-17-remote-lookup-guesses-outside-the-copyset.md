# Remote lookup chooses a node outside the object's copyset

Date: 2026-09-17. Design: §4.8 Lookup and takeover, §4.9 exactly-once, AC-8.14.

## Reproduction and cause

The five-daemon Linux history
`a_cross_region_client_finds_the_copyset_successor_instead_of_an_unrelated_live_peer`
has four home-region members at f=1 and one foreign client. It selects a real volume whose
two surviving candidate holders exclude the first-ranked member of the entire live region.
Both candidates hold the sealed head before the original owner stops. The successor recovers,
places the head and serves Status locally. The foreign client must then read and snapshot it,
and retry that snapshot with the same request id.

Before the fix, the bounded invocation failed in 42.83 s. The local successor was
`4224536857973966081`; all-member ranking selected `4275317117897721336`. The foreign
client received `NotFound` throughout its 30 s observation window. The record was available;
the request was routed to a node without it.

`verbs::owner_in_region` ranked all live members of the home region. Its claim that this
matched takeover was wrong: takeover ranks the surviving candidates remembered for that
record. Two unit tests encoded the same incorrect assumption. Fresh membership cannot
reconstruct a historic copyset, so recomputing another ranking would retain the defect.

## Fix

The replay check exposed a second fault in the same forwarding path. `forward_to_owner`
delivered replies with `recorded=false`, so `retry_deferred` recorded every forwarded reply
in the origin's partition. A transient `HomedElsewhere` therefore completed the request
permanently there, and even a successful retry never reached the owner's RIFL window. The
strengthened existing cross-region write history failed in 2.54 s: the direct-forward counter
remained 2 instead of advancing to 3. Forwarded replies must use the existing externally
recorded delivery path; successful write completions belong to the executing owner.

- Replace the all-member guess with a bounded read-only location query over authenticated
  home-region sessions. Replies bind the object, home and root version, and use each peer's
  actual held-object routing view. Only the current local owner claims to serve the object.
- Collect replies under the existing liveness budget, return every borrowed session and
  refuse contradictory ownership claims at the newest observed regional generation.
- Retain one successful route per live client, bounded by the existing client arena. Invalidate
  on a home/root change, peer retirement or a failed forward. Keep the creator fast path.
- Forward the actual verb once, preserving its original completion key. Discovery never
  submits a write. The origin never records a forwarded reply as a new local completion;
  in particular a temporary routing refusal cannot poison a retry. A routing hint does not
  grant write or read authority.
- Replace the two incorrect unit tests, add hostile/binding/conflict coverage, and retain the
  live regression with its non-vacuous forwarded-write retry.

## Validation

Red command (2026-09-17): the supervised `linux-fleet-run.py` runner invoked
`/quantum-test a_cross_region_client_finds_the_copyset_successor_instead_of_an_unrelated_live_peer
--exact --nocapture --test-threads=1` in the cached Rust 1.98 Linux image, with four CPUs,
2 GiB, no external network, uid 1000 and a 90 s supervisor. Docker used an aarch64 Linux
6.12.76 VM on the M5 Max host. Trace: `lookup-before.log` in the session scratch directory.

Green results will be recorded after implementation. This location repair does not establish
the separate lease or arbitrary configuration state-transfer obligations.
