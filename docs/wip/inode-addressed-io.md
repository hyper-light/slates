# Inode-addressed shared I/O: interface and lifetime design

> **Status: accepted in principle, revised on Ada's review (2026-09-05).** Ada agreed with
> inode-addressed I/O and deferred reclamation and gave the corrections now folded in below
> (authority, open-file semantics, reference ownership, the sillyname guarantee, the generation
> tests). Step 1 — the volume-core reference count and deferred reclamation (§9), with the
> hardening of Ada's point 3 — has landed (`docs/bugs/2026-09-05-drop-link-open-reference.md`), and
> so has step 3: the local attachment authority (real generation-checked records, epoch fencing,
> revocation — no placeholders or unconditional validators), the **whole** `Bridge` trait moved
> onto `(ObjectId, &OpContext)` with rights/view/volume enforced at the seam once for every
> transport, and both transport edges carrying the context (FUSE `dispatch`/`serve_blocking`, the
> NFS `Export` admitting its attachment at mount time). Step 4's NFS surface is complete: the
> metadata, file-I/O, namespace and directory-listing procedures all run over the interface —
> `GETATTR`/`SETATTR`, `LOOKUP`/`ACCESS`, `READ`/`WRITE`, `CREATE`/`MKDIR`/`SYMLINK`/`READLINK`,
> `REMOVE`/`RMDIR`/`RENAME`, `READDIR`/`READDIRPLUS`, `FSSTAT`/`FSINFO` — each seam-enforced against
> the export's rights, view and volume (`7c4480a` … `0bbe097`). The daemon wiring of
> the FUSE mount's attachment, per-attachment reference ownership with the teardown sweep, synthetic
> `.`/`..`, and the orphan's survival across a daemon restart are the remaining work. On completion
> this becomes amendment A-10 applied to §4.5 (inode lifetime), §4.6 (the Bridge trait) and §4.13
> (authority).

## 1. The decision and why

The shared operation layer (`slates-bridge-core`, §4.6 "one VFS operation layer") today keys
`read`/`write` on an *open handle* the way the first, FUSE-shaped implementation did: `open`
returns a small integer, the kernel echoes it, and `VolumeBridge` maps it back to an inode through
a per-bridge table. NFS is stateless — a `READ` carries a file handle that names its object by
identity, with no prior `open` — so it cannot supply that handle. virtio-fs (FUSE-over-virtio) and
a future FSKit path have the same operation set as FUSE but are still transports over the one
layer.

The design already fixes the addressing model: **"Every request carries `(volume handle, inode no,
gen)`"** (§4.6). So the shared I/O operations identify the object by that tuple, carry the
authenticated attachment/view context, and take an explicit offset. NFS calls it directly; FUSE and
virtio-fs translate their requests into the same calls.

**Object lookup loses its handle indirection; I/O still consults the necessary open authority and
intent.** The identity tuple says *which object* a request addresses; it does not authenticate the
caller, resolve the view, count references, or carry the open descriptor's granted access and
append intent. Those remain, and are specified here. This note removes a redundant indirection
(handle → inode for read/write) only after accounting for every responsibility that indirection was
adjacent to.

## 2. The interface

```rust
/// The object a request addresses (§4.6). The volume is the attachment's own; an implicit volume
/// is fine, but the context (§below) is checked against it. Identity survives copy-on-write (§6).
struct ObjectId { inode: u64, generation: u64 }

/// The authenticated attachment/view context a request carries. Constructed from trusted server
/// state — never from anything a caller declares — and checked against the addressed volume.
struct OpContext {
  /// The generation-checked attachment this request rides (a mount, an NFS export, an FSKit
  /// volume, a virtio-fs device); a superseded attachment's request is refused before any effect.
  attachment: AttachmentRef,
  /// The fencing epochs the request must match — the owner's host epoch and the attachment
  /// generation (§4.8); a stale epoch is refused, not applied.
  authority: AuthorityStamp,
  /// The view resolved for this operation *from the attachment*: the volume's current head or a
  /// pinned immutable version (§4.16 `advance` re-pins). A write against a pinned view is `EROFS`.
  view: PinnedView,
  /// The enrolled consumer and its mapped POSIX credentials (§4.13). `AUTH_SYS` uid/gid alone do
  /// NOT authenticate a consumer — RFC 5531 §14 records that the flavor is unverified — so the
  /// subject is the server's enrolled identity; the credentials are an advisory mapping.
  subject: AuthenticatedSubject,
  /// The validated I/O authority for this operation: the open descriptor's *granted-at-open*
  /// access (FUSE) or the per-request access (NFS). A write's authority is what was granted, not a
  /// re-derived check of the current mode (§4).
  io_authority: IoAuthority,
}

