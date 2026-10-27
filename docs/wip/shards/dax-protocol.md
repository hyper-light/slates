# Shards DAX over a slates backing: the protocol (Shards part 3; Shards D189, slates A-135)

Status: **agreed 2026-10-24; slates' side built and proven the same day** (A-163; a real guest on this Mac, 3 of 3).
Shards' half is a six-patch series with its owner for review: part 2's four, the testguest DAX routine, and the range
on the ask (written by slates, as the owner offered to write or review it). The Shards owner (the `g121` session, `gpu`'s owner) checked the five window behaviours
below against `gpu` at `f5dcff24`, whose fs device is unchanged since `b12ec23d`, and confirmed each. They accepted the
range on the ask and will write the device side of it. They asked for the testguest routine as a patch series on
`gpu` rebased on its tip, linted with `scripts/lint` on all 8 targets, never pushed by slates. They added two points,
recorded below: shmem budgets (D188) and VM saves (D176). This note is what both sides build to: what crosses the
stream, what each side guarantees, what a guest sees, and why. The evidence is listed at the end.

slates' side as built so far (2026-10-24), proven by use on macOS:
- the object (`crates/mem/src/dax_object.rs`);
- the descriptor answer, with its fill, bounds and refusals (`crates/server/src/shards/dax.rs`);
- the DAX offer at INIT and in attributes (`slates_bridge_core::DaxOffer`);
- ingest at FSYNC, FLUSH, RELEASE, READ, FORGET and the stream's end (step 3), a refused one answered with its errno;
- write-through at the volume's write boundary (step 4, `crates/vfs/src/shards_backing.rs`), and the overlay rules
  below ("What a guest sees").

- the testguest routine (step 5, in Shards' series) and a real boot on this Mac (step 6): 3 of 3, red-checked without
  ingest and without write-through (GAPS, BENCHMARKS).

## The decision in one paragraph

slates answers Shards' existing DESCRIPTOR ask with a descriptor of **one shared-memory object per attachment and file**.
The object is sized once, to the largest size a guest's file can have, and is never resized. It holds the file's bytes
at the file's own offsets and zeros everywhere else, and memory is used only for the pages someone touches. slates
writes every change of the file, made through any path, into the object before it acknowledges the change. So the
guest sees other paths' changes in its map at once, as memory. That is exactly how Shards' window already treats a
host file's page cache, so **no revoke is needed and no backend-to-device message exists**. The guest's own stores
through the map reach the volume when the guest syncs or closes the file, or when the file's backing ends. At those
points slates compares the object with the volume and writes what differs.

On the wire, one optional addition is proposed: the DESCRIPTOR ask may carry the range it asks for. Nothing else
changes.

## Why no revoke is needed

- **Shards' window maps a file's pages directly** (D189). A host-side write to a mapped host file is visible to the
  guest as memory, with nothing told to anyone. The window does no revoking. It only **follows a file's size**:
  - before a reply tells the guest a size, it re-reads the file's size by `fstat` (`Window::reconcile`);
  - its Linux watch says when another process cut the file short;
  - the hypervisor's fault marks a page it could not give.

  It then maps the file's pages up to the file's end, and its own zeros past it.
- **A slates object behaves like such a file whose size never changes.** Its size is fixed at the largest size a guest's
  file can have, so the window's follow machinery never has anything to do:
  - `fstat` always reports the same size;
  - no page is ever cut away under the mapping;
  - no fault ever comes from a missing page.
- **Coherence comes from write-through.** A change made through NFS, an SDK client, another attachment, a merge or
  the guest's own FUSE_WRITE is written into the object before it is acknowledged. A guest reading through its map
  afterwards sees it, with no window action at all.
- **Durability comes from ingest at the points a guest asks for it.** At FUSE_FSYNC, FUSE_FLUSH and FUSE_RELEASE of a
  node, and when its backing ends, slates compares the object with the volume and writes the difference into the
  volume as a copy-on-write change. The reply comes after that change is published. Every one of those points comes
  over the stream, so slates never needs to ask the device for anything.
- **The size is the only thing that would have needed a revoke, and the fixed size removes it.** An object sized to
  the file would have to grow and shrink with it:
  - a macOS shared-memory object can be sized only once (measured, below);
  - a Linux memfd whose size the peer may change can be shrunk under slates' own mapping, which faults the daemon.

  Sizing the object once, to the maximum, avoids both. It costs nothing until pages are touched (measured, below).

## What changes on the wire

**The DESCRIPTOR ask (control frame, ask 3)** keeps its 10 bytes and may append the range its mapping is for:

| bytes | field | meaning |
|---|---|---|
| 0 | ask | `3` (DESCRIPTOR), as today |
| 1..9 | node | the node's id, `le64`, as today |
| 9 | writable | `1` if the mapping is to be written, as today |
| 10..18 *(new, optional)* | foffset | the mapping's offset in the file, `le64` (`fuse_setupmapping_in.foffset`) |
| 18..26 *(new, optional)* | len | the mapping's length, `le64` (`fuse_setupmapping_in.len`) |

The ask is 26 bytes long with the range and 10 without. A backend that reads 10 bytes ignores the rest; Shards' own
server does (`answer` reads `1..9` and `9`).

**The answer is unchanged:** a `shards_ipc` message whose header is the kind byte and a big-endian `u32` length.
- **Kind 0** carries the descriptor by `SCM_RIGHTS` and an empty payload.
- **Kind 1** carries a `le32` errno and no descriptor.

**What the range buys** (slates works without it, at a cost):
- **Lazy fill.** With the range, slates fills only the ranges asked for, so memory follows what the guest maps.
  Without it, slates fills the whole file at the first ask.
- **Smaller ingest.** With the range, slates compares only the ranges asked writable at each sync. Without it, it
  compares the whole file.

## What slates guarantees

1. **One object per attachment and file.** Every DESCRIPTOR ask for a node is answered with a new descriptor of the same
   object.
   - Shards' window keeps one descriptor per file and maps each range from the descriptor its own ask returned. A
     fresh object per ask would map ranges of one file from different objects.
   - The object lives until the node is forgotten (FORGET to zero) or the stream ends.
2. **Its size is fixed and is the largest a guest's file can have.**
   - The size is `2^63 − 2 MiB`. That is a whole number of 2 MiB ranges, and every range a guest can map lies wholly
     inside it except the last one below `2^63`.
   - A change that would take a file with a live backing past that size is refused `EFBIG`. Only a hand-made sparse
     file can get there; a Linux guest's own files stop at `MAX_LFS_FILESIZE`, `2^63 − 1`.
   - **Linux:** a memfd sized at creation and sealed against growing, shrinking and further seals. **macOS:** a POSIX
     shared-memory object opened read-write under a random name, its name unlinked at once, sized once.
   - HVF maps a shared range only from a descriptor opened to write (M17), so the descriptor is always read-write,
     even for a mapping the guest asks to read only. The guest's stores reach only this copy; ingest (guarantee 5) is
     what gives them effect, and it runs only for an attachment that may write.
3. **Before answering an ask, the object holds the file's current bytes over the asked range.**
   - The range is filled from the volume on its first ask, through slates' own mapping of the object, never by
     `write(2)` (see (e) below). A hole needs no fill, because untouched pages read as zeros.
   - Without the range, the whole file is filled at the first ask.
4. **Every change of the file through any path reaches the object before it is acknowledged** (write-through), over the
   ranges filled so far. An unfilled range gets the latest bytes when it is first asked for.
   - **Bytes past the file's end stay zero.** A truncate zeroes what it cuts. An extension zeroes the hole it opens,
     so a store the guest made past the old end is never taken for data.
   - On macOS only pages that hold something are zeroed: `mincore` says resident (`MINCORE_INCORE`) or paged out
     (`MINCORE_PAGED_OUT`, a compressed or swapped page still holds its bytes). On Linux the hole is punched through
     slates' own mapping (`MADV_REMOVE`; `fallocate` is a file write, allowed only in `slates-land`, R1). Either way
     slates never commits fresh memory to zero headroom.
5. **The guest's stores reach the volume at its sync points**, in an attachment that may write; a read-only
   attachment's object is never compared. At FUSE_FSYNC, FUSE_FLUSH and FUSE_RELEASE of a node with a backing, and when
   the backing ends:
   - slates compares the object with the volume up to the file's end, over the ranges asked writable (the whole filled
     extent when asks carry no range);
   - it writes what differs as one copy-on-write change, in slices under the shard's per-turn budget;
   - it answers after that change is published.

   At overlapping bytes, the later write wins, as with the mount route (A-139/A-140).
6. **Reads for a node with a backing come from the object** over filled ranges, so a FUSE_READ never contradicts the
   guest's own unsynced stores. Linux sends none for DAX files; this covers a guest that does.
7. **Confinement (A-135).** The descriptor reaches one file's bytes and zeros, and nothing of any other file or volume.
8. **slates never waits on the device.** No change through any path waits for a VM, and no slates resource is held
   hostage by one.
9. **DAX is offered only where slates can back it.**
   - The FUSE_INIT reply offers per-inode DAX (`FUSE_HAS_INODE_DAX`, and `FUSE_MAP_ALIGNMENT` at the host's page
     size) when the guest asks for it.
   - `FUSE_ATTR_DAX` is set on regular files of at least one 2 MiB range, re-decided at every attribute reply as
     Shards' own server does. That holds in read-only attachments too, whose objects take changes through
     write-through and are never ingested.
   - Fills are counted against a derived budget on the memory they take. New files stop being offered DAX before the
     budget is reached, and a fill past it is refused `ENOMEM`, typed and counted.
   - A guest mounted `dax=always` maps every file whatever the server says (Linux `fs/fuse/dax.c:1298-1318`).
     `dax=inode` is the mode slates' choices steer; under `dax=always` the budget can only refuse.
   - **Whose budget a page lands in (Shards D188), from the Shards owner.** On Linux a memfd's pages are shmem,
     which nothing reclaims without swap, charged to the memory cgroup of whoever touches each first. slates' fills
     land in slates' own cgroup. A guest's first store into a page slates never filled (a hole, or past the file's
     end) lands in the run's cgroup, which Shards' daemon counts as no room, since its budget test pins shmem as not
     room. A budgeted run therefore holds its stores to holes until slates lets the object go, when the node is
     forgotten or the stream ends. Shards will record this in D188 and D189 when this lands.

## Windows (plan G-shards-windows step 5, agreed with the Shards device's author 2026-10-26)

Windows passes no descriptor over a stream, so the object goes to the VM process by handle duplication, and the ask
says where.

**The ask** is the 26-byte ranged ask, then two fields: 38 bytes after the length word. Windows accepts no shorter form.

| bytes | field | meaning |
|---|---|---|
| 26..30 | pid | the asking process's id, `le32` (`GetCurrentProcessId`) |
| 30..38 | stream | the value, in that process, of its handle of the stream, `le64` |

**The answer** keeps the `shards_ipc` framing: kind 0 with an 8-byte payload, the section's handle value in the asking
process (`le64`); kind 1 with the errno (`le32`, Linux numbers), as on Unix.

**The check before anything is handed.** The daemon opens the named process, duplicates the named handle into itself,
and requires it to be the same kernel object as a known holder's handle of this stream (`CompareObjectHandles`). The
known holders are the attaching client, whose handle the attach duplicated, and the last claimant that passed. Any
other answer is refused `EACCES`, and nothing is put into the named process: another pipe, no pipe, a value naming
nothing, a process gone or of another user. The pipe allows one instance, already connected, so "the same object" and
"a handle of this pipe's name" refuse and grant the same claimants. The check is by object because every name query
waits on a synchronous handle's lock: measured on jupiter (Windows 10.0.26200, 2026-10-26), `FileNameInfo`,
`NtQueryObject(ObjectNameInformation)` and `FileModeInformation` each hung past 3 s on a synchronous pipe with a read
pending. `CompareObjectHandles`, `GetFileType`, the object's type, and closing the duplicate each answered at once. So
the attaching client keeps its copy of the stream until its child exits (step 2).

**The object** is an unnamed pagefile-backed section under `D:P(A;;RC;;;OW)` (A-175's confining descriptor). It is
reserved (`SEC_RESERVE`) at the machine's physical memory in whole ranges (`slates_mem::dax_object_bytes`), not at
`2^63 − 2 MiB`, because a section's making costs in proportion to its size, reserved or not. Measured on jupiter the
same day: 2^48 bytes took 3.3 ms and 12,001 pages of commit to make, 2^53 took 129 ms and 358,807 pages, and 2^62 or
more was refused `ERROR_NO_SYSTEM_RESOURCES`. A RAM-only volume's file holds no more bytes than the machine has, so
only a hand-made sparse file can pass the size; a change taking a backed file past it is refused `EFBIG`, as on Unix
past `2^63 − 2 MiB`. A reserved page faults every view until it is committed, so each range an ask covers is committed
whole before the answer, past the file's end too. A commit belongs to the section, so the device commits nothing.
The duplicate maps read alone, or read and write for a writable ask, and may query its size (`SECTION_QUERY`). The
descriptor refuses any duplicate of more, a rewrite of the descriptor included.

## What Shards' device does

**Nothing new is required.** The design rests on behaviour the device already has, each confirmed by its owner on
2026-10-24:
- **(a)** A descriptor is asked for at every FUSE_SETUPMAPPING (`mod.rs:719-725`).
- **(b)** Each range is mapped from its own ask's descriptor. The node keeps the first descriptor, or the first
  writable one, for reconcile, the watch and the record of written ranges (`window.rs:142-150`, `:560`).
- **(c)** A mapped range changes again only by:
  - following its file's size: `reconcile`, the daemon's watch saying it was cut (`daemon/cuts.rs`), or
    `memory_fault`;
  - the guest mapping over it again, which includes upgrading a read-only range to writable and asks again;
  - FUSE_REMOVEMAPPING;
  - the device's reset, which unmaps everything;
  - a restore (D176), where `Slot::attach` asks again for every saved mapping and maps zeros where an ask is refused.
- **(d)** Nothing maps a new range of a file without asking. SETUPMAPPING and attach are the only two paths that map,
  and both ask.
- **(e)** The Linux watch tolerates a memfd, with a cost. The daemon watches each mapped node's descriptor with
  inotify `IN_MODIFY` on its `/proc/self/fd` path (`cuts.rs`, `watch_writes`). For a memfd that is its shmem inode.
  - A size that never shrinks never tells a cut.
  - Every `write(2)`, `pwrite` or `fallocate` on the object fires `IN_MODIFY` and wakes the daemon to `fstat` it.
  - Stores through a mapping fire nothing.

  So slates fills and writes through **its own mapping only, never by `write(2)`**, which costs the watch nothing.
  Linux's hole punch (guarantee 4) is a `madvise(MADV_REMOVE)`, which the kernel carries out as a `vfs_fallocate`
  that fires the event (`mm/madvise.c:1052-1053`, `fs/open.c:340-348`). That is one wakeup per truncate or
  extension of a mapped file. macOS has no such watch.

**The range on the ask: agreed.** The Shards owner writes the device side: `descriptor_on` takes the range,
`mapping()` passes the request's, and attach passes each saved mapping's. A test will show their own server ignores
the extra bytes.

## What a guest sees

The same as through the mount route, measured there in `docs/wip/virtiofs.md`:
- Other paths' acknowledged writes appear in the guest's map at once.
- The guest's stores reach other paths when it syncs or closes the file, as a host page cache's write-back would.
- At overlapping bytes, the later write wins.
- **A VM that dies with unsynced stores:** its stream ends, and slates ingests every backing first, so the stores land
  in the volume. The mount route does the same (a VM killed with an unsynced store left it in the host's cache, which
  wrote it back).
- **A daemon that dies:** the stream dies with it, as every Shards attachment does (A-161: nothing survives a restart).
  The guest's unsynced stores in its maps are lost, and its next sync fails, so it knows.
- **A detached or revoked attachment:** the stream ends after a final ingest, and slates writes through no longer. The
  VM keeps the bytes it already maps, which it was allowed to read, and its writes reach nothing.
- **A sync the volume refuses** (no room, a file drifted beneath an overlay): its FSYNC or FLUSH answers the errno, so
  `fsync(2)` or `close(2)` fails; the stores stay in the map, and the next sync tries what still differs.
- **A file beneath an overlay that no one has written** follows the disk (D-25). An outsider's edit reaches a map of it
  when slates sees the edit: before the reply to the guest's next request that could show it (a GETATTR's new size,
  say), the map is refilled, a copy slice a turn. Until that request, as with a guest's cached pages, the map holds the
  bytes it had. A **writable** ask of such a file copies it up first, as its first write would. From then on the
  outsider's edit is drift that `status` reports, never taken for the guest's stores. That ask can be refused with the
  copy-up's errno.
- **A file an in-process guest maps writable** (slates' own virtio-fs device) is refused `EBUSY`, and the other way
  round: those stores go to the file's block past the write boundary, where no object could follow them.
- **A saved VM (Shards D176), from the Shards owner.** slates refuses SAVE and RESTORE today, so no snapshot of a VM
  holding a slates share exists. If they are served one day, the window's memory is not in the snapshot. Stores made
  after the save and before the stream ends are ingested, so a VM restored from that snapshot sees them, as a host
  file under DAX keeps the writes made after a save. That is consistent, and stated here so nobody expects
  otherwise.

## What a misbehaving peer can do

A device or guest that breaks these rules can only hurt itself:
- It can lose its own stores, by writing after it unmapped, after a FORGET, or after the stream ended.
- It can read or write zeros, or its own junk, in its own object past the file's end.

It cannot reach another file or volume (guarantee 7), cannot make slates wait (guarantee 8), and cannot shrink an
object under slates' own mapping:
- **Linux:** the memfd is sealed against resizing.
- **macOS:** a shared-memory object cannot be sized twice.

Memory a guest touches is charged to the VM process:
- **macOS:** to the toucher (measured, below).
- **Linux:** to the memory cgroup of the task that first touches each page.

slates' own fills cover only bytes the volume references, since a hole needs none, so they are at most one copy of
the volume per attachment, counted against the derived budget of guarantee 9.

## Costs

| cost | estimate, or measurement where one is recorded |
|---|---|
| Memory | One copy of each range the guest has asked for, held until the node is forgotten or the stream ends: at most one copy of the volume per attachment (slope 1.0 over mapped bytes, the cost `docs/bugs/2026-10-18-…` measured and rejected for the in-process device, where zero copy was possible). |
| Time: fill | At most one 2 MiB copy per range, at its first ask. |
| Time: write-through | One copy of the changed bytes per live backing: measured 117 µs per MiB per backing on an M5 Max, 3.7 times the write itself (BENCHMARKS, "What write-through adds to a write of a mapped file"). |
| Time: ingest | One comparison over the ranges asked writable, per sync point: measured 0.70–1.14 ms per mapped MiB on an M5 Max (BENCHMARKS, "What a guest's FSYNC costs when its map is ingested"). |

## Rejected, with the reason

- **A revoke protocol.** It would need a doorbell descriptor, REVOKES and REVOKED asks, and a stage-2 unmap and remap in
  the device. It was the plan's first shape. Rejected for three reasons:
  - write-through makes another path's change visible with no revoke, as host files already are;
  - a revoke makes every change through another path wait on a VM, so a hung VM stalls or refuses writers;
  - it adds state to the device's fault path.
- **An object sized to the file, resized with it.** macOS refuses a second `ftruncate` (EINVAL, measured). On Linux a
  peer allowed to resize can shrink the object under slates' mapping (SIGBUS in the daemon).
- **Doubling capacity, with a swap at growth.** A swap must stop the guest's stores before the content moves: a stage-2
  unmap, and a fault path that waits. That is new state in Shards' most delicate code, and the sparse maximum-size
  object makes it unnecessary.
- **A fresh copy per ask.** Ranges of one file would map different objects (see guarantee 1), and a remapped range
  would lose the guest's unsynced stores.
- **Zero copy, handing slates' content itself.** Refused by A-135: a descriptor authorizes its whole object, and the
  content arena holds every volume. A per-file content object would end block sharing across files and volumes
  (clones, snapshots). That is what zero copy would need, recorded here.
- **A Linux udmabuf over the file's blocks in the arena.** This is zero copy and confined, and slates' in-process
  device uses it to hand out one window
  (`docs/bugs/2026-10-18-the-message-based-dax-map-hands-the-vmm-the-whole-arena.md`, option 4). It does not fit
  this route, for four reasons:
  - Shards' window maps any range of a file at its file offset from one descriptor per file, which it also `fstat`s
    for the file's size. A udmabuf covers a run of arena pages, not a file's layout.
  - A copy-on-write write moves the file's bytes to new blocks, so the pinned pages go stale and a revoke would be
    back.
  - macOS has no counterpart.
  - `/dev/udmabuf` access depends on the host's configuration, and each buffer is limited to 64 MB by default.

## Evidence

**Shards, `gpu` at `b12ec23d`.** The fs device is unchanged through `gpu` `4fc9cb98f` (2026-10-24):
`git log b12ec23d..4fc9cb98f -- crates/vmm/src/devices/virtio/fs/` is empty.
- `crates/vmm/src/devices/virtio/fs/window.rs`:
  - `:142-150`: one descriptor per file, kept from the first ask (or the first writable one);
  - `:540-560`: each range mapped from its own ask's descriptor;
  - `:796-830`: `reconcile` follows a file's size by `fstat`;
  - `:847`: `memory_fault`;
  - `:65`: `SHARED_NEEDS_WRITE` on HVF.
- `mod.rs`:
  - `:719-725`: a descriptor asked for each FUSE_SETUPMAPPING;
  - `:201-217`: the 10-byte DESCRIPTOR ask;
  - `:380-398`: Shards' server reads the node and the writable byte only.
- `crates/ipc/src/unix.rs:333,379`: the message header (kind, then a big-endian length).

**Linux master `fs/fuse` and `mm/shmem.c`, fetched 2026-10-24.** sha256: `dax.c` `e3980540…`, `file.c` `6991f44f…`,
`inode.c` `ac0a4500…`, `shmem.c` `b4f1a43e…`.
- `file.c:1838`, `:1859`: a DAX file's reads and writes go through `fs/fuse/dax.c`, never FUSE_READ.
- `dax.c:739-745`: only a file-extending write goes as FUSE_WRITE. Shards' kernel patch 0009 also sends whole pages its
  host does not hold, which slates handles as any FUSE_WRITE.
- `inode.c:151-162` with `dax.c:232-248`: an evicted DAX inode's mappings are removed by waited FUSE_REMOVEMAPPING
  requests before its FORGET is queued, so at FORGET the device maps none of it.
- `inode.c:1605` and `shmem.c:5249`: `MAX_LFS_FILESIZE` bounds a FUSE file and a memfd alike. A Linux run of the memfd
  sizing is owed in step 2.
- `dax.c:1298-1318` with `inode.c:1358-1363`: under `dax=inode` a file is DAX only if the server's INIT reply said
  `FUSE_HAS_INODE_DAX` and its attributes say `FUSE_ATTR_DAX`; under `dax=always` every file is, whatever the server
  says.
- `shmem.c:3855`: punching a hole in a memfd is refused only under its write seals, so the resize seals leave it to
  slates.
- macOS SDK `sys/mman.h:226-233`: `mincore` marks a page resident (`MINCORE_INCORE`) or paged out
  (`MINCORE_PAGED_OUT`).
- Linux master `mm/madvise.c:1052-1053` and `fs/open.c:340-348` (fetched from kernel.org 2026-10-24; sha256
  `e9df6ee4…`, `a5c3fa10…`): `MADV_REMOVE` is carried out as `vfs_fallocate(PUNCH_HOLE | KEEP_SIZE)`, which fires
  a modify event on success even when the size is unchanged.

**The Shards owner's answer** (the `g121` session, 2026-10-24, by message). It confirmed (a)–(e) against `gpu`
`f5dcff24`, gave the cost of (e), accepted the range, asked for the testguest series, and added D188 and D176.

**Measured on this Mac** (Apple silicon, macOS 26.4.1, 2026-10-24). Each probe is a `python3 -I` script in
`~/.cache/slates-scratch/probes/`, RAM only, which unlinks its object's name at once.
- `shm_max.py` sized objects of `2^40` to `2^63 − 2 MiB`. Every size was accepted, and the last 2 MiB of each mapped,
  took a byte and read it back.
- `shm_capacity.py 68719476736` sized and mapped a 64 GiB object and touched one byte. The process footprint went from
  12 MB to 13 MB. A second `ftruncate` was refused: errno 22.
- `shm_charge.py 268435456`: a parent created and sized a 1 GiB object, and a forked child touched 256 MiB of it.
  The child's footprint was 259 MB and the parent's 13 MB.

Step 2 turns these platform facts into Rust tests by use.

**slates.**
- `docs/bugs/2026-10-18-a-read-handed-the-peer-the-whole-content-arena.md` (A-135).
- `crates/server/src/shards.rs:774`: the DESCRIPTOR ask is refused `EOPNOTSUPP` today.
- `docs/wip/virtiofs.md`: the mount route's semantics this matches.

## How it will be proven

- **slates** (steps 2–4). A test plays Shards' device over a real daemon's stream:
  - asks with and without the range;
  - maps the answer and checks the bytes and the size;
  - plants a marker in another volume and another file, and finds it zero times in the object, after first finding it
    in the content arena;
  - stores through the map, sends FSYNC, and reads the bytes back through `slates read`;
  - writes through another path and reads the change through the map;
  - truncates and extends over junk;
  - sends FORGET, and ends the stream.

  Hostile asks are each refused with an errno and no descriptor: a node the attachment does not reach, a directory, a
  truncated ask, an overflowing range.
- **Shards** (step 5, a patch series as in part 2, unless the owner prefers to write it). A testguest routine:
  - mounts `shards0` with `dax=inode` and checks that a file of at least 2 MiB is DAX;
  - stores through its map and fsyncs, then checks the host's `slates read`;
  - lets the host write through another path, then reads the change through the same map.
- **The real boot** (step 6): that routine under `slates shards` on this Mac, 3 of 3. **Done 2026-10-24**: 3 of 3, the
  synced store in `slates read` 261–581 ms after the guest's word. With the owner's ranged ask (`gpu` 6534aee8, which
  replaced slates' patch 6), it took 75–80 ms, 3 of 3, on the five patches rebased onto it.
  - Red-check without write-through: the guest reads stale bytes. Seen: it never saw the change; the test failed at
    its 90 s bound.
  - Red-check without ingest: the host reads old bytes. Seen: the original pattern where the store should be.
