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
> **byte dimension's reservation — including live dynamic-growth admission — is landed** (§4):
> `ShardBudget` (now in the `Store`, reached by the write path without a lock) keeps a *derived*
> operation headroom (`2 × chunk_bytes × clients_per_shard`) free of every admission, and a dynamic
> volume's growth does a check-and-acquire against that one budget on each increment (`BudgetGrowth`),
> debiting it — so two dynamic volumes cannot spend the same capacity, a dynamic volume cannot consume
> a bounded volume's entitlement, and the hold is accounted through teardown and recovery. This is
> proven through the real write path, no mount, in `crates/bridge-core/tests/admission.rs` (the five
> histories). What remains: the retention dimension, and the step from a per-volume cap to disjoint
> per-volume *reservation* for the **inode** dimension — which §4 shows waits on a *measured*
> `operation_headroom` (the peak of transient CoW versions, which has no structural anchor the byte
> headroom has). That measurement needs the version slab driven under write load; the honest inode cap
> stands until it exists. Mounted POSIX and guest conformance are separately pending host environments.

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
- **Retention** (snapshot-retained inode versions): *the mechanism is landed.* The version slab
  (`store.max_inodes`) is shared, and a volume's snapshots retain old inode versions as the head
  diverges (on the snapshots' deadlists), which the per-volume inode allowance — a *logical* count via
  `next_no` — does not bound, so one volume's snapshots could consume the slab and starve others (safe
  via `SlabFull`, but unfair). Now `Volume::retained_versions` (crates/vfs/src/volume.rs) computes the
  count from the deadlists — the source of truth, so it cannot drift from a maintained counter, and
  the "why not built" worry (a decrement missed across `retire`/`destroy_snapshot`/`destroy`) does not
  arise — and `make_current_inode` refuses a diverging copy-up with `NoSpace` when a snapshot would
  pin a version past the volume's `retention_allowance` (as §4.2 asks: "a new retained snapshot ...
  cannot use up a writer's promised future space"). Gated in crates/vfs/tests/recover.rs
  (`retained_versions_counts_the_snapshot_pinned_inode_versions` and
  `the_retention_allowance_refuses_a_diverging_write_past_the_bound`, the refusal leaving the write
  with no partial effect). The allowance is unbounded (`u64::MAX`) by default, so nothing is affected
  until an owner sets it.
  - *What remains — the server policy:* the daemon does not yet *set* a retention allowance (like it
    sets the inode/namespace ones), so production leaves it unbounded. The derivation needs care: too
    tight a bound would refuse a legitimate snapshot-and-diverge workflow, so it wants a reasoned or
    measured share of the version slab, not a guessed number. The fuller step is the *disjoint
    reservation*: charge retention (and reserve the logical allowance) from a shared version budget
    over `max_inodes`, the exact parallel of the byte `ShardBudget`.

So the applicable per-volume **caps** are in place (content=quota, inode, namespace, retention); the
remaining §4.2 work is the step from cap to **reservation** below, plus wiring the server to set the
retention allowance and the handle charge.

## 4. Full admission (beyond a cap)

The above is a per-volume *cap*. The complete §4.2 rule *reserves* each dimension's credits from
physically-backed capacity before publishing (disjoint per-shard credits, protected through resize,
pressure and recovery). That builds on the cap: once each dimension is counted and bounded, admission
reserves the vector rather than only capping it, and `ShardBudget` grows a per-dimension reserve
alongside its byte reserve.

**The byte dimension's reservation is landed — live dynamic-growth admission included.**
`ShardBudget` is the one capacity owner for content bytes (crates/mem/src/budget.rs), and it now lives
in the `Store` so the write path reaches it without a lock (the shard is single-threaded). It keeps an
`operation_headroom` free of *every* admission, reservation and growth alike
(`admittable = capacity − committed − headroom`), and the headroom is **derived, not a placeholder** —
`2 × chunk_bytes × clients_per_shard` (a copy-up's source and destination chunk per concurrent
writer), from structural anchors, replacing the old `reserve_per_shard / clients` stand-in. Earlier
only `grow` respected the floor while `reserve` did not; now both obey the one `admittable` rule.

The growth path is the part that had bypassed ownership accounting, and it no longer does. A dynamic
volume's source is `BudgetGrowth`, which on *each* increment does a **check-and-acquire together**
against the live shard budget (`Quota::admit` → `source.may_grow(bytes, &mut store.budget)` →
`budget.grow`), debiting it — not a private per-volume ceiling and never the raw `memory_available_now()`
the design forbids. So two dynamic volumes cannot receive the same capacity, a dynamic volume cannot
consume a bounded volume's entitlement, and a bounded volume's admission accounts for growth already
taken. The acquired bytes are tracked as the quota's `granted` (`Volume::budget_hold`) and released to
the budget on teardown, and re-acquired from the rebuilt budget on recovery (`rebuild_volume`) — so the
reservation is accounted through allocation, failure, teardown and recovery. This is exercised through
the real write path, no mount: `crates/bridge-core/tests/admission.rs` drives `VolumeBridge::write`
with the server's quota construction and admission for all five histories (a dynamic volume cannot eat
a bounded entitlement; a bounded admission accounts for prior growth; two dynamic volumes cannot
double-spend; a refused growth leaves credits consistent with what is retained; a bounded volume keeps
its allowance after competing growth is refused) — non-vacuously (the old private ceiling fails four of
the five).

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
step waits on the peak-version-burst measurement: the version slab driven under write load — which
`VolumeBridge::write` can now do, no mount, exactly as the byte admission tests drive growth — to
establish the transient-version headroom before reserving version credits. It is a separate, unbuilt
piece; it is *not* the byte dimension's growth admission, which is done (§4 above).

**What the reservation's own observable proof would be (once the headroom exists).** A create
refusal, not only `statfs`: fill a shard's inode-version reserve with prior volumes, then a further
create refuses with a resource-vector refusal even though byte budget remains. `statfs` reporting
backed inode availability (`allowance − live`, and the reserve's remaining credits) is the second
surface and is owed with the data-plane `statfs` work (BUG-9, GAP-A9-3). BUG-3 (dynamic growth
consuming only unpromised capacity) is **fixed** for the byte dimension — `BudgetGrowth` debits the one
budget on each increment, gated by `crates/bridge-core/tests/admission.rs`; the inode version-credit
reservation is the remaining, separate piece.
