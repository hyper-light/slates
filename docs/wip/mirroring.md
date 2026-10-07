# Mirroring across regions: the implementation design

Status: design, 2026-10-07. Realizes §4.10 "Mirroring across regions" and §4.8 "Mirroring", Phase 8 item 10,
AC-8.15 and T-8.13. None of it is built yet (`docs/wip/GAPS.md` 2026-10-07): `Placement::mirror_acked` is never
filled, `RegionalConfiguration::has_mirror` is never set, and `promote-region` re-homes routing without moving any
copy. Each piece below lands with its own tests and a status line here.

## What the design asks

- **Shipping.** Every committed record and its content goes to the owner's neighbourhood in the mirror region,
  asynchronously, in epoch and sequence order, by the same put machinery. It is acknowledged at `f + 1` there.
- **Lag.** `mirror_age` is measured on the home clock from the oldest home-committed record still lacking a mirror
  acknowledgement, and is zero when caught up. Unknown is reported as unknown, never as zero.
- **Waiting.** `await placed(mirror)` returns once the named snapshot's record and content are acknowledged in the
  mirror. An unreachable mirror is Degraded: `mirror_age` grows, and the wait refuses `NotPlaced { mirror }` at its
  deadline.
- **Promotion.** Region loss promotes the mirror through the root group, at operator cadence. The promoted owner is
  chosen in the mirror region by the same rendezvous. The loss window is the lag at the moment of loss, and zero for
  every operation that awaited the mirror.
- **Laptop.** With no mirror, the verbs refuse `Unsupported`, as they do today.

## Prior art and the semantics chosen

These are the semantics of asynchronous geo-replication with operator failover. Azure Storage GRS replicates
asynchronously to a paired region and fails over on request, with the loss window being the replication lag
(Microsoft, "Azure Storage redundancy", vendor documentation, tier B). Aurora Global Database ships storage-level
records to secondary regions and promotes one on a managed or detached failover, its recovery point the lag
(AWS documentation, tier B). Both refuse automatic failover on a partition for the reason §4.8 gives: a merely
partitioned region would keep serving beside its promotion. The register rules are the ones the home region
already runs: Vertical Paxos II (Lamport, Malkhi, Zhou, PODC 2009) with the owner as leader-acceptor (§4.8
mechanism 1), each acceptor ordering records by epoch and sequence.

## The decisions this note makes

### 1. The mirror cohort is chosen by the owner and named in the record

The owner ranks the mirror region's members it holds alive by the same rendezvous it uses at home
(`rendezvous_first` over the object id). It puts to the first `f + 1`, hedging to the rest of the `2f + 1` after
the measured p95, as content placement does at home. The owner's view of another region's members comes from the
manifest's regions (`node_regions`) and its failure detector. It can differ from the mirror council's own view,
so nothing depends on the two agreeing. The set that acknowledged is written into the mirrored head record. Every
later step, the lag and the promotion alike, uses the hosts the record names, never a recomputed cohort.

### 2. Who may put a mirror copy

A holder in region `M` accepts a mirror put of object `O`'s record or content only when all of these hold:

1. the root configuration's `home_of(O)` is a region `R` whose declared mirror is `M` (the manifest's `mirrors`,
   the same on every node);
2. `R` is not promoted;
3. the sender is an authenticated member whose declared region is `R` (its certificate-derived member id;
   `node_regions`).

Peers are trusted and crash-stop, which is the fleet's fault model; a peer that lies is out of scope, as it is at
home. Within those, the hazard is a stale owner: an owner in `R` cut off and replaced by a successor at a higher
epoch. Each mirror holder's acceptor orders records by (epoch, sequence) exactly as a home holder does, and refuses
one below the highest epoch it holds for `O`. A stale owner's puts are fenced by its successor's first mirrored
record, and until then they are records the home region committed too. Once `R` is promoted, every mirror put for
`R`'s objects is refused, so a partitioned `R` that keeps writing cannot change what `M` serves.

### 3. Shipping

A mirror job per object runs on the owner shard after the home placement is recorded (`SnapshotPlaced { region }`).
It reuses the healer's re-offer: rebuild the archive envelope from the volume, then run the ordinary content rounds
with the mirror cohort as the candidates.

- **Ordering and coalescing.** Records ship in sequence order: a mirror job for sequence `s` starts only after
  `s − 1` is mirrored or superseded. A newer snapshot supersedes a mirror job still shipping an older one, since
  the mirror needs the newest state, not every state. This is the design's "in epoch and sequence order" for a
  register, whose value is the latest write.
- **The record.** Once content is acknowledged at `f + 1` in `M`, the head record naming the mirror content holders
  is committed at `f + 1` of the same hosts. `SnapshotPlaced` then records `mirror: Some(holders)`, and
  `Placement::mirror_acked` is filled.
- **The lag.** `mirror_age` per volume is the age of the oldest snapshot recorded `placed` at home and not yet in
  the mirror. Status reports it with its observation time.

### 4. Promotion adoption

When the root group commits `PromoteRegion { lost: R }`, every node of `M` installs it. The takeover machinery
then runs with `R`'s hosts as the departed owners and `M`'s committed council membership as the survivors.

- Each object's successor is ranked among `M`'s members by the same rendezvous.
- Phase one asks `M`'s members, in the per-host paged round `takeover.rs` already runs, for the mirror records
  they hold of each departed host.
- The successor adopts the newest record at `f + 1` promises and materializes the content from the mirror holders
  that record names.
- It serves under a host epoch `M`'s council issues.

What the mirror did not receive is the loss window; the status reports it per volume.

## Pieces, in order

1. **M1, the cohort and the authority rule.** Built 2026-10-07 (`crates/db/src/mirror.rs`):
   - `mirror_cohort(object, members, quorum)`: the rendezvous prefix (`register::rendezvous_ranked`);
   - `admits_mirror_put(root, mirrors, regions, own_region, sender, object)`, with the typed `MirrorRefusal`.

   Tests: the cohort is the rendezvous prefix whatever the input order, and every authority clause is checked
   against a table.
2. **M2, shipping content and the record.** The mirror job, the mirror holders' authority path, and
   `SnapshotPlaced { mirror }`. Fleet test: two regions of three at `f = 1`; a snapshot in region 0 is held at
   `f + 1` in region 1.
3. **M3, `has_mirror` and the waits.** `has_mirror` comes from the manifest; then `await placed(mirror)`,
   `NotPlaced { mirror }` at its deadline, and `mirror_age` in `slates status ID` and the health signals.
4. **M4, promotion adoption.** Fleet test (AC-8.15): region 0 killed and promoted, the snapshot's bytes read from
   region 1, and an operation that awaited the mirror intact.
5. **M5, the two-network proof.** `docs/wip/bench/multiregion/run.sh` under a shaped router. This waits on the
   detector's far-link fix (`docs/bugs/2026-10-07-a-far-member-condemns-the-near-side-by-its-pooled-deadline.md`).
