# Anchor-owned volume storage and recovery (§4.2, §4.8): milestone design

> Status: in progress. The recovery **mechanism** is complete and gated end to end: capture, faithful
> rebuild, per-shard framing, and — landed now — the **daemon wiring**, so a volume is recovered from
> its image in anchor-owned RAM across a real daemon restart (the client restart test). What remains
> is the **data-plane content barrier** and the process-level content-bytes proof, which are blocked
> on a mount (FUSE/NFS need host capabilities — Ada's step-4 gate; the control client has no file
> I/O), and the fuller **§4.2 admission accounting** (content-object sizing is a first, derived cut).
> This doc is the assistant-owned record; the numbered requirements live in `SLATES_DESIGN.md` §4.2
> and §4.8 (A-9), and the gap ledger is `GAPS.md`.

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
   Directory entries are captured in a canonical (name-sorted) order, so the image is independent of
   the small/indexed directory representation and a rebuild re-captures byte-identically.
4. **Volume recovery image — rebuild.** *(Landed, `crates/vfs/src/recover.rs` `from_image`, with
   `Volume::recovery_shell`/`VolumeSeed` in `volume.rs`.)* The store is rebuilt from a `VolumeImage`
   faithfully: every inode is placed at its own number (`trie::set`) with its identity, attributes,
   home and body; directory nodes are rebuilt with their parent and name fixed from the reaching
   entry; file bytes are re-established through the volume's own `write` path, so the chunk store and
   the quota/byte accounting end where a live write would leave them. The whole rebuild runs at the
   image's head epoch so nothing copies-on-write while it is built; each inode's true birth epoch and
   version are restored last. A dynamic quota is refused for now (its live pressure source is not in
   the image). Gated by the oracle `to_image(from_image(img)) == img` (byte-identical) plus a read of
   the multi-chunk file through the inode number handed out before the "restart" — the
   write→[image]→[drop]→[rebuild]→read proof at the library level. Non-vacuity of the oracle was
   checked by injecting a dropped-attribute-restore bug and confirming the round trip then fails.
