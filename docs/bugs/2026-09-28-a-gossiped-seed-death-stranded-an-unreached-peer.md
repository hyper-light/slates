# A gossiped seed death stranded a peer this node had not reached

Date: 2026-09-28. Scope: fleet formation (`crates/server/src/fleet.rs`, the direct-contact rule shared by
the probe task, the record link and formation). Found by CI run 36462066595 (`fd4f0ef`, the KIND lane:
"the fleet formed at five did not happen within 180s"); reproduced locally in the KIND lane.

## Symptom

On a fresh install, one pair of nodes never linked. In the CI run, `slates-2` saw two members while the
other four saw four; `slates-0`, `slates-1`, `slates-3` and `slates-4` had counted NXDOMAIN answers
resolving `slates-2`'s name. The split was permanent: in the local reproduction the pods were still split
more than four minutes later.

Local reproduction, `cargo xtask kind scale --keep` in a loop on this machine: 2 of 5 runs failed before
the fix (the three-replica fresh install that follows the five-replica one), and 1 of 3 in a second loop
with the transition logging below.

## Evidence

The status summary could not say why a probe task stopped, and only the first refusal of each kind was
logged. Logging was added for each probe task's idle and resume transitions, with the reason from the
direct-contact rule, and for each change of a peer's member id (one line per transition, never per
period). The failed run then showed, on `slates-0`, `slates-2` and `slates-3`:

```
fleet: resolving `slates-1.slates.slates.svc.cluster.local`: the nameserver answered rcode 3
fleet: probe of HostId(7808796613653679892) idles: Some(BelievedDead)
```

and on `slates-4`:

```
fleet: peer anchor HostId(4173061545102521122) is now member HostId(16925178712613950328) (was HostId(7808796613653679892))
```

`7808796613653679892` is `slates-1`'s manifest **seed** id (`member_id(anchor, 0)`), not a live
incarnation. `slates-1` in turn logged `idles: Some(BelievedDead)` for the seeds of `slates-0`, `slates-2`
and `slates-3`.

## Root cause

1. At boot every node knows each peer only by its seed. `slates-0`, `slates-2` and `slates-3` first got
   NXDOMAIN for `slates-1` (its DNS record was not yet published), so their first dials failed.
2. `slates-4` reached `slates-1` and learned its real id. `learn_member` folds the seed dead, correctly:
   the placeholder must not count as a live member.
3. That death gossiped to the others. Gossip deliberately never enrolls a stranger's id, so they learned
   only that the seed was dead.
4. Their probe tasks for `slates-1` were still keyed on the seed. The direct-contact rule said
   `BelievedDead`, so each task idled and never dialed again, and never learned the real id.
5. `slates-1` had the same view of the others, so neither side ever dialed the other.

The record link already stated the rule this broke (`refresh_record_identity_in`: "the manifest's seed id
was never a live incarnation"), but the direct-contact rule treated a seed's death like a real member's.

## Impact

A fleet could form permanently partitioned whenever a node's first dials to a peer failed (DNS not yet
published, a pod not yet listening) while a third node reached that peer. The partitioned pair never
meshed, so formation, consensus voter contact and record placement between them never completed.

## Fix

The direct-contact rule (`direct_contact`, which `keeps_direct_contact_with` reads for the probe task,
the record link and formation) gains a kept reason, `UnlearnedSeed`: a peer believed dead only as a
manifest seed whose anchor this node has not learned a real id for. Contact is kept, so the node keeps
dialing (one dial per period, as during formation) until it learns the real id on contact. Once the real
id is learned, the seed is a retired placeholder, and a death of the real id drops contact as before.

Test first: `fleet::tests::a_gossiped_seed_death_keeps_contact_until_the_real_id_is_learned` failed
before the change ("a peer known only by its seed keeps contact when the seed's death is gossiped") and
passes after. By use: the KIND scale step, looped, below.

## Sibling sweep

- The record link and formation read the same rule, so they share the fix.
- `learn_member` still folds the seed dead on learning the real id (needed so the placeholder never
  counts as alive).
- `refresh_record_identity_in` already treated the seed as a placeholder.

## Verification

- Unit: the new test passes; the server's library tests and the in-process fleet suite pass (below).
- KIND, this machine, `cargo xtask kind scale --keep` looped with the fixed image: **10 of 10 passed**
  (2026-09-28), against 2 of 5 and 1 of 3 failing in the two loops before the fix.
- The fleet test that asserted the superseded rule for this exact scenario (a gossiped seed death dropping
  the pending dial) now asserts the corrected one: the dial is kept through the seed's death (held 2 s,
  twenty probe periods), and B's return at new addresses still meshes — in 0.03 s in each of three runs, so
  keeping the dial costs nothing measurable (B's own dial reaches A, which learns B on its serve side).
- Full in-process fleet suite: 3 of 3 runs passed (50 of 50 tests each).
- The transition logging stays: it logs one line per probe idle or resume (with its reason) and per member
  id change, never per period, and it is what named this cause.
