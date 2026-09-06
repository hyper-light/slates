# §4.2 resource vector — design for the inode dimension (and the rest)

> Status: the **inode and namespace dimensions are landed**; the remaining dimensions (xattr,
> handle) and full reservation are designed below. The namespace dimension bounds live directory
> entries (`Volume::dir_insert`/`dir_remove` maintain the count, `next_no`'s sibling), so hard-link
> fan-out — one inode, many names — is bounded where the byte quota and inode allowance are not; the
> server derives it as `quota / size_of::<Child>()`. Fixtures leave it unbounded, so the oracle is
> untouched. The volume enforces a per-volume inode allowance with correct live-count accounting; the
> server derives the allowance from the volume's quota (`min(quota / size_of::<Inode>,
> store.max_inodes)`) rather than the tight fair-share first tried, and the vfs test fixtures leave
> it unbounded so the determinism oracle is untouched. §4.2 and GAP-A9-1 are the authority; BUG-1
> (locked store) and BUG-2 (usable capacity) are also fixed. Xattr is not applicable (unimplemented)
> and open handles are bounded at the bridge's `Slab` (BUG-4), so the applicable per-volume caps are
> all in place and *safe* (typed refusals at both the logical allowance and the version slab). The
> **byte dimension's reservation is now complete** (§4): `ShardBudget` keeps a *derived* operation
> headroom (`2 × chunk_bytes × clients_per_shard`) free of every admission, reservation and dynamic
> growth alike, so a bounded volume's claim is sacred and growth takes only unpromised capacity —
> through the one budget, never against raw host memory. What remains: the retention dimension, and
> the step from a per-volume cap to disjoint per-volume *reservation* for the **inode** dimension —
> which §4 shows is data-plane-gated for *its* `operation_headroom` (the measured peak of transient
> CoW versions, which has no structural anchor the byte headroom has), exactly as the byte dimension's
> live per-growth re-check is gated on the data-plane write path. The honest inode cap stands until
> that measurement exists.

## 1. The requirement

§4.2: "A byte quota cannot bound arbitrarily many empty files, names, xattrs, open handles or
snapshots. Admission therefore publishes a resource vector: content bytes plus inode/namespace,
xattr, handle, in-flight and retention allowances, each derived from the requested policy and the
prepared arena layout. Every dimension has its own bound and refusal; `statfs` includes backed
inode availability." An empty file costs an inode but no content bytes, so the byte quota alone
lets one volume exhaust the shard's inode slab — the concern this dimension closes.

## 2. The inode dimension

**Enforcement point — one site.** `Volume::next_no` (crates/vfs/src/volume.rs) is the single place a
new inode number is issued (called by `create_file`, `mkdir`, `symlink`). Make it fallible: check
the allowance, increment the live count, or refuse. The three verbs change `self.next_no()` to
`self.next_no()?`; nothing else issues numbers.

**Live-count accounting — two mutation sites plus initialisation.**
- `next_no`: `+1` (a new logical inode).
- `reclaim_inode` (the single permanent-free site; `table_remove` drops the number): `−1`.
- `Volume::create`: `1` (the root).
- `Volume::clone_of`: the origin's live count at clone time (a clone logically holds the inherited
  inodes — §4.2 dedup charges each volume's full logical count).
- `Volume::recovery_shell`/`from_image`: the recovered head's inode count.
- Snapshot: no change to the head count (a snapshot shares the head's inodes; retained-after-divergence
  inodes belong to the **retention** dimension below, not the head count).

Count *logical* inodes (numbers), not CoW versions: `next_no` fires once per logical inode, while a
mutation that copies-on-write inserts a new *version* of an existing number and must not be counted.

**Derivation — the sticking point, and why the oracle must move with it.** The allowance must bound
empty files, so it cannot be derived from the *content* byte quota (empty files are 0 bytes — that
is the whole point). Two candidate derivations:

1. `quota.limit() / size_of::<Inode>()` — inodes whose metadata would fit in the volume's reserved
   footprint. Principled and per-volume, but tiny for a tiny quota.
2. A fair share of the shard's inode slab: `store max_inodes / volumes_per_shard` (both already in
   `StoreConfig`/`DaemonConfig`), threaded into `VolumeConfig`. Larger, and tied to the prepared
   arena layout as §4.2 asks, but needs the store capacity passed to `Volume::create`.

Recommendation: derivation (1) with the floor from (2) — `max(quota/size_of::<Inode>, fair_share)` —
so no magic number appears and a small-quota volume still gets its fair share of inode capacity.

**The oracle coordination (verified, not assumed).** `crates/vfs/tests/model.rs` has two proptest
oracles:
- `the_volume_equals_the_model_on_every_history` at quota `1<<20` with 1..40 steps — allowance under
  either derivation is in the thousands, never hit; **unaffected**.
- `a_tight_quota_refuses_the_same_writes_as_the_model` at quota **150** with 1..30 steps — its whole
  premise is that empty-file *creates* succeed while *writes* are byte-refused. Any inode allowance
  changes that. This test must move with the change: the reference `Model` (model.rs) gains the same
  inode-count-and-allowance rule (stating the design's rule, not the implementation's — the model.rs
  gotcha), and the tight test's quota is chosen so a few creates still succeed and their writes are
  still byte-refused, so it keeps testing byte refusals. Skipping this makes the oracle either fail
  or certify drift.

**A dedicated test.** A volume whose allowance is small (a config with a low fair-share, or a small
quota) accepts creates up to the allowance, refuses the next with `NoSpace` (ENOSPC — §4.2: "a
create beyond an advertised inode allowance may return ENOSPC even when content-byte space
remains"), and accepts again after an unlink frees one — proving the live count decrements.

**`statfs`.** `bridge-nfs`/`bridge-core` `statfs` should report backed inode availability
(`allowance − live`), owed with the data-plane `statfs` work (BUG-9, GAP-A9-3).

## 3. The other dimensions

- **Namespace** (directory entries): *landed*, the same shape — a live-entry count at
  `dir_insert`/`dir_remove`, a `quota / size_of::<Child>()` allowance, a `NoSpace` refusal — bounding
  hard-link fan-out the inode allowance cannot.
- **Xattr**: **not applicable** — the volume core does not implement extended attributes, so there is
  no resource to bound; a dimension would be added with the feature.
- **Open handles**: **already bounded elsewhere** — the bridge's attachment/handle registry is a
  bounded `Slab` that refuses `SlabFull` (BUG-4). Charging those handles against §4.2 admission (so
  they count toward the reservation below) is the owed refinement.
- **In-flight**: bounded by the ring/credit admission (§4.7), not a per-volume vfs dimension.
- **Retention** (snapshot-retained inodes and bytes): interacts with recovery — a recovered
  snapshot's retained content is a separate charge from the head (§4.2 "a new retained snapshot may
  require a separate retention charge"); it pairs with snapshot recovery (docs/wip/recovery.md).

So the applicable per-volume **caps** are in place (content=quota, inode, namespace); the remaining
§4.2 work is the step from cap to **reservation** below, plus retention and the handle charge.

## 4. Full admission (beyond a cap)

The above is a per-volume *cap*. The complete §4.2 rule *reserves* each dimension's credits from
physically-backed capacity before publishing (disjoint per-shard credits, protected through resize,
pressure and recovery). That builds on the cap: once each dimension is counted and bounded, admission
reserves the vector rather than only capping it, and `ShardBudget` grows a per-dimension reserve
alongside its byte reserve.

**The byte dimension's reservation is landed, headroom and growth included.** `ShardBudget` is the
one capacity owner for content bytes (crates/mem/src/budget.rs): it keeps an `operation_headroom` free
of *every* admission, reservation and growth alike (`admittable = capacity − committed − headroom`),
and the headroom is now **derived, not a placeholder** — `2 × chunk_bytes × clients_per_shard` (a
copy-up's source and destination chunk per concurrent writer), from structural anchors, replacing the
old `reserve_per_shard / clients` stand-in. Earlier only `grow` respected the floor while `reserve`
did not, so a bounded volume could commit into the headroom; now both obey the one `admittable` rule,
so a bounded reservation keeps it free and a dynamic growth takes only capacity neither committed to
another volume nor reserved as headroom (a sacred claim is never eaten — gated in budget.rs by
`dynamic_growth_cannot_eat_a_bounded_reservation_or_the_headroom`). Dynamic growth is admitted through
this same budget: the daemon's dynamic-volume pressure source is now a budget-anchored ceiling, not
the raw `memory_available_now()` the design forbids ("raw free RAM … do not qualify"). What remains
for the byte dimension is the *live* per-growth re-check against the budget as later volumes are
admitted — owed with the data-plane write path (BUG-3), the only place a dynamic volume actually
grows; the control-path growth (`resize`) already reserves through the headroom-respecting budget.

The **inode** dimension's reservation is a separate story, below: it turns on a headroom that must be
measured, not derived, so it stays a cap for now.

**Where the current state already is safe.** Both inode enforcement points are typed refusals, so
today's cap never corrupts or panics — it can only be *over-optimistic* about availability, never
unsafe. `Volume::next_no` refuses a logical inode past the per-volume allowance (`NoSpace`), and the
inode-version slab is a hard `Slab` that refuses past `store.max_inodes` (`SlabFull`). The gap the
reservation closes is honesty, not safety: the *sum* of per-volume logical allowances can exceed the
shard's version slab, so an advertised allowance need not be physically backed.

**Why the reservation is not a clean isolated slice — the version/logical gap and its measured
headroom.** `store.max_inodes` bounds inode *versions*, not logical inodes, and
`Volume::make_current_inode` (crates/vfs/src/volume.rs) inserts a **new** slab slot whenever it
mutates an inode whose `born` epoch is earlier than the head's (a snapshot or a prior epoch pinned
the old version), retiring the old slot for reclaim at the next epoch boundary. So the version slab
holds, at any instant, the logical inodes **plus** the snapshot-retained versions **plus** the
transient in-epoch versions that are retired but not yet reclaimed. A correct reservation must
therefore carry an `operation_headroom` for those transient versions (the §4.2 invariant's
`operation_headroom` term) — as the byte budget now keeps a *derived* operation headroom free
(`ShardBudget`, `2 × chunk_bytes × clients_per_shard`; §4 above). But the inode equivalent has no such
structural anchor: it is the peak of concurrent retired-but-unreclaimed versions, which depends on the
write rate against
the epoch/reclaim cadence — a quantity that can only be measured with the data-plane write path
driving the slab under load. Reserving the logical allowance as version-slots **without** that
headroom would be a *false* guarantee: a volume within its logical allowance could still exhaust the
version slab through in-epoch CoW churn before a reclaim pass runs. Under R3 a headroom number cannot
be invented, and under R4/R5 a reservation must not advertise a guarantee it does not hold, so this
step waits on the peak-version-burst measurement — it is data-plane-gated for its headroom exactly as
BUG-3 is gated for its growth source.

**What the reservation's own observable proof would be (once the headroom exists).** A create
refusal, not only `statfs`: fill a shard's inode-version reserve with prior volumes, then a further
create refuses with a resource-vector refusal even though byte budget remains. `statfs` reporting
backed inode availability (`allowance − live`, and the reserve's remaining credits) is the second
surface and is owed with the data-plane `statfs` work (BUG-9, GAP-A9-3). BUG-3 (dynamic growth
consuming only unpromised capacity) is part of this same reservation and is likewise gated on the
data-plane write path (docs/wip/recovery.md).
