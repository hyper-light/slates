# Inode-addressed shared I/O: interface and lifetime design

> **Status: proposed, pending Ada's acceptance (2026-09-05).** This note is the "precise
> shared-interface/lifetime design" Ada asked for before replacing the handle-dependent I/O
> interface. Only its foundational step 1 (the volume-core reference count and deferred
> reclamation) is implemented; the interface change (steps 3–4) is not. On acceptance it becomes
> amendment A-10, applied to §4.5 (inode lifetime) and §4.6 (the Bridge trait), and the
> implementation follows the plan in
> §9 below, each piece gated and each of the §8 tests passing first.

## 1. The decision and why

The shared operation layer (`slates-bridge-core`, §4.6 "one VFS operation layer") today keys
`read`/`write` on an *open handle* the way the first, FUSE-shaped implementation did: `open`
returns a small integer, the kernel echoes it, and `VolumeBridge` maps it back to an inode through
a per-bridge table. NFS is stateless — a `READ` carries a file handle that names its object by
identity, with no prior `open` — so it cannot supply that handle. virtio-fs (FUSE-over-virtio) and
a future FSKit path have the same operation set as FUSE but are still transports over the one
layer.

The design already fixes the addressing model: **"Every request carries `(volume handle, inode no,
gen)`"** (§4.6). So the shared I/O operations should identify the object by that tuple, carry the
authenticated attachment/view context, and take an explicit offset. NFS calls it directly; FUSE and
virtio-fs translate their requests into the same calls.

**Removing the handle→inode lookup does not remove per-open lifetime obligations.** The identity
tuple says *which object* a request addresses; it does not, by itself, count references or hold
per-open state. Those obligations — inode/content lifetime after unlink while still referenced,
access rights and open flags and lock ownership, attachment fencing and writeback barriers and
final reclamation — remain and are specified here. This note removes a redundant indirection
(handle → inode for read/write) only after accounting for every responsibility that indirection was
adjacent to.

## 2. The interface

```rust
/// The object a request addresses (§4.6). The volume is the bridge's own (one export/mount per
/// volume), so it is implicit here; a multi-volume transport carries it alongside.
struct ObjectId { inode: u64, generation: u64 }

/// The view and authority a request carries — the "authenticated attachment/view context".
struct OpContext {
  /// Which state the request sees: the volume's current head, or a pinned immutable version (a
  /// green-volume or snapshot mount pins one; §4.16 `advance` re-pins). A write against a pinned
  /// version is `EROFS`.
  view: View,            // Current | Version(Version)
  /// The attachment's authority. A read-only attachment refuses writes before any effect.
  rights: Rights,        // Read | ReadWrite
  /// The authenticated consumer, for per-principal access checks (§4.13). Threaded from the
  /// transport's credentials (FUSE header uid/gid; NFS `AUTH_SYS`); owed until §4.13 lands, and
  /// until then the owner is assumed (a volume is its provisioning agent's own).
  principal: Principal,
}

