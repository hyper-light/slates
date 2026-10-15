# Another region's configuration version superseded the owner's lease

**Found:** 2026-10-07, on the two-network Docker topology (`docs/wip/bench/multiregion/run.sh`), once same-region
forwarding let region 1's reads reach region 0's owner. **Status: fixed (2026-10-07).**

## Description

Region 0's owner, `a1`, refused reads of its own volume with `LeaseUnconfirmed`. The refusal never cleared. Its status
counted `lease.refused.superseded`. At the time, region 1's council was at configuration version 5 and region 0's at
version 4.

## Root cause

The owner lease (§4.8 "Leases and reads") is renewed by its holders' probe answers. Each answer carries the answering
member's configuration version and the standing that configuration gives this node. `fold_events`
(`crates/server/src/member_task.rs`) fed **every** probe answer to `lease.answered`, including answers from members
of another region.

Each region's council keeps its own version counter, so the two counters cannot be compared. Region 1's 5 read as
newer than region 0's 4, and its configuration gives `a1` no standing. The lease therefore held itself superseded by
a configuration that does not govern it, and stayed so: region 1's version only climbs.

## Impact

Once a region's council version passed the owner region's, every owner in that region could refuse its own volumes'
leased reads for good. Any two-region fleet whose councils changed membership a different number of times would hit
this. That is every real one.

## Fix

`fold_events` gives the lease only answers from this node's own region (`fleet::same_region`). The lease's holders
are all in that region, and another region's answer has no standing for this node. The path sample and the probe
counters still take every answer.

## Test

`member_task::tests::another_regions_configuration_version_never_supersedes_the_owner_lease`:

- fold an answer from another region carrying a newer version and no standing, then the same answer from a
  same-region member;
- expect the first to leave the lease alone and the second to supersede it, as before.

It passes. Before the fix, the first answer superseded the lease.

## Siblings checked

`known_version` and `answered` take their version only from `fold_events`. The council's own version comparisons
(`RegionalConfiguration`) are scoped to the region by construction.