5. **Content-object framing and handoff survival.** *(Landed, `crates/vfs/src/recover.rs`
   `write_to`/`read_from`.)* An image is published into a content-object buffer behind a small frame
   — a little-endian byte length and a CRC-32C of the image bytes — so a restarted daemon finds it,
   an empty (fresh) object reads as `None` (nothing to recover, start a new volume rather than fail),
   and a write torn by a crash is caught by the CRC and refused with `RecoveryIncomplete` rather than
   decoded to garbage (§4.8: never an empty success). Gated `crates/vfs/tests/recover.rs`: a volume
   survives a real `SharedObject` **handoff** — the memory-level shape of a daemon restart with the
   anchor surviving (slice 1's property) — its bytes read back through their original inode number
   after the writer's mapping is dropped, and the frame signals empty, reads back, refuses a torn
   write and refuses too small a buffer. This is Ada's step-two proof at the library level, through
   the actual restart-surviving primitive; only the server/client process lifecycle is left to wire.
6. **Whole-shard image.** *(Landed, `crates/vfs/src/recover.rs` `ShardImage`/`KeyedImage`.)* A shard
   holds many volumes in one store and has one content object, so it publishes them all together: a
   `ShardImage` is the volumes in key order, each a `KeyedImage` pairing the volume's image with an
   opaque `u64` routing key its owner (the server) files it by — vfs does not interpret the key, so
   the format is layering-clean. Same framing (`write_to`/`read_from`, shared with `VolumeImage` via
   `frame`/`unframe`) and a distinct magic so a shard image is never decoded as a single volume.
   Gated: two volumes with distinct prefixes survive a content-object handoff together, recovered by
   key and each rebuilt faithfully. This is the format the daemon wiring needs, so that wiring is one
   correct integration rather than a single-volume version later replaced.
7. **Daemon/content-object wiring.** *(Landed, `crates/server`, `crates/anchor`, `crates/cli`.)* The
   anchor creates one content object sized `partitions × reserve_per_shard` (lazily backed, so the
   unused tail costs address space, not RAM) and hands it off with the segment; `SegmentSource`
   carries the content handoff so a daemon that attaches by handoff (the restart test) adopts it too.
   `init_shard` opens the object and takes this shard's slice (`ShardState.content`/`content_range`).
   A control mutation that changes the volume set or roots (create, clone, resize, destroy) republishes
   the shard's `ShardImage` into its slice (`publish_shard`); `rebuild_recovered` reads it back and
   rebuilds each recovered volume through `from_image` (its prefix, tree and content restored — fixing
   BUG-11's empty recreate and prefix reassignment), refusing `RecoveryIncomplete` for a db volume with
   no image rather than presenting it empty. Gated by the existing client restart test, now threading
   the content object: the volume is recovered from its image across a real daemon restart.

## 4. Owed, as individual gates

- **Data-plane content barrier and the process content-bytes proof.** File content is written through
  the bridge (FUSE/NFS), not the control client, so a barrier there must `publish_shard` after a
  content write, and the end-to-end "write bytes → kill → restart → read the same bytes through the
  client" proof needs a mount — which needs host capabilities (Ada's step-4 gate). The content-bytes
  survival itself is already proven at the library level (through a real `SharedObject` handoff); what
  the mount adds is the last process hop. Until then the control path proves catalog, roots and prefix
  recovery across a real restart.
- **§4.2 admission accounting and content-object sizing.** The object is sized at a derived
  `partitions × reserve_per_shard`; the fuller §4.2 admission invariant (physically-backed
  entitlement, the resource vector, typed refusals) and a tighter, non-doubling size (content resident
  once via `Region::shared` rather than copied into the image) are owed (BUG-1/2/3 and the in-place
  refinement).
- **Crash-during-publish (double buffering).** The single-slot frame detects a torn write but does
  not keep the prior good image across one; the database's two-slot, sequence-numbered publish
  (`replay.rs`) is the pattern to adopt so a crash mid-publish recovers the last complete image.
- **Snapshot recovery.** *(Landed.)* `to_image`/`from_image` capture and rebuild every CoW snapshot
  with the tree frozen at it; ids (slot + generation) are reproduced so a `SnapshotId` a client held
  still resolves, and metadata (referenced_bytes, seq, links, head pointer) is restored. The daemon
  publishes after `Snapshot` and keeps recovered snapshots in `reconcile_lost`, so a snapshot
  **survives a real daemon restart** (the client restart test asserts survival, not reconciliation).
  Gates: a recovered snapshot's tree is independent of the head's (CoW *sharing* is a §4.2 efficiency
  refinement) and its deadlist is empty (reclaiming its unique bytes on drop is owed) — neither a
  content-correctness issue.
- **Clone recovery.** *(Content landed.)* A clone's image captures its whole tree (the bytes it
  inherited from the origin snapshot and the bytes it wrote after diverging), and `from_image`
  rebuilds it faithfully, keeping the inherited root inode number (fixed in
  docs/bugs/2026-09-06-clone-recovery-root-number.md) and restoring the origin epoch. Owed: the O(1)
  *sharing* between a recovered clone and its origin (a §4.2 efficiency refinement, not content), and
  a process-level clone-across-restart test through the daemon.
- **Other fidelity extensions.** Referenced-but-unlinked orphans (§4.6 lifetime across a restart —
  though a restart drops the open handles that pinned them, so they are correctly not recovered);
  base-backed volumes (a live base restored only through retained handles or validated source
  identity — "reopening a path alone cannot substitute another base"); the live pressure source of a
  dynamic quota (re-supplied on recovery like the clock).
- **§4.2 reservation accounting.** BUG-2 (admit against usable arena capacity) and BUG-1 (a strict
  volume locks its RAM or refuses) are landed. Still owed: BUG-3 (dynamic growth must consult the
  live shard budget, not machine RAM — needs the vfs↔server write-path coupling, Phase-4-exercised),
  the resource vector (per-volume inode/namespace/xattr allowances), and the non-doubling in-place
  size (`Region::shared` backing the arena rather than copying content into the image).
- **NFS durability gate.** `WRITE` currently returns `FILE_SYNC` (`crates/bridge-nfs`); once the
  image is published durably this becomes truthful for daemon-crash survival, but anchor RAM alone
  does not satisfy NFS's power-failure stable-storage contract (RFC 1813 §§3.3.7, 4.8) — recorded
  as a specific unrun gate, not blocking the storage foundation or virtio-fs work.
