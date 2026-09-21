# §4.2 admission and residency — the all-cost charge (GAP-A9-1)

> Status (2026-09-13, branch `agent/admission`, seven commits on `c80b6f9`): **the per-host admission
> is all-cost and atomic on one code path.** Content bytes are charged at the arena block a window
> takes (allocator rounding included), snapshot-retained bytes are charged from unpromised capacity
> by the operation that retains them, and every volume's metadata records are reserved from a
> per-shard ledger laid out against the metadata class — each refused typed before any mutation,
> credited as it is freed, re-established on recovery. Shard credits are disjoint mappings; the
> effective capacity clamps total RAM to the tightest OS/job/cgroup bound; mapped and usable bytes
> are both reported. An admitted claim is protected through resize (shrink refuses below use, grow
> reserves first) and recovery (the claim and its retention are re-taken ahead of new ones) —
> §§4a–4e below, each with its by-use tests and numbers. On the way two defects were found and fixed
> first: a retained inode version freed the chunks its successor shared (data loss on
> `destroy_snapshot`), and the inline spill sized window 0 to the write's end (a phantom chunk).
> **Owed:** a memory-pressure signal that stops new admission (§4e names the design), the Windows
> job-object bound, and the guest/open-reference bytes composed into the same picture (§5).
>
> **For the integrator — the ledger row (GAP-A9-1, Machine/memory/runtime):** *Per-host admission
> is all-cost on one code path: content charged at its buddy block, snapshot retention charged from
> unpromised capacity by the retaining operation (refused typed before mutation, balanced through
> destroy and recovery), metadata laid out against the class and every volume's records reserved
> from a per-shard ledger; effective capacity clamped to the OS/job/cgroup bound; mapped and usable
> reported; an admitted claim protected through resize and recovery; proven by the charge oracle
> (150 histories) and AC-2.11's neighbour test. Owed: a pressure signal that stops new admission,
> the Windows job-object bound, guest and open-reference bytes (docs/wip/admission.md).*
>
> **For the integrator — the §4.2 status blockquote:** *Status (2026-09-13). The server establishes
> the reservation: the shard budget is over the arena's buddy-usable capacity (BUG-2), a strict
> volume locks its arena or refuses (BUG-1), dynamic growth is check-and-acquire against the one
> budget (BUG-3), and the charge is all-cost — a window is charged the buddy block it takes,
> snapshot-retained chunks are charged from unpromised capacity by the operation that retains them
> (a write's reopen, a truncate's cut, an edit, the last name of a file; refused `NoSpace` before
> mutation, as OpenZFS refuses a delete on a full pool), and every volume's records are reserved from
> a per-shard metadata ledger laid out against the metadata class. The effective capacity is total
> RAM clamped to the tightest OS/job/cgroup bound; `slates status` reports mapped, usable, committed,
> retained and metadata bytes per shard. Credits are re-taken on recovery ahead of new claims; shrink
> refuses below use. Not yet: a pressure signal that stops admission (the design: sample PSI /
> `MemAvailable` at the profile refresh cadence on the control shard and apply a hold above
> `committed`, never below it), the Windows job-object bound, guest request buffers and open-reference
> maps in the same ledger, and a boot-time refusal of a hand-edited layout past the bound. GAP-A9-1's
> contract gaps (BUG-1–3, uncharged metadata/transient/retained bytes) are closed; see
> `docs/wip/admission.md`.*

## 1. The model as built

The design's invariant per host (§4.2 "Atomic admission"):

```
physical_used + outstanding_entitlement + operation_headroom + control_reserve <= effective_capacity
```

is realized per shard by one ledger, `ShardBudget` (crates/mem/src/budget.rs), which lives in the
`Store` so the write path reaches it without a lock (D-7):

