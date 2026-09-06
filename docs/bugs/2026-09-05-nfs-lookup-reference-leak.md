# NFS lookup-reference leak: the shared bridge references for a transport that never forgets

Date: 2026-09-05. Found by reading design §3 against the code after landing the NFS procedure
surface. Not yet fixed — the fix mechanism is architectural and awaits Ada's scope confirmation.

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

## Proposed fix (awaiting scope confirmation)

Move lookup-reference-taking out of the shared bridge and make it an explicit action the FUSE edge
invokes, per §3 ("FUSE takes one per LOOKUP/CREATE/MKDIR/readdirplus entry"):

1. Add `Bridge::reference(object, cx)` (the inverse of the existing `forget`); remove the four
   `reference_lookup` calls from the shared `lookup`/`create`/`mkdir`/`symlink`.
2. The FUSE `dispatch` calls `bridge.reference` after a successful entry-returning op; NFS does not.
3. Update the mock (`bridge-fuse/tests/dispatch.rs`) and the reference tests
   (`bridge-core/tests/volume_bridge.rs`, `bridge-nfs/tests/procedures.rs`) — the bridge-core
   lifetime test then simulates the FUSE edge by calling `bridge.reference` explicitly.

This fixes the leak and needs no per-attachment ledger. **Separately owed** (design §3, §4.8, not
this fix): the per-attachment reference *ledger* and the **teardown sweep** (a FUSE unmount discards
a whole attachment's outstanding lookup references in one bounded batch, since FUSE does not
guarantee a `FORGET` per reference), and the restart handoff of an orphan with a live holder. That
work belongs at the server/shard layer (§4.1: the shard owns both the volume and the attachments),
not in the per-request `bridge-core` seam, so it is a larger piece than this leak fix.

## Sibling check

Grepped the shared bridge for other unconditional reference-taking: the four sites above are the
only `reference_lookup` calls. The open reference (`open_handle`) is correct for both transports
(FUSE `release` and NFS's immediate `release` both drop it). No other sibling instances.
