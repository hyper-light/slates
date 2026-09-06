# NFS lookup-reference leak: the shared bridge references for a transport that never forgets

Date: 2026-09-05. Found by reading design §3 against the code after landing the NFS procedure
surface. **Fixed** the same day (the leak-closing half); the per-attachment teardown sweep and
restart handoff remain owed (server/shard layer, below).

## Description

Every NFS `LOOKUP`, `CREATE`, `MKDIR` and `SYMLINK` takes a **lookup reference** on the resolved
inode that is never dropped, so an NFS export's referenced-inode set grows without bound over the
life of the export. This directly contradicts the design.

## Root cause

Reference-taking lives in the transport-neutral shared bridge, not at the transport edge:

- `crates/bridge-core/src/volume_bridge.rs` calls `self.reference_lookup(...)` inside `lookup`
  (line 230), `create` (378), `mkdir` (439) and `symlink` (473). Every transport that calls these
  takes a lookup reference.
- The FUSE edge balances it: `Bridge::forget` drops `n` references on `FORGET(inode, n)`.
- The NFS edge does not: `crates/bridge-nfs/src/procedures.rs` has no `forget`/`unreference` call
  anywhere (grep confirms), because **NFSv3 has no `FORGET`** — an NFS file handle is stateless and
  the client never tells the server it is done with an inode.

Design §3 (`docs/wip/inode-addressed-io.md`), verbatim: *"NFS has no `open`/`FORGET`, so it adds no
references."* And: *"Every reference is owned by an attachment."* So reference-taking is a
per-transport behaviour (FUSE yes, NFS no), which the transport-neutral shared bridge is the wrong
layer to perform unconditionally.

## Impact

- Bounded but unreclaimed growth: an NFS export accumulates one `references` map entry per distinct
  inode it ever resolved, and never releases them. Bounded by the inode-table cap (`reference`
  validates the inode exists), so it is a leak within a volume's inode ceiling, not unbounded —
  Part 2 item 8's "unbounded growth" bound is not breached, but the reclamation rule is defeated for
  the NFS path: an unlinked inode an NFS client resolved once is pinned forever (`references != 0`),
  never reclaimed.
- The open reference is **not** affected: NFS `CREATE` already drops the open reference immediately
  (`do_create` calls `bridge.release`), so only the *lookup* reference leaks.

## Fix (landed)

Reference-taking moved out of the shared bridge to an explicit action the FUSE edge invokes, per §3
("FUSE takes one per LOOKUP/CREATE/MKDIR/readdirplus entry"):

1. Added `Bridge::reference(object, cx)` (the inverse of `forget`); removed the four
   `reference_lookup` calls from the shared `lookup`/`create`/`mkdir`/`symlink`
   (`crates/bridge-core`). `create` now takes only an open reference.
2. The FUSE `dispatch` calls `bridge.reference` after a successful `LOOKUP`/`CREATE`/`MKDIR`/
   `SYMLINK`; a reference failure fails the reply so the kernel never gets an unreferenced node id.
   NFS calls it nowhere, so the NFS path takes no references (§3).
3. Tests: the FUSE dispatch LOOKUP test asserts the mock's `referenced` counter moved (one on a
   hit, none on a miss) — the non-vacuity proof the edge references. Three bridge-core lifetime
   tests: an open file survives unlink via the open reference (reclaimed at release); the FUSE
   model (an explicit `reference` pins across release until `forget`); and the NFS model / this
   fix — a transport that takes no reference does not pin past release, so an unlink reclaims at
   once. That last test would have failed before the fix (the implicit reference kept the inode
   alive), so it discriminates the fix.

Verified: 140 tests pass across the four bridge/vfs crates; fmt, clippy, xtask, cross clean.

**Separately owed** (design §3, §4.8, not this fix): the per-attachment reference *ledger* and the
**teardown sweep** (a FUSE unmount discards a whole attachment's outstanding lookup references in
one bounded batch, since FUSE does not guarantee a `FORGET` per reference), and the restart handoff
of an orphan with a live holder. That work belongs at the server/shard layer (§4.1: the shard owns
both the volume and the attachments), not in the per-request `bridge-core` seam, so it is a larger
piece than this leak fix.

## Sibling check

Grepped the shared bridge for other unconditional reference-taking: the four sites above are the
only `reference_lookup` calls. The open reference (`open_handle`) is correct for both transports
(FUSE `release` and NFS's immediate `release` both drop it). No other sibling instances.
