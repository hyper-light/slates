# The owner lease was voided by other hosts' configuration changes

Date: 2026-09-29. Scope:
- the owner lease's evidence and supersession (`OwnerLease::answered`, `OwnerLease::verdict`,
  `crates/server/src/lease.rs`);
- the SWIM acknowledgement that carries it (`SwimMessage::Ack`, `crates/cluster/src/swim.rs`);
- the cohorts it counts over (`Configuration::lease_cohorts`, `Configuration::standing`,
  `crates/db/src/register.rs`);
- which peers an owner keeps in direct contact (`direct_contact`, `crates/server/src/fleet.rs`).

§4.8 "Leases and reads", "Neighbourhood changes", "Authority scope"; AUD-08. Found by CI run 36603261909 and
confirmed by a Linux io_uring loop.

## Symptom

CI run 36603261909 (`265c1eb`) failed twice on owner-lease refusals:
- Ubuntu: `a_takeover_successor_serves_the_dead_owners_content_over_nfs`. The successor's `LOOKUP` of the
  file it had just adopted answered `NFS3ERR_JUKEBOX`, right after its `status` had answered.
- macOS: the three-process CLI test's `slates mount` on the owner was refused
  `LeaseUnconfirmed { version: 3 }`.

A Linux loop of the NFS test and the location test (`rust:1.98.0`, `--security-opt seccomp=unconfined`, 17
rounds) failed the NFS test 5 times and the location test 4 times.

## Evidence

The loop's build counted each lease refusal by its reason (`lease.refused.superseded` or
`lease.refused.unconfirmed`), and the NFS test printed the refusing node's counts on failure. All four failures
that printed them show one reason:

```
fail-4:  LOOKUP hello.txt answered status 10008 on …  "lease.refused.superseded": 1
fail-6:  GETATTR of the root answered status 10008 on …  "lease.refused.superseded": 1
fail-16: GETATTR of the root answered status 10008 on …  "lease.refused.superseded": 1
fail-17: LOOKUP hello.txt answered status 10008 on …  "lease.refused.superseded": 1
```

(The fifth, fail-7, failed at formation, unrelated.) One location failure reached the successor and was refused
`LeaseUnconfirmed { version: 11 }`: eleven configuration versions for a five-node fleet with one takeover.

## Root cause

**The lease was keyed to the regional version.** An owner counted a holder's answer only under the exact
configuration version it had installed. It was superseded, refusing every latest-state read even inside its
startup allowance, as soon as any holder answered under a newer version. The version advances on every
change in the region:
- every admission and retirement;
- since `84cc9c1`, every owner's `Settle`;
- since `265c1eb`, every takeover's `Confirm`.

After a takeover each survivor settles its new neighbourhood and the successor confirms its share, so the
version moves several times within a few periods. Each time, whichever node installed it first answered its
peers' probes under it, and every peer that had not installed it yet refused its own clients until it did. None
of those changes touched the refusing owner's authority. At scale, where some host joins, leaves or settles
nearly every period, owners would be refusing most of the time.

The design names what the evidence must belong to: "the relevant authority generation". For an owner that is
the neighbourhood a takeover of it recovers through, its **settled** neighbourhood. That changes only:
- at the owner's admission;
- at its own `Settle`;
- at its retirement.

**Found by the analysis — the lease counted the settled cohort only, while its writes were joint.**
- A takeover recovers through the owner's settled neighbourhood as the council holds it.
- The owner's report moves that from its settled neighbourhood to its current one.
- The owner installs that move a moment later, or not at all if it is cut off.

Suppose an owner is cut off together with holders that had not learned of the settlement either. It kept its
lease on their answers, while a successor recovered through the new cohort, whose members it never asked. The
old version check caught this only if one of those holders happened to have installed a newer version, and a
holder cut off with the owner had not.

**Found with it — settled-neighbourhood hosts were dropped from direct contact.** `direct_contact` kept a
peer as a neighbour only in the owner's current neighbourhood. A host that a change moved out of it, and that
was no voter, lost its record session and its probes, although the owner's joint writes and its settled-cohort
lease both needed it.

## Impact

Every owner refused latest-state reads, writes and mounts (`LeaseUnconfirmed`, `NFS3ERR_JUKEBOX`) for a
moment at every configuration change anywhere in its region. This was the cause of CI's two red lanes on
`265c1eb`. The joint-lease gap was a safety gap, possible only under a partition that cuts an owner off with
lagging holders just as its settlement commits. No run observed it.

## Fix

- **The answer carries the prober's standing.** The acknowledgement names the version its configuration
  fixed the prober's settled neighbourhood at, or `None` when the prober is no member, beside the version it
  read that at.
- **The owner's standing is two versions**, its settled and its current neighbourhood's (`Standing`).
  - An answer it recognizes confirms it.
  - An answer showing its own authority changed supersedes it: no membership under a newer configuration (its
    retirement), or a settled neighbourhood newer than its current one (its id admitted again).
  - An older answer is no evidence either way.
- **The lease is joint while a change is in flight**, as the writes are: the intersection bound in the settled
  cohort and in the current one.
- **A retired candidate is not waited for**: it cannot promise. The bound counts the live candidates against
  the whole cohort's recovery quorum.
- **Direct contact covers the settled neighbourhood too** (`Configuration::neighbours`).

## Tests

- `only_a_change_to_the_owners_own_authority_supersedes_it`.
- `a_change_in_flight_needs_confirmations_in_both_cohorts`.
- `a_retired_candidate_is_not_waited_for`.
- The exhaustive intersection oracle, now over every set of live candidates.
- `an_owner_keeps_direct_contact_with_its_settled_neighbourhood_while_a_change_is_in_flight` (red before:
  the displaced host was dropped).
- `a_hostile_standing_is_refused` (wire).
- The NFS takeover test and the location test, looped in Linux under io_uring.
