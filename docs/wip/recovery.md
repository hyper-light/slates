# Anchor-owned volume storage and recovery (§4.2, §4.8): milestone design

> Status: in progress. The memory foundation (a region and an anchor content object that survive a
> daemon restart) and the volume recovery **image** capture are landed and gated; the image rebuild,
> the daemon/content-object wiring, and the process-level write→kill→restart→read proof are owed.
> This doc is the assistant-owned record of the milestone; the numbered requirements live in
> `SLATES_DESIGN.md` §4.2 and §4.8 (A-9), and the gap ledger is `GAPS.md`.

## 1. The requirement (settled, not a choice)

Two design sections make anchor-owned storage and real recovery mandatory, and `GAPS.md` puts them
first:

- **§4.2 (physically-backed admission).** A volume's admission invariant is
  `physical_used + outstanding_entitlement + operation_headroom + control_reserve <=
  effective_capacity`. The bytes a claim will need must be reserved against physically-backed,
  anchor-owned capacity *before* the claim is published, including copy-on-write divergence
  capacity; capacity that cannot be backed is a typed refusal.
- **§4.8 (A-9 recovery).** Local record recovery must recover *all* reachable bytes, roots, bases,
  witnesses, rights, reservations and completion records from anchor-owned RAM. "Rebuilding a
  scratch volume from only a quota and id loses acknowledged data." Missing resources return
  `RecoveryIncomplete`/`BaseUnavailable`, never empty success. The scope for this milestone is
  **daemon-crash survival** (the anchor process outlives the daemon), not host reboot.

Ada's directive (2026-09-06): deliver one concrete recovery slice first — "create a scratch volume,
write bytes, kill the daemon while the anchor survives, restart, and read the same bytes through the
client" — then extend to snapshots, clones, referenced orphans and attachment recovery. "Keeping a
mapping alive is insufficient unless its allocator state, object references and committed roots are
also recoverable." Report blockers against individual acceptance gates, not the whole project.

## 2. The mechanism and why

The store is a graph of generational handles: `Store` holds slabs of `DirNode`, `DirBlock`, `Inode`,
`TrieNode` and a `ChunkStore`, and a `Volume` holds handle roots into it (`root: Handle<DirNode>`,
`inode_root: Handle<TrieNode>`). Two mechanisms were considered.

- **Position-independent memory (rejected).** Put the whole store in the content object so recovery
  is a re-map with no serialization. Rejected because the store's types are not plain-old-data: a
  `DirNode` owns a `Box<str>` name and vectors of entries, an `Inode`'s body holds `Box<str>` and
  `Vec`, a `Body::Directory` holds a `Handle<DirNode>`. Making them raw bytes in shared memory would
  force every allocation — names, entry vectors, chunk metadata — to be shared-memory-native, a
  rewrite of the volume core. Not tractable, and not what the evidence favours.
- **Full-state image published into anchor-owned RAM (adopted).** Mirror the database's proven
  pattern (`crates/db/src/partition.rs` `to_snapshot`/`from_snapshot`, published into the anchor
  segment by `crates/db/src/replay.rs` `snapshot`): capture the store's logical, handle-free state
  into a flat image, serialize it canonically, publish the bytes into the anchor content object at a
  barrier, and on recovery read the bytes back and rebuild the store (re-establishing handles). The
  content object holds the durable image; the live store stays heap-backed and fast. This recovers
  "allocator state, object references and committed roots" because it recovers the *logical* state
  they encode — inode numbers, entries, bytes and roots — and rebuilds the handles faithfully, which
  is exactly what the database does with its slab-and-index records.

Content bytes travel *in* the image (copied), not by aliasing arena extents, for this first correct
version; an in-place refinement (content bytes resident in the content object, the image carrying
only metadata) is a later §4.2 efficiency step, recorded as its own gate.