| Term | Realization | Where |
|---|---|---|
| `effective_capacity` (content) | the arena's buddy-allocatable capacity, not its mapping length (BUG-2) | `Store::new` |
| `outstanding_entitlement` | every bounded volume's reservation plus every dynamic volume's admitted growth | `reserve`/`grow` |
| retained bytes (part of `physical_used` outside any entitlement) | **new:** every snapshot-retained chunk's arena block, charged from unpromised capacity | `charge_retention`/`credit_retention` |
| `operation_headroom` | `2 × chunk_bytes × clients_per_shard` (a copy-up's source and destination window per concurrent writer), kept free of every admission | `daemon.rs` `init_shard` |
| `admittable` | `capacity − committed − headroom`: what a reservation, a growth **or a retention** may still take | `Ledger::admittable` |

`committed = Σ reservations + Σ growth + retained`; the `retained` sub-account is reported beside
it (`ShardReport::retained_bytes`, `retained_versions`; `slates status` prints `retained=` and
`retained_versions=`). The inode dimension has the same shape over the version slab
(`VersionBudget`), which already charged retained versions this way; the byte dimension now does the
same, by the same predicate, so one rule governs both.

## 2. Chunk ownership (the defect fixed first)

A copy-up (`Volume::make_current_inode`) clones the inode version's body, so a retired version and
its successor name the same chunks until the head rewrites a window. `release_dead` freed a
released version's chunks anyway, so `destroy_snapshot` after a partial overwrite (or a `chmod`, or a
copy-up once the last snapshot was gone) returned the head's untouched windows as zeros. The rule
now stated in `content.rs` and enforced in `volume.rs`: **a chunk is released exactly once, by the
head, by the epoch rule when the head stops reaching it** — freed at once if born after the last
snapshot, else listed as its own `Dead::Chunk` on the newest snapshot's deadlist. A version's
release never frees chunks; `Volume::destroy` lists the head's chunks itself and the §4.8 recovery
deadlist lists a private version's chunks (and a recovered snapshot's private files are sealed, so
every block they own is listable). Proven by `crates/vfs/tests/chunk_ownership.rs` (6 histories,
4 of which failed on the unmodified tree; whole-volume destroy and a recovered snapshot's drop
still return every block, `allocated_bytes` asserted).

## 3. The retained-byte charge

**What is retained.** `Volume::retained_bytes(&store)`: the arena block lengths of every
`Dead::Chunk` on the volume's snapshot deadlists that the volume owns (born after its clone origin,
if any). Computed from the deadlists and the live chunk records — the source of truth — so it
cannot drift; `Volume::retention_charged` is the credit authority (a volume never credits more than
it charged), and the oracle asserts the two agree at every step.