trait Bridge {
  fn read(&mut self, object: ObjectId, cx: &OpContext, offset: u64, size: u32, out: &mut Vec<u8>)
    -> Result<(), VfsError>;
  fn write(&mut self, object: ObjectId, cx: &OpContext, offset: u64, data: &[u8])
    -> Result<u32, VfsError>;
  // getattr/lookup/... likewise take ObjectId + &OpContext instead of a bare inode.
}
```

`read`/`write` no longer take an open handle. The object identity is authoritative; the context
carries the view and the authority. The offset is explicit (it always was). This is exactly what an
NFS `READ`/`WRITE` supplies (a file handle → `ObjectId`, the credentials → `principal`, an explicit
offset), and exactly what FUSE supplies (the request's node id → `inode`, the mount's attachment →
`OpContext`, the request's offset).

**What this deletes:** the `read`/`write` dependence on a handle table to find the inode. The FUSE
read/write path already carries the node id (the inode) in every request, so it loses nothing.

**What this keeps (see §3–§5):** `open`, `create`, `release`, `flush`, and the per-inode reference
count they maintain — because they carry per-open state and drive reclamation, which the identity
tuple does not.

## 3. The reference model (the lifetime the identity tuple does not carry)

Give each inode a reference count separate from its hard-link count:

```
references(inode) = lookup_references + open_references
```

- **`lookup_references`** — held by a transport that has been handed the object and may address it
  later. FUSE takes one on every `LOOKUP`/`CREATE`/`MKDIR`/readdirplus entry and drops `n` on
  `FORGET(inode, n)` (this is FUSE's contract; the kernel guarantees a `FORGET` for every reference
  before the inode may be reclaimed). virtio-fs is identical.
- **`open_references`** — one per live `open`/`create` result, dropped on `release`.

**Reclamation rule.** An inode's content and table entry are reclaimed only when **both**
`nlink == 0` (it has left the namespace) **and** `references == 0` (no transport still holds it).
Until then an unlinked-but-referenced inode is *unlinked* (invisible to name lookups, its name
freed) but *alive* (its content served to holders of its identity).

This closes the concrete gap Ada identified. `drop_link` (`crates/vfs/src/volume.rs`) today, at
`nlink == 0`, immediately `release_body` + `table_remove` + `retire`, with no reference check — so a
`open` + `unlink` + `read` loses the content. Under this rule `drop_link` at `nlink == 0` instead:

- if `references > 0`: mark the inode *unlinked* (remove its name/table-visible entry from lookups,
  keep the inode and its body), and record it on a per-volume *orphan set*;
- if `references == 0`: reclaim now, as today.

and `release`/`forget`, when they bring `references` to 0 on an inode already `nlink == 0`, perform
the deferred `release_body` + `retire` + `base_forget` at that terminal step. The orphan set is
bounded (open files are bounded) and is part of recovery state (an orphan with a live holder must
survive a daemon restart's handoff, §4.8).

**NFS is the exception that needs no server deferral.** NFS has no `open` and no `FORGET`, so it
adds no references. Unlink-while-open across NFS is the *client's* job: it renames the victim to a
`.nfsXXXX` sillyname and removes it on last close, so the server sees an ordinary rename then an
ordinary unlink. The server-side reference deferral above therefore serves FUSE, virtio-fs and
FSKit; NFS correctness for this case is the sillyname, which the server already supports as a
rename. NFS inode reclamation is instead time- and generation-bounded (§6, the design's "inode GC
because NFS never says forget").

## 4. Access, flags and locks (per-open state, kept)

- **Access rights.** Checked before every effect against `cx.principal` and the object's mode/owner
  (§4.13). `write` on a `Rights::Read` attachment, or on a `View::Version` (a pinned immutable
  view), is refused before any mutation (`EROFS`/`EACCES`). The NFS `ACCESS` correction already
  computes granted bits from the mode; this generalizes it to every op and to the real principal.
- **Open flags.** `O_APPEND` and friends are per-open. slates serves offset-addressed writes; the
  kernel resolves append to an offset for FUSE, and NFS `WRITE` is always offset-addressed, so the
  data path needs no per-open flag. Flags that *gate* an open (e.g. `O_RDONLY` vs `O_RDWR`) are
  checked at `open` against `cx.rights` and recorded on the open handle for `flush`/`release`.
- **Lock ownership.** Byte-range locks are their own operations (FUSE `SETLK`/`GETLK`, NLM for NFS),
  keyed by an owner id, not carried on `read`/`write`. They are out of scope for this interface
  change and remain owed; the open handle is where a per-open lock owner would live.

## 5. Fencing, barriers and reclamation

- **Attachment fencing.** Each attachment carries an epoch; a write records the epoch; a superseded
  attachment's late write is refused (§4.6 "writeback and snapshot barrier", §4.8 host epoch).
  `OpContext` carries the attachment identity so the owner can fence.
- **Writeback barriers.** `snapshot`/`submit`/`advance`/`detach` stop admission into the closing
  generation, drain accepted writes, and publish only after they are recorded (§4.6). The reference
  model above is what lets a barrier know which inodes still have live writers (`open_references`).
- **Final reclamation.** As in §3: the terminal `release`/`forget` that zeroes `references` on an
  `nlink == 0` inode runs the deferred body release under the owning operation's terminal step
  (cancellation-safe, §3 of CLAUDE.md).

## 6. Generation and staleness

`ObjectId` carries a generation so the tuple can distinguish a reused inode *number* — but slates
**never reuses inode numbers** (D-4, "inode numbers on demand and never reused"). So in a single
volume the primary staleness mechanism is simpler and stronger: a handle names a number, and once
that number's inode is reclaimed (§3) it is never reissued, so a `getattr` on it fails and the
handle is refused **stale** (a gone number is a gone object). The corrected NFS `attrs_of` maps
exactly that — `VfsError::NotFound` on a handle's inode becomes `NFS3ERR_STALE`, not `NFS3ERR_NOENT`
(which is for a *name* lookup, §4.4 of the audit).

The generation therefore stays a stable value (0) for an object's lifetime; note that the inode's
existing `generation` field is the *slab-slot* generation and changes on copy-on-write, so it must
**not** be propagated to the handle (that would make a handle stale after any write). The tuple's
generation is reserved for the cross-incarnation / fleet cases where a number space could be
re-minted (a restarted owner, a clone-from-archive); those bump it and are owed with §4.8. NFS
reclamation of an unreferenced inode is time-bounded (a GC timeout), the design's "inode GC because
NFS never says forget".

## 7. Per-transport mapping

| | FUSE / virtio-fs | NFS (loopback) | FSKit |
|---|---|---|---|
| Address | node id → `inode`; gen tracked | file handle → `(inode, gen)` | item id → `(inode, gen)` |
| Context | mount attachment (view, rights, uid/gid) | export attachment + `AUTH_SYS` | FSVolume attachment |
| Refs | `LOOKUP`/`open` +; `FORGET`/`release` − | none (stateless) | per FSKit item lifetime |
| Unlink-while-open | server defers (§3) | client sillyname | server defers |
| Inode GC | on last `FORGET` | timeout + generation | on last item release |

## 8. Tests required before the change is accepted (Ada's list)

Each is "do X, expect Y", driven through the seam (and, where it needs the kernel, in the mounted
lanes AC-3.10/3.12):

1. **open → unlink → continued I/O.** Open a file, unlink it, read and write through the open
   object; expect the bytes to persist until the last reference drops, then the content reclaimed.
2. **rename-over an open file.** Rename another file over an open one; expect the open object to
   keep serving its original content until its last reference drops.
3. **stale generations.** Reclaim an inode and reissue its number under a new generation; expect a
   handle under the old generation refused stale, never served the new object.
4. **access enforcement.** A read-only attachment/principal; expect `write` refused before any
   effect; `ACCESS`/`read` reflect the object's real permissions.
5. **reclamation after the last reference.** Drop the last reference on an `nlink == 0` inode;
   expect the body released exactly once (a counter), no earlier, no leak, and a daemon restart in
   between preserves a still-referenced orphan.

Non-vacuity counters: the deferred-reclamation path and the orphan-survives-restart path each
export a counter a test asserts moved.

## 9. Implementation plan (piecewise, each gated)

1. **Volume core reference count and deferred reclamation.** *(Landed 2026-09-05.)* A per-volume
   `references: BTreeMap<InodeNo, u32>` and `orphans: BTreeSet<InodeNo>`; `Volume::reference` /
   `Volume::unreference`; `drop_link` splits into *defer-when-referenced* (orphan-set insert, keep
   addressable) and `reclaim_inode` (the deferred body release, run at the last `unreference`).
   Gated `crates/vfs/tests/lifetime.rs` (3 tests): open→unlink→read survives then reclaims;
   rename-over an open file preserves it; several references reclaim only at the last. Failing test
   first proved the gap. Owed within this step: the orphan set's survival across a daemon restart
   (§4.8 recovery).
2. **Per-inode generation.** Track and bump the generation on inode-number reuse; `getattr`
   reports the real generation. Test 3.
3. **The `OpContext` and the neutral `Bridge` signature.** Introduce `ObjectId`/`OpContext`;
   change `read`/`write` (then the rest) to take them; `open`/`create`/`release` take/refresh a
   reference and record per-open state. `VolumeBridge` uses the inode directly. Update the FUSE
   edge (node id → `ObjectId`, mount attachment → `OpContext`, `LOOKUP`/`open` → reference,
   `FORGET`/`release` → unreference). All existing FUSE tests stay green.
4. **The NFS read/write/setattr/namespace procedures over the new interface.** Now stateless and
   natural: each op carries the handle → `ObjectId` and the credentials → `OpContext`. Test 4.
5. **Access enforcement through `cx.principal`** once §4.13 threads the real principal; until then
   the owner assumption, documented.

Removing the redundant `read`/`write` handle indirection happens in step 3, only after step 1 has
accounted for the reference/reclamation responsibility it sat next to. The bounded generational
handle arena (BUG-4) stays: it is where `open`/`release` hold the open reference and per-open
state; it is no longer on the `read`/`write` path.
