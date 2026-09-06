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
> (locked store) and BUG-2 (usable capacity) are also fixed. What remains: the namespace/xattr/handle
> dimensions, the retention dimension, and the step from a per-volume cap to disjoint per-volume
> reservation (with BUG-3's data-plane dependency).

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
alongside its byte reserve. BUG-3 (dynamic growth consuming only unpromised capacity) is part of this
and is additionally gated on the data-plane write path (docs/wip/recovery.md).