**When it is charged.** By the operation that causes the retention, *before it mutates anything*,
from `admittable` (unpromised) capacity: a write's reopen of a window whose chunk a snapshot pins
(`retention_of_write`: the windows `apply_write`'s cursor enters), a truncate's cut
(`retention_of_truncate`: pieces wholly past the cut), an edit's cut and reopen
(`retention_of_edit`), and the last name of a file (`retention_of_drop`, secured in `unlink`,
`rename`-over and the overlay's two drop sites before any directory entry moves). The open extent
counts as the chunk the copy-up seals it into. The secured bytes ride in `retention_prepaid`, are
consumed as each retained chunk lands on a deadlist (`retain_dead_list`), and any surplus returns at
the operation's end (`settle_retention`). A push that finds its bytes unsecured is counted in
`retention_shortfall_bytes` (zero by construction; the oracle asserts it stays zero).

**When it is refused.** `NoSpace` (ENOSPC at a mount), with nothing changed — the accounting, the
arena, the ledger and the bytes all unchanged (`a_refused_retention_leaves_nothing_changed`), and
`retention_refusals` counts it (the non-vacuity witness). This includes an **unlink** that would
retain into promised space: the precedent is OpenZFS, whose `rm` on a full pool of a file a
snapshot still holds fails with ENOSPC until a snapshot is destroyed or space freed
[C: OpenZFS `zfs_remove` → `dmu_tx_assign` ENOSPC; the "cannot delete files when the pool is full"
FAQ]. The remedy is the same here: destroy a snapshot, or grow the shard. The alternative — backing
the overflow from the volume's own entitlement, so unlink never refuses — was considered and
rejected: it gives one charge two backing sources (unpromised capacity first, the entitlement after)
with the bookkeeping to remember which part sits where and to credit each correctly on every free,
a second path for one rule (CLAUDE.md §2 item 7), and it quietly converts the design's separate
retention dimension into ZFS `quota` semantics the design did not choose (D-13 keeps `referenced`).

**Open-unlinked files.** The last close cannot refuse, so the unlink secures the whole content's
retention when the name goes and the orphan record carries it (`orphans: BTreeMap<InodeNo, u64>`);
the reclaim at the last `unreference` consumes it and returns the surplus. A write into the orphan
meanwhile secures its own reopen, so between that write and the reclaim the same window is briefly
charged twice — bounded by open-unlinked content, never an under-charge, corrected at the reclaim.

**Clones.** A clone's deadlists may name its origin's objects (a copy-up of an inherited version
lists the version; a later rewrite lists its window); the origin's pinned snapshot holds them
whatever the clone does, so they cost the clone nothing and are neither charged nor counted
(`Volume::owns`). The inode dimension was aligned to the same rule in this change (it had charged a
clone for inherited versions).

**Credits.** `destroy_snapshot` credits the drop in the drift-free retained count across the
release (migration to the previous snapshot is neutral); `destroy` credits everything the volume
charged plus the bytes secured for its orphans; both bounded by `retention_charged`.

**Recovery.** `Volume::from_image` re-establishes the retention its rebuilt deadlists hold and
secures each recovered orphan's pinned content, all or nothing; a refused rebuild discards the
half-built volume (returning its slots and blocks) instead of leaking it. The server's recovery
path now discards on its own later failures too (`rebuild_volume`), closing the residue noted in
docs/bugs/2026-09-06-partial-volume-slab-leak-on-create-failure.md for the recovery case.

## 4. Evidence (2026-09-13, this branch, Apple silicon laptop under concurrent build load)

- `cargo test -p slates-vfs --test chunk_ownership` — before the fix: 2 passed, 4 failed (window 1
  read back as `[0, 0, 0, 0, 0, 0, 0, 0]`); after: 6 passed.
- `cargo test -p slates-vfs --test retained_bytes` — 9 passed: the balance at every step of
  snapshot-and-overwrite and destroy; whole-volume destroy; AC-2.11 against a snapshotting
  neighbour (two cycles retain the unpromised half, the third is refused at its first window, the
  neighbour then lands its whole quarter — with retention uncharged the third cycle succeeds and the
  neighbour meets an exhausted arena); unlink retained-and-refused; the open-unlinked orphan;
  truncate and edit; the clone's inherited windows; recovery; a refused retention changing nothing.
- `cargo test -p slates-vfs` — every file green (model oracle 7, recover 31, retention 3, clones 1,
  lifetime 9, base 10, edges 8, derive 3, discard 1, differential 1, reference_model 1, unit 22).
- `cargo test -p slates-mem --lib` — 37 passed; `cargo test -p slates-bridge-core --test admission`
  — 5 passed.
- Gates: `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`,
  `cargo xtask check` — clean at each commit.
- `cargo test -p slates-server --test daemon` fails in the RIFL scenario (`daemon.rs:282`: a retry
  of an acknowledged request is not refused `DuplicateRequest`) on this branch **and on the
  unmodified base `c80b6f9`** (rebuilt from `git archive` into a scratch copy sharing the same
  dependency cache, same failure, same line) — pre-existing, outside this change's paths (the
  completion window in `verbs.rs` `serve`); main's later ephemeral-id follow-up (`ff8021c`) is the
  candidate fix. Not touched here.

## 4a. Allocator rounding (piece 1, second commit)

`write_charge` charged the materialized bytes from the window's start — §4.5's chunk-window rule as
the model oracle encoded it — while the arena block a window takes is the buddy's: the smallest
power-of-two number of pages holding the materialized length. A one-byte write at a window start
charged one byte and took a page (4 KiB here, 16 KiB on the design's laptop), so a sparse writer
could hold `page ×` its quota of arena: the "uncharged bytes can defeat the cap" finding in its
allocator form, and the shape §4.2 names ("`physical_used` includes allocator rounding"; the
reservation "covers the worst permitted allocation shape"). D-13's evidence points the same way:
ZFS `referenced` counts allocated bytes.

**The rule now built and stated by the model:** a window is charged its arena block —
`charged_window(m) = min(chunk, page × next_power_of_two(ceil(m / page)))` — and the block tracks
the materialized length in both directions: a truncate that cuts a window to a smaller block rebuilds
it at its new length (`clip_extents` → `rebuilt_if_smaller`, `ChunkStore::shrink_open`), the old
chunk released by the epoch rule (retained when a snapshot pins it — `retention_of_truncate` covers
that window). So `referenced_bytes` and `unique_bytes` are physical (inline bytes, which live in the
inode, stay byte for byte) and `allocated_bytes == Σ head blocks + retained blocks` exactly — the
identity the charge oracle asserts. The first run of the model under a bare page multiple found the
buddy's power-of-two rounding (a 15-page window takes 16; `minimal failing input: Write(0, 16378,
…)`, real 65536 vs model 61440), which is why the rule names the buddy block, not the page multiple.

**A defect the truthful charge exposed at once.** `apply_write`'s inline-spill arm sized window 0's
block to the write's end capped at a chunk, whatever window the write landed in, so a far write into
a few inline bytes gave window 0 a whole chunk (uncharged under the logical rule; charged and refused
under the physical one). Fixed: window 0 is opened at what it will hold
(docs/bugs/2026-09-13-inline-spill-sizes-window-zero-to-the-write-end.md).

**Contract change (for the design amendment log).** T-1.3 (`crates/vfs/tests/edges.rs`) asserted
"charges one chunk holding one byte" with `referenced_bytes == 1` beside `allocated_bytes == PAGE`;
it now asserts `referenced_bytes == PAGE` — the charge is the block, so the two numbers are one.
§4.5 "Write"/the chunk-window rule and the Phase 1 status line "the write charge is the materialized
delta under the chunk-window rule, which the model encodes" should read: *charged by the chunk-window
rule at the allocator's granule — each window its buddy block (the smallest power-of-two number of
pages holding the materialized length, at most a chunk); a partly cut window is rebuilt at its new
length; the model encodes the same rule.* Evidence: `cargo test -p slates-vfs --test charge` (the
sparse writer: sixteen one-byte windows admitted, the seventeenth refused, the arena holds exactly
the quota; a truncate returns the pages a window no longer takes) and the model oracle (7 passed,
400 histories each) after the change.

## 4b. The charge oracle (piece 1, third commit)

`crates/vfs/tests/charge_oracle.rs`: 150 generated histories of 1–40 steps — writes of 1–48 pages
at page offsets in the first four windows of three files, truncates to arbitrary lengths, edits
(splices), unlinks, snapshots (three live at most) and snapshot destroys in every order — over a
store holding a neighbour's reservation of three quarters of the shard and the volume's own eighth.
After every step: `allocated_bytes == referenced_bytes + retained_bytes` (the arena holds exactly
the head's charged blocks plus the retained ones — allocator rounding, chunk ownership and the
retention charge agreeing to the byte); `committed == reservations + retained_bytes ≤ capacity`;
every reserved byte not yet used is physically free; `retention_shortfall_bytes == 0`; a refused
step changed nothing (accounting, arena, chunk count, ledger, retained bytes, and every file's
bytes); every file reads back its shadow. At the end the neighbour writes its whole reservation in
whole windows and every write lands. Unlike `model.rs`, these histories destroy snapshots, which is
where a chunk freed twice or too early shows (the C1 defect's class). `cargo test -p slates-vfs
--test charge_oracle`: 1 passed, 150 cases, 2.35 s.

## 4c. The metadata dimension (piece 1b)

**The finding.** The store's slabs (directory nodes, directory blocks, inode versions, inode-table
nodes, chunk records) are lazily allocated heap segments bounded only by their caps, and the caps
were not laid out against the shard's metadata class: `max_dir_blocks` equalled `max_dirs =
reserve / size_of::<DirNode>() / 6` while a block carries a 4 KiB entry area inline, so the block
slab alone could grow to roughly forty times the directory-node share — several times the whole
class — and every volume's journal (`quota × 1 %`, at least a page, a heap `VecDeque`) and object
were uncharged. Metadata could defeat the per-host cap while the byte ledger looked healthy.

**The layout now built.** The metadata class is the shard's second memory class (one reserve,
`StoreCaps::metadata_class_bytes`). Each slab's bound is the class over the true cost of its
dimension's unit — `Slab::<T>::slot_bytes()` (the value plus its generation and vacancy link):
an inode is a version slot and a table slot (`inode_unit_bytes`), a directory a node and one block
(`directory_unit_bytes`; the block bound is the directory bound, a large directory's extra blocks
coming out of the same count), a chunk record one per arena page — each slab dimension taking a
sixth of the class (`STORE_TABLE_DIVISOR`), so the slabs' maximum footprints sum to about a third of
it. At boot the daemon calls `Store::set_metadata_class(class)`: the slabs' maximum footprint
(`Store::slab_footprint_bytes`) comes off the top and the remainder is the **records ledger**
(`MetadataBudget`, crates/mem/src/budget.rs); a layout whose slabs alone exceed the class is refused
before serving (`ServerError::Memory(BudgetExceeded)`). Every create — scratch, clone, taken-over —
reserves `Volume::metadata_footprint(journal_bytes, page)` (the journal's whole retention budget,
the volume object, the snapshot slab's first segment) from that ledger before the volume exists,
whole or refused `BudgetExceeded`; the credit rides in the volume's slot, is released on destroy and
on every failure path, and is re-acquired on recovery (a refused recovery discards the rebuilt
volume). `ShardReport` carries `metadata_bytes`/`committed_metadata`; `slates status` prints them.
Not counted: the open-reference maps (bounded by the bridge's handle slab) and the db partition's
tables (in the anchor segment, sized from `table_bytes` — the third class with the client regions).

**Evidence.** `cargo test -p slates-vfs --test metadata` (a class the slabs fill is refused; the
ledger is the class less the slabs; two volumes' records fit, a third is refused, a release backs
another). `cargo test -p slates-server --lib
the_derived_slab_bounds_lay_the_metadata_class_out_with_room_for_records` (the derived caps' slabs
fit the class with at least half left for records — with the old block bound the class was refused).
`cargo test -p slates-server --test daemon a_metadata_class_bounds_the_volume_records_a_shard_admits
-- --exact` (a daemon whose class holds its slabs plus 64 KiB of records: creates land until the
ledger binds, the next is refused `BudgetExceeded` while bytes and version slots are plainly roomy,
a destroy returns the records and a fresh create lands; 0.87 s).

## 4d. Effective capacity and mapped versus usable (piece 2)

**Disjoint shard credits.** Each shard maps its own arena region of `reserve_per_shard` bytes, its
own slabs and its own share of client regions, so no two shards are ever handed the same bytes:
the "control owner distributes disjoint capacity credits to shards" rule is realized by disjoint
mappings laid out at boot, and the write path consumes local credits with no shared lock (D-7).
The laptop is the fleet's degenerate: the same accounting at N = 1 (R8).

**Effective capacity.** The reserve was derived from the machine's total RAM (1f40689's stable
virtual ceiling, kept). It is now clamped to the tightest bound the OS, a job or a cgroup sets on
the process (§4.2 "effective capacity ... constrained by OS/job/cgroup/lock limits"):
`MemoryFacts::limit` — on Linux the smallest `memory.max` (cgroup v2) or `memory.limit_in_bytes`
(v1) up the process's cgroup path from `/proc/self/cgroup` (a parent's limit binds its children),
on every Unix a finite `RLIMIT_AS`/`RLIMIT_DATA`; Windows reports none yet (the job object's limit
is owed). `effective_capacity(total, limit) = min(total, limit)` is the pure decision, cfg-free and
unit-tested with hostile limit texts (`max`, empty, signs, suffixes, overflow → no bound, never a
guessed one); the reserve is `region_bytes(effective, shards, classes)` and the boot log records
`effective_capacity_bytes` with its inputs. A container limited to 2 GiB on a 128 GiB host no
longer derives 42 GiB shard reserves it would be killed for using. Raw free memory still never
enters (it is a fluctuating snapshot). The profile field is `#[serde(default)]`, so a profile
written before it existed still reads.

**Mapped versus usable.** `ShardReport::mapped_bytes` (the arena's address space) now stands beside
`reserve_bytes` (the buddy-allocatable capacity the budget admits against), so the difference is
visible rather than implied (§4.2 "geometry report usable capacity, not mapping length"; BUG-2's
budget-over-usable rule is unchanged and still gated by `crates/mem/tests/capacity.rs`). `slates
status` prints `mapped=` before `reserve=`.

## 5. Owed (this charter)

1. ~~Allocator rounding in the write charge~~ — done (§4a).
2. ~~The generated-history charge oracle~~ — done (§4b).
3. ~~Metadata bytes~~ — done (§4c). Still owed in this dimension: the open-reference maps (~48 B per
   open inode, bounded by the bridge's handle slab) and the guest request buffers
   (`DaemonConfig::guest_credits` on main, derived at boot) composed into the same per-host picture.
4. ~~Effective capacity and mapped versus usable~~ — done (§4d). Still owed: the Windows job-object
   memory limit (`QueryInformationJobObject`, Win32 FFI under the unsafe budget), and a boot-time
   typed refusal when a shard's mapped classes exceed the host bound (today the classes are derived
   from the bound, so the sum fits by construction; the check would guard a hand-edited config).
5. ~~Entitlement through resize and recovery~~ — done (§4e). ~~**Pressure**~~ — **done 2026-09-21
   (§5.5).** The hold is on each shard's byte budget — `admittable = capacity − committed − headroom
   − hold` (`ShardBudget::set_hold`/`hold`, `crates/mem/src/budget.rs`) — and shrinks admittable
   only, never revoking a committed claim, so no admitted volume's within-entitlement write is
   touched. The daemon's reap loop samples `Facts::memory_available_now` at the liveness cadence (the
   cheap refresh), captures a boot baseline, and fans the host-wide shortfall below it, divided
   evenly among the shards, as each shard's hold (`daemon::refresh_pressure_hold` over
   `xshard::run_on`); the hold releases to zero as the sample recovers, and a host with no sampler
   leaves it at zero (admits exactly as before). Proven by use: a raised hold refuses a new bounded
   create `BudgetExceeded` while an admitted volume's further write within its limit still lands
   (`crates/server/tests/nfs_mount.rs::a_memory_pressure_hold_refuses_new_admission_but_not_an_admitted_volumes_writes`,
   with `Daemon::inject_pressure_hold`/`pressure_holds` driving the mechanism deterministically), and
   by the pure ledger unit
   (`crates/mem/src/budget.rs::a_pressure_hold_withholds_admission_without_touching_a_committed_claim`).
   Still owed in this dimension (not the pressure hold): the finer pressure source (Linux PSI
   `memory.some` where the kernel offers it, over the coarser `MemAvailable`), and the Windows
   job-object memory limit sub-part of §5.4.

## 4e. Resize and recovery (piece 3)

**Resize.** Grow reserves the additional entitlement first (the server's `resize`: the version
reservation, then the byte reservation, each refused whole before anything changes) and shrink is
applied after the core accepts the new limit; the core refuses a shrink below current use
(`Quota::resize`, now against physical `referenced_bytes`) with nothing changed. Retained bytes are
charged outside the quota, so a shrink cannot strand them ("shrink refuses below retained
obligations" is met by construction: the obligations are not the quota's).

**Recovery.** `rebuild_volume` re-takes the byte reservation and the dynamic hold before the
rebuild, `from_image` re-establishes the retention charge, and the inode allowance and metadata
records are re-reserved after — a rebuild the fresh shard cannot back is discarded whole. So an
admitted claim survives a restart ahead of any new claim; the server-level restart oracle
(`crates/client/tests/client.rs`, a session outliving a daemon restart) already exercises the
reservation half.

**Evidence.** `cargo test -p slates-vfs --test entitlement` — 3 passed: a shrink below current use
is refused `NoSpace` with nothing changed and the claim stays spendable at its old limit; with two
quarter claims and retention filling the unpromised half, a new claim is refused `BudgetExceeded`
while A lands the other half of its own quota and B its whole quarter; after a rebuild on a fresh
shard behind B's claim, the ledger holds both claims plus A's retention, a claim that would need the
retained bytes is refused, A writes within its claim and B lands its whole claim.