trait Bridge {
  fn read(&mut self, object: ObjectId, cx: &OpContext, offset: u64, size: u32, out: &mut Vec<u8>)
    -> Result<(), VfsError>;
  fn write(&mut self, object: ObjectId, cx: &OpContext, offset: u64, data: &[u8])
    -> Result<u32, VfsError>;
  // getattr/lookup/... likewise take ObjectId + &OpContext instead of a bare inode.
}
```

`read`/`write` no longer take an open handle. **Rights are derived from the attachment, never
declared by the caller**: the server builds `OpContext` from its own attachment/authority records,
so a request cannot assert an authority it was not granted. The object identity is authoritative
for *which* object; the context is authoritative for *whether* and *as what view/authority*. The
offset is explicit (it always was). This is exactly what an NFS `READ`/`WRITE` supplies (a file
handle → `ObjectId`, the server's export/credentials → `OpContext`, an explicit offset), and what
FUSE supplies (the request's node id → `inode`, the mount's attachment → `OpContext`, the request's
offset).

**What this deletes:** the `read`/`write` dependence on a handle table to find the inode. The FUSE
read/write path already carries the node id (the inode) in every request, so it loses nothing.

**What this keeps (see §3–§5):** `open`, `create`, `release`, `flush`, the per-inode reference
count, and the per-open state (granted access, append intent, lock owner) — because the identity
tuple carries none of it.

## 3. The reference model (the lifetime the identity tuple does not carry)

Each inode has a reference count separate from its hard-link count:
`references = lookup_references + open_references`.

- **`lookup_references`** — held by a transport handed the object that may address it later. FUSE
  takes one per `LOOKUP`/`CREATE`/`MKDIR`/readdirplus entry and drops `n` on `FORGET(inode, n)`.
  **FUSE does not guarantee an individual `FORGET` for every reference**: an unmount can discard a
  mount's outstanding lookup references without per-inode messages. So the model must release a
  whole attachment's lookup references in one bounded sweep at teardown, not only on explicit
  `FORGET`.
- **`open_references`** — one per live `open`/`create` result, dropped on `release`.

**Reference ownership and cleanup.** Every reference is owned by an attachment, recorded so that
the reference set is releasable per attachment. Disconnect, cancellation, a failed reply, and
daemon restart each have a defined release/recovery: a lost attachment's references are swept
(bounded batch) at its teardown; an in-flight operation pins its object for the operation's
lifetime and releases at its terminal step (cancellation-safe, §3 of CLAUDE.md); a restart either
hands the reference set across through the anchor or reconstructs it from the reconnecting
attachments, and an orphan with a live holder must survive that handoff (§4.8). Mapped ranges
(DAX/mmap) pin like an open reference; snapshot storage retention is a *separate* concern (a
snapshot pins content whether or not anything is open). **Open-reference counts alone do not
establish a writeback barrier** — a barrier additionally drains in-flight and pending writes (§5).

**Reclamation rule.** An inode's content and table entry are reclaimed only when **both**
`nlink == 0` and `references == 0`. Until then an unlinked-but-referenced inode is *unlinked*
(invisible to name lookups) but *alive* (its content served to holders of its identity). `drop_link`
at `nlink == 0` defers when referenced (records a per-volume orphan, keeps it addressable) and
reclaims otherwise; the last `unreference` runs the deferred reclamation at its terminal step,
dropping the orphan record only after reclamation succeeds so a failure retains the obligation.

**Hardening (landed, step 1).** `Volume::reference` validates the inode exists (so the map cannot
grow past the inode table's cap) and uses checked arithmetic; `Volume::unreference` is failure-safe
as above. Charging references against §4.2 admission and the per-attachment ownership records are
owed with the interface change (steps 3–4).

**Reference-taking is a per-transport action (landed).** The shared operation layer no longer
references implicitly: `Bridge::reference` is an explicit verb the FUSE edge calls after a
successful `LOOKUP`/`CREATE`/`MKDIR`/`SYMLINK`, and the NFS edge never calls (NFS adds no
references, above). This closed a lookup-reference leak in the first NFS surface, where the shared
`lookup`/`create`/`mkdir`/`symlink` referenced for every transport and NFS — having no `FORGET` —
never dropped them (`docs/bugs/2026-09-05-nfs-lookup-reference-leak.md`).

**Per-attachment ownership and the teardown sweep (landed, vfs + bridge halves).** Every reference
is attributed to the attachment that took it: the volume core holds a per-attachment ledger keyed
by an opaque owner id, whose invariant is that the global reference count equals the sum of the
per-attachment counts (`Volume::reference_for`/`forget_for`). `Volume::sweep_attachment` releases
exactly one attachment's share of each inode it held, in one bounded batch, reclaiming those that
reach zero references and no links — so a FUSE unmount discards a whole mount's references with no
per-inode `FORGET`, and a sweep can never reclaim an inode another attachment still holds (the
multi-attachment guard is a test at both the vfs and bridge levels). The bridge's `open`/`reference`/
`forget`/`release` route through this API keyed by `cx.attachment.key()`, and `Bridge::sweep_attachment`
is the teardown verb. *Owed:* the transport edges calling the sweep at unmount/disconnect, and
reconciling the process-local attachment key with the durable §4.8 attachment id (the server
wiring).

**NFS: no server reference, a narrowed guarantee.** NFS has no `open`/`FORGET`, so it adds no
references. A single client's unlink-while-open is the *client's* `.nfsXXXX` sillyname (an ordinary
rename then unlink the server already handles). This is **not** a complete cross-client
unlink-while-open guarantee: another client or another transport can remove a file the opener does
not know about (RFC 1813 §4.2 documents exactly this limitation), and slates does not promise
otherwise for the NFS path. NFS's asynchronous requests still need request-lifetime pins (an
in-flight `READ`/`WRITE` pins its object until it completes). Object lifetime and NFS attribute-cache
eviction are different concerns: an authoritative inode is *not* reclaimed on a timer merely because
NFS lacks `FORGET`; it is reclaimed by the reference/reclamation rule above (or, for a truly
orphaned server inode with no client that can ever forget it, by a bounded GC that is a cache/GC
policy, not the object's authority).

## 4. Access, flags and locks (per-open intent, kept)

- **Open-time access, not current mode.** Access is checked at `open`/first-touch against the
  subject and the object's mode/owner, and the *granted* access is recorded in `io_authority`.
  `read`/`write` consult that granted access and the attachment's fencing/revocation — **not the
  object's current mode**. Re-deriving from the current mode on every I/O would wrongly revoke an
  already-open descriptor after a `chmod`, which POSIX forbids. A write against a read-only
  attachment or a pinned (`View::Version`) view is still refused before any mutation.
- **Append is the filesystem's job, owner-serialized.** The kernel does *not* always resolve
  `O_APPEND` to an offset: without writeback caching FUSE expects the filesystem to implement
  append, and with writeback caching kernel-managed append is unreliable when other paths also
  modify the file — and slates has other paths (the SDK ring, the merge task, other mounts). So an
  append write is handled on the owner shard, serialized, resolving the offset to the current end
  under the single-writer discipline; `io_authority` carries the append intent. This is specified
  for the supported (owner-serialized) configuration.
- **Lock ownership.** Byte-range locks are their own operations (FUSE `SETLK`/`GETLK`, NLM for
  NFS), keyed by an owner id carried in per-open state, not on `read`/`write`. Their full
  implementation is owed; the per-open state is where the lock owner lives.

## 5. Fencing, barriers and reclamation

- **Attachment fencing.** `OpContext.authority` carries the epochs; a superseded attachment's late
  write is refused (§4.6 "writeback and snapshot barrier", §4.8 host epoch).
- **Writeback barriers.** `snapshot`/`submit`/`advance`/`detach` stop admission into the closing
  generation, drain **accepted and in-flight** writes, and publish only after they are recorded
  (§4.6). The reference model tells the barrier which inodes have live writers, but the barrier also
  accounts for pending/in-flight I/O — reference counts alone are not the barrier.
- **Final reclamation.** The terminal `release`/`forget` (or an attachment teardown sweep) that
  zeroes `references` on an `nlink == 0` inode runs the deferred body release under the owning
  operation's terminal step, failure-safe (§3).

## 6. Generation and staleness

`ObjectId` carries a generation, but slates **never reuses inode numbers** (D-4). So the primary
staleness mechanism is that a reclaimed number is never reissued: a handle to it fails `getattr`,
and the NFS edge maps `VfsError::NotFound` on a handle's inode to `NFS3ERR_STALE`, not
`NFS3ERR_NOENT` (which is for a *name* lookup). **Object identity survives copy-on-write**: the
handle generation is a stable value for the object's lifetime and must **not** be taken from the
inode's `generation` field, which is the *slab-slot* generation and changes on every CoW. The
tuple's generation is reserved for cross-incarnation cases where a number space is re-minted (a
restarted owner, a clone-from-archive): faithful daemon recovery preserves inode identities and the
allocator's high-water mark so a recovered handle still names the same object, while restoring a
volume as a *new* volume gives it a *distinct volume identity* (its handles do not alias the
original's). Those cases bump or re-scope the generation and are owed with §4.8.

## 7. Per-transport mapping

| | FUSE / virtio-fs | NFS (loopback) | FSKit |
|---|---|---|---|
| Address | node id → `inode` | file handle → `(inode, gen)` | item id → `(inode, gen)` |
| Context | mount attachment (built server-side) | export attachment + request identity | FSVolume attachment |
| Refs | `LOOKUP`/`open` +; `FORGET`/`release` −; unmount sweeps | none (request-lifetime pins only) | per FSKit item lifetime |
| Unlink-while-open | server defers (§3) | client sillyname (single-client only, §3) | server defers |

## 8. Tests required before the change is accepted

Each is "do X, expect Y", through the seam (and, where it needs the kernel, in the mounted lanes
AC-3.10/3.12). **Landed** ones are marked; the rest gate steps 3–4.

1. **open → unlink → continued I/O.** *(read landed; write landed — `lifetime.rs`.)* Read and
   write through the open object after unlink persist until the last reference, then reclaim.
2. **rename-over an open file.** *(Landed — `lifetime.rs`.)* The replaced-but-open inode keeps its
   content until its last reference.
3. **identity across CoW and reclaimed-handle staleness.** *(reclaimed→stale landed —
   `procedures.rs`.)* A handle survives copy-on-write of its object (same identity, updated bytes);
   a reclaimed handle is refused stale; an old identity never aliases a new object. **Not**
   inode-number reuse — numbers are never reused (D-4).
4. **authorization of an actual write.** A read-only attachment/subject: expect `write` refused
   before any effect (not only `ACCESS` reporting reduced bits). `ACCESS` reflecting the mode is
   landed (`procedures.rs`); write authorization is owed with `OpContext`.
5. **reclamation, capacity and cleanup.** Drop the last reference on an `nlink == 0` inode: the body
   is released exactly once (a counter), no earlier, no leak, and the freed capacity is reusable by
   a later allocation. A daemon restart preserves a still-referenced orphan's identity. A
   disconnect/cancellation releases the attachment's references and pins (bounded sweep) with no
   leak and no premature reclamation.
6. **bridge lifecycle wiring.** Through the FUSE/NFS edges: `open`/`lookup` reference, `release`/
   `forget`/unmount unreference, and unlink-while-open works end to end over the mount, not only
   against the core API. (Mounted lane.)

Non-vacuity counters: the deferred-reclamation path, the orphan-survives-restart path, and the
attachment-teardown sweep each export a counter a test asserts moved.

## 9. Implementation plan (piecewise, each gated)

1. **Volume core reference count and deferred reclamation.** *(Landed 2026-09-05, with the point-3
   hardening.)* Per-volume `references`/`orphans`; `Volume::reference` (validated, checked) /
   `unreference` (failure-safe); `drop_link` defers when referenced. Gated `lifetime.rs` (5 tests:
   read- and write-after-unlink survive then reclaim; rename-over preserves; several references
   reclaim only at the last; a reference to an absent inode is refused). Owed within this step: the
   per-attachment ownership records, the teardown sweep, and the orphan's survival across a daemon
   restart (§4.8).
2. **Identity across incarnations.** Confirm identity survives CoW (a stable handle generation, not
   the slab-slot field); preserve inode identities and the allocator high-water mark on recovery; a
   restored-as-new volume gets a distinct volume identity. Tests of §8.3. *(No inode-number reuse.)*
3. **The authority machinery and the `OpContext` interface.** *(Authority, the whole trait onto
   `(ObjectId, &OpContext)`, and both transport edges landed 2026-09-05.)* The local attachment
   authority (`bridge-core/src/authority.rs`): an owned, generation-checked `Attachment` record
   binding volume/view/subject/rights/epoch; an `Attachments` registry that admits under the
   current owner epoch, revokes, drains, fences on `take_over`, and builds an `OpContext` only from
   a validated record — refusing an unknown, revoked or epoch-fenced attachment. **Every** `Bridge`
   method now names its object by `ObjectId` and rides an `&OpContext`; `VolumeBridge` holds its
   volume id and enforces authority at the seam once for every transport (`authorize_read`,
   `authorize_write`, `authorize_volume`): a context binding another volume, a read without read
   rights, a write without write rights or off the current head, is refused before any effect; the
   handle table is now open-state only. FUSE `dispatch` threads the context through every op and
   `serve_blocking` rebuilds it from the mount's attachment before each effect; the NFS `Export`
   admits its attachment at mount time for the enrolled subject. §8.4 (write authorization) landed
   at both edges. Reference-taking is now an explicit per-transport action (`Bridge::reference`):
   the FUSE edge references on `LOOKUP`/`CREATE`/`MKDIR`/`SYMLINK`, NFS references nowhere — closing
   the lookup-reference leak the first NFS surface had. The per-attachment reference ledger and the
   teardown sweep landed (vfs + bridge): references are attributed per attachment, and
   `Bridge::sweep_attachment` releases a whole attachment's references at once, never reclaiming an
   inode another attachment holds. *Owed:* the daemon wiring that creates the FUSE mount's
   attachment from the rendezvous-established principal and calls the sweep at unmount/disconnect
   (the server↔bridge seam, with the real Linux mount); reconciling the process-local attachment key
   with the durable §4.8 attachment id; §8.5 (cleanup), §8.6 (lifecycle).
4. **The NFS read/write/setattr/namespace procedures over the new interface.** *(The metadata,
   file-I/O, namespace and directory-listing procedures landed 2026-09-05.)* Stateless: handle
   → `ObjectId`, the export identity → `OpContext`. `READ` returns the bytes with the count and eof;
   `WRITE` lands `FILE_SYNC` in the anchor and returns `wcc_data` + the volume-derived write
   verifier, its data length capped at the offered transfer size. `SETATTR` decodes the `sattr3`
   set-attribute union (resolving `SET_TO_SERVER_TIME` to the volume wall clock through
   `Bridge::now`, AC-3.10) and honors an `sattr_guard3` ctime compare-and-set (`NFS3ERR_NOT_SYNC`).
   `CREATE`/`MKDIR`/`SYMLINK` create through the seam (CREATE honoring `createhow3`: GUARDED,
   UNCHECKED, and EXCLUSIVE refused `NOTSUPP`) and reply the object's handle/attributes + parent
   `wcc`. `READLINK` returns a symlink target (a non-symlink is `INVAL`). `REMOVE`/`RMDIR`/`RENAME`
   decode `diropargs3` to a parent `ObjectId` and reply the directories' `wcc_data`.
   `READDIR`/`READDIRPLUS` paginate by cookie, budget the reply against the client's
   `count`/`maxcount` (READDIRPLUS carrying each entry's attributes and handle so no per-entry
   GETATTR/LOOKUP is needed), and answer `TOOSMALL`. A write/create through a read-only or
   foreign-volume export is refused at the seam. *Owed:* synthetic `.`/`..` entries (a shared bridge
   concern, the FUSE edge omits them too); a directory-change `cookieverf`; and the request-lifetime
   pins for the async driver.
5. **Access enforcement through `subject`** once §4.13 threads the enrolled consumer; `AUTH_SYS`
   remains an advisory mapping, not authentication.

Removing the redundant `read`/`write` handle indirection happens in step 3, only after step 1
accounted for the reference/reclamation responsibility it sat next to. The bounded generational
handle arena (BUG-4) stays: it holds the open reference and per-open state (granted access, append
intent, lock owner); it is no longer the object-address lookup for `read`/`write`.
