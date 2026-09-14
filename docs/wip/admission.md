# §4.2 admission and residency — the all-cost charge (GAP-A9-1)

> Status (2026-09-13, branch `agent/admission`): **the retained-content dimension is charged**
> (piece 1a of the charter). A chunk a snapshot keeps alive after the head lets go of it is charged
> against the shard's unpromised capacity by the operation that retains it, refused typed before any
> mutation when the shard cannot back it, credited as it is freed, and re-established on recovery;
> the charge/credit balance is proven against the drift-free deadlist walk. On the way, a data-loss
> defect in chunk ownership was found and fixed first (docs/bugs/2026-09-13-snapshot-destroy-frees-
> head-shared-chunks.md). Owed from this charter, in order: allocator rounding in the write charge,
> the generated-history charge oracle, the metadata/transient ledger and the mapped-vs-usable
> per-host ledger (piece 2), and entitlement through resize/pressure/recovery (piece 3). Each lands
> as its own commit and extends this note.

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

## 5. Owed (this charter)

1. ~~Allocator rounding in the write charge~~ — done (§4a).
2. **The generated-history charge oracle** (proptest): writes, truncates, edits, unlinks, snapshots
   and snapshot destroys over a store holding a neighbour's reservation; at every step the arena's
   allocated bytes equal the head's charged blocks plus the retained bytes, every refusal changes
   nothing, and the neighbour's whole entitlement lands.
3. **Metadata and transient bytes** (piece 1b): the per-volume journal (`journal_bytes`, a heap
   `VecDeque` bounded per volume) and the fixed per-volume record cost charged as part of the
   volume's claim against a metadata ledger; the client regions and guest request buffers
   (`DaemonConfig::guest_credits` on main) composed into the same per-host picture.
4. **Disjoint per-shard credits and the truthful per-host ledger** (piece 2): the boot-time sum of
   every shard's mapped classes checked against the host's effective capacity with a typed refusal,
   mapped versus usable reported per shard.
5. **Entitlement through resize, pressure and recovery** (piece 3): shrink refusing below retained
   obligations, a pressure signal that stops admission without touching an admitted claim, and the
   restart proof that an admitted claim survives.