The image is encoded with the workspace `Wire` codec (`crates/wire`), not the ad-hoc encoding of
`crates/vfs/src/derive.rs`, because a recovery image is read back from the content object after a
possible mid-write crash: it is external, possibly-corrupt input. `Wire` checks every length against
the remaining bytes before allocating and refuses a bad tag, a truncated body, a non-canonical value
or trailing bytes, so a corrupt image is a typed refusal, never a panic and never a silently-smaller
volume.

## 3. Slices

1. **Region over a shared object.** *(Landed `e012717`, `crates/mem`.)* `Region` can be backed by a
   `SharedObject` (restart-surviving RAM: Linux `memfd`, macOS `shm_open`) as well as a private
   mapping; the arena is agnostic to the backing (an `Extent` is a region index plus offset,
   position-independent). Gated `crates/mem/tests/shared_region.rs`: an arena over a shared region
   allocates and serves bytes, and content in a shared region survives the writing mapping being
   dropped — the memory-level shape of a daemon restart.
2. **Anchor content object.** *(Landed `fdb2ad2`, `crates/anchor`.)* `AnchorSegment` owns a content
   `SharedObject` the supervisor creates, holds across restarts, and hands off; a restarted daemon
   re-opens it from the handoff environment. Gated `crates/anchor/tests/anchor.rs`: content written
   through one incarnation survives the crash and is re-read by the "restart."
3. **Volume recovery image — capture.** *(Landed, `crates/vfs/src/recover.rs`.)* A faithful,
   handle-free `VolumeImage`: the volume's roots (prefix, name policy, epoch, inode counter, quota
   parameters, root number) and every inode in number order with its identity, attributes, home and
   body — a directory's entries by name and child number, a file's bytes read through the read path
   (inline and multi-chunk alike), a symlink's target. `Volume::to_image` is a read-only walk of the
   inode table; `VolumeImage::to_content`/`from_content` are the canonical `Wire` bytes, and
   `from_content` refuses a foreign magic, an unknown version, a truncation or trailing bytes with
   `VfsError::RecoveryIncomplete` (added, §4.8-cited). Gated `crates/vfs/tests/recover.rs` (8 tests):
   the roots, the tree and a hard link (one inode, two names), an inline file's bytes and mode, a
   256 KiB multi-chunk file's every byte in order, a symlink target, a bytes round-trip, a
   determinism gate (two images byte-identical), and a hostile-input gate (empty, all-zero,
   truncated, foreign-magic and trailing-garbage content all refuse without panicking). Refuses,
   rather than silently drops, a base-backed body or a whiteout (the base-plane recovery gate).

## 4. Owed, as individual gates

- **Image rebuild (`from_image`).** Reconstruct the store from a `VolumeImage` faithfully —
  inodes at their exact numbers via `trie::set`, directories rebuilt from their entries, file bytes
  re-established, roots restored — then a full recovery round-trip test (`to_image` → drop →
  `from_image` → observably identical volume, inode numbers included). The capture half is in; this
  is the next slice.
- **Daemon/content-object wiring.** The create path sizes and creates the content object
  (`AnchorSegment::with_content`); a barrier publishes the image into it; `init_shard` reads it back
  and rebuilds via `from_image`. Then the process-level proof: write bytes through the client, kill
  the daemon while the anchor survives, restart, read the same bytes.
- **Fidelity extensions.** CoW snapshots and clone lineage; referenced-but-unlinked orphans
  (§4.6 lifetime across a restart); base-backed volumes (a live base restored only through retained
  handles or validated source identity — "reopening a path alone cannot substitute another base");
  the live pressure source of a dynamic quota (re-supplied on recovery like the clock).
- **§4.2 reservation accounting.** The admission invariant against physically-backed capacity, the
  resource vector, and its typed refusals (tracked as BUG-1/2/3 in the ledger).
- **NFS durability gate.** `WRITE` currently returns `FILE_SYNC` (`crates/bridge-nfs`); once the
  image is published durably this becomes truthful for daemon-crash survival, but anchor RAM alone
  does not satisfy NFS's power-failure stable-storage contract (RFC 1813 §§3.3.7, 4.8) — recorded
  as a specific unrun gate, not blocking the storage foundation or virtio-fs work.
