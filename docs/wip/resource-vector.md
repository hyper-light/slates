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
> histories). The **inode dimension's reservation is now landed too**: a volume's logical inode
> allowance is reserved against a per-shard `VersionBudget` over the version slab (`store.max_inodes`,
> crates/mem/src/budget.rs) at create — bounded and dynamic alike, on both the scratch-create and
> clone-create paths and re-acquired on recovery — and released on destroy, so the *sum* of advertised
> allowances is physically backed by the slab, not merely capped against `SlabFull`. An earlier
> version of this note deferred it as waiting on a *measured* `operation_headroom` (the peak of
> transient CoW versions under write load); **that was wrong.** The design defines `operation_headroom`
> as "bounded temporary coexistence during copy-up" with "measured *or structural* anchors" (§4.2,
> SLATES_DESIGN.md:809–816), and the code confirms the structural reading: `Volume::make_current_inode`
> inserts the new version and retires the old within one synchronous, exclusively-borrowed shard call
> (no `await` between), and `retire` frees the old at once or keeps it on a snapshot deadlist — the
> **retention** dimension, now bounded — so at most one transient version coexists per copy-up. The
> headroom is therefore the structural constant one, not a write-rate measurement. Proven through the
> real create and destroy verbs, no mount, in `crates/server/tests/daemon.rs` (a second volume refused
> while physical slots plainly remain; non-vacuous — without the reservation both are created). Resize
> now moves the inode allowance with the policy too: it re-derives the allowance and grows or shrinks
> the version reservation the same grow-first/shrink-after way as the byte reserve (a resize-down
> returns slots that back another volume). What remains for the inode dimension: charging *retention*
> into the same `VersionBudget` (so retained versions and logical allowances share the slab disjointly).
> Mounted POSIX and guest conformance are separately pending host environments.

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

The **inode** dimension's reservation is now landed too, on a *structural* headroom (below).

**Where the current state was already safe, and is now backed.** Both inode enforcement points are
typed refusals, so the cap never corrupts or panics — it could only be *over-optimistic* about
availability. `Volume::next_no` refuses a logical inode past the per-volume allowance (`NoSpace`), and
the inode-version slab is a hard `Slab` that refuses past `store.max_inodes` (`SlabFull`). The gap the
reservation closes is honesty, not safety: the *sum* of per-volume logical allowances could exceed the
shard's version slab, so an advertised allowance need not have been physically backed. The reservation
below closes that gap — the sum of advertised allowances can no longer exceed the backed slab.

**Why the headroom is structural, not measured — correcting an earlier mistake.** An earlier version
of this section claimed the inode reservation waited on a *measured* `operation_headroom` — "the peak
of concurrent retired-but-unreclaimed versions, which depends on the write rate against the
epoch/reclaim cadence." That was wrong, on two counts the code settles. First, `retire`
(crates/vfs/src/volume.rs) resolves a copy-up's old version *immediately*: it is either kept on a
snapshot deadlist (the **retention** dimension, now bounded by `retention_allowance`) or freed at once
by `release_dead` — there is no third "retired-but-unreclaimed" pool that accumulates with write rate;
the trie's own retired nodes are drained in the same `table_set`/`table_remove` call. Second,
`Volume::make_current_inode` inserts the new version and retires the old within one *synchronous,
exclusively-borrowed* shard call — there is no `await` between the insert and the retire, and the
single-threaded shard runs no other operation meanwhile — so at most **one** transient inode version
coexists at a time, regardless of the client count. The `operation_headroom` is therefore the
structural constant one (`COPY_UP_VERSION_HEADROOM`, crates/vfs/src/volume.rs), exactly the design's
"bounded temporary coexistence during copy-up" with "measured *or structural* anchors" — no
measurement, no invented number. The write-dependent accumulation the earlier note feared is real, but
it *is* the retention dimension, charged separately, not a transient headroom.

**The reservation, as built.** A per-shard `VersionBudget` (crates/mem/src/budget.rs) — the counted
parallel of `ShardBudget`, sharing its `Ledger` arithmetic — is over the version slab
(`store.max_inodes`) less the one-slot copy-up headroom. At create, the server reserves the volume's
whole logical inode allowance against it (`state.store.versions.reserve(inode_allowance(...))`,
crates/server/src/verbs.rs) *before* `Volume::create` (and before `clone_of`) — the design's "reserve
all required credits or none before publishing" — so a refusal allocates no root inode, trie or dir to
leak, and a clone refusal does not leave the origin's `clone_refs` bumped. Refused whole (giving back
the byte reservation) if the slab cannot back it; the allowance is capped at `max_inodes − headroom`
so the largest derivable allowance is still reservable. The credit rides in the volume's server slot
and is released on destroy, given back on every later failure path (including the slot-insert failure,
which previously leaked the byte reservation — a sibling bug fixed here), and re-acquired from the
rebuilt budget on recovery. A clone now also carries an inode cap (it previously had none), set to that
same allowance, so its reservation actually bounds it. (A partial volume dropped on the *db-mutate* or
*slot-insert* failure — the paths still after allocation — leaks its slab slots; that is the residue
tracked in docs/bugs/2026-09-06-partial-volume-slab-leak-on-create-failure.md, now that the common
reservation-refused path no longer allocates.)

**Its observable proof (now exercised).** A create refusal, not only `statfs`: `crates/server/tests/
daemon.rs` fills a small shard's version slab with one big-quota volume, then 24 further creates are
each refused (`BudgetExceeded`) *while physical slots plainly remain* — the disjoint reservation the
bare cap does not give — and one more is admitted once destroy returns the first's slots. It is
non-vacuous two ways: neutered to `reserve(0)`, the reservation test creates the second volume and
fails; and run against the pre-reorder ordering (reservation after `Volume::create`), the leak probes
turn into `SlabFull` by the fourth attempt as leaked partial volumes fill the trie slab. Shard-level
availability is now a second surface too: the daemon status carries the version slab's capacity and
committed slots (`ShardReport::version_slots`/`committed_versions`, beside the byte reserve it already
reports, and the per-volume `allowance − live` the mounted `statfs` reports), gated in
`crates/server/tests/daemon.rs` (committed slots move as a volume's allowance is reserved). Resize
re-derives the allowance and grows/shrinks its version reservation (grow-first, shrink-after, with a
resize-up refused whole before anything changes if the slab cannot back it; gated in
`crates/server/tests/daemon.rs`, non-vacuous — the pre-resize code leaves the resized volume's old
reservation standing and the second volume refused). What remains for the inode dimension: charging
*retention* into the same `VersionBudget` so retained versions and logical allowances share the slab
disjointly. BUG-3 (dynamic growth consuming only unpromised capacity) stays **fixed** for the byte
dimension.
