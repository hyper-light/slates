# A pinned immutable view received the current head's cache invalidations (names and inodes it never sees)

- **Date:** 2026-10-13
- **Area:** `crates/bridge-core/src/volume_bridge.rs` (`VolumeBridge::invalidations`), §4.16, §4.6
- **Severity:** **DEFENSIVE hardening, not a reachable leak (corrected 2026-10-13).** See the reachability
  correction below: no current mount serves a `View::Version` attachment through this bridge, so this path is not
  reached in practice. The fix is a correct guard should such an attachment ever be served here, but the original
  "a pinned reader gets post-snapshot names" framing overstated it — I asserted reachability from the code and a
  synthetic `View::Version` context without confirming the real attach path.

## Reachability correction (2026-10-13)

Every real attach in the server passes `View::Current` (`server/src/fuse.rs`, `fs_verbs.rs`, `nfs.rs`), and a
snapshot or green **view is served as a clone pinned at that version** (`snapshot_view::open` /
`verbs::clone_green` call `Volume::clone_of`), i.e. `View::Current` over the clone — its own journal is the
clone's, which is correct to deliver. `View::Version` exists in the authority model (and `authorize_write`
refuses it), but nothing wires it onto an attachment served through `VolumeBridge`, and invalidations are
FUSE-only over the attachment's context. So `cx.view == Version` does not reach `VolumeBridge::invalidations`
today. The fix below is kept as a correct guard (defence in depth), **not** a reachable-leak fix. The sibling
scope-axis leak (`ScopedBridge::invalidations`) *is* reachable — scoped exports are served through `ScopedBridge`
over FUSE/virtio-fs, which use the invalidation stream.

## Description

Found by a real run at the bridge seam
(`crates/bridge-core/tests/volume_bridge.rs::a_pinned_view_owes_no_content_journal_invalidation`): an attachment
with a pinned view (`View::Version(_)` — a snapshot or green-volume reader), asked for its owed cache
invalidations after another writer changed the volume's current head, received
`Entry { parent, name: "posthead" }` and `Inode { ino }` for that head change — a file name and inode created
*after* the pinned view's snapshot point, which the frozen view can never see.

Run evidence (pre-fix): the pinned view's owed list was
`[Entry { parent: 281474976710657, name: "posthead", expire: false }, Inode { ino: 281474976710657, data: true }]`.

## Root cause

`VolumeBridge::invalidations` drew its records from the volume's current-head journal
(`self.volume.op_log().since(cursor.journal_seq)`) for every caller, checking only `authorize_read` — which,
unlike `authorize_write`, does not require `View::Current`. A pinned view passed that check and received the
head's mutations. But a pinned view never sees the head: it is served from its version through the view-aware
`view_read`/`view_getattr` paths, and its content changes only through an `advance` re-pin
(`server/snapshot_view::advance`), which computes its own invalidated paths (`invalidated_between`) and returns
them in the `Advanced` reply, recording `Op::AttachmentRepinned` — a catalog op, not a content-journal op. So the
content journal never carries anything a pinned view should act on.

## Impact

A snapshot/green-volume reader over FUSE (the only transport that polls `invalidations`, via
`bridge-fuse/coherence.rs`) learns, for every current-head mutation: the changed inode number, data-vs-metadata,
and for a namespace change the parent inode and the **file name** — including objects created after its snapshot
that are absent from its own view. It also drops kernel cache entries that remain valid for its version, forcing
needless re-fetches (which return the same snapshot data). No wrong data is served; the defect is the disclosure
to, and the wasted work on, a reader that should see a frozen point in time.

## Fix

Emit content-journal invalidations only for a current-head view; a pinned view owes none from this stream. One
guard around the journal step in `VolumeBridge::invalidations`:

```rust
if matches!(cx.view, View::Current) {
  let records = self.volume.op_log().since(cursor.journal_seq) … ;
  for (op, path, inode) in records { self.journal_invalidations(&op, &path, inode, out); }
}
```

The cursor still advances to the head (`Ok(self.seen(cx))`), so a pinned view simply skips head records it does
not care about. The `advance` re-pin path is unchanged — it already delivers its own invalidated paths — so no
working behaviour is removed.

## Scope of the fix / not touched

Only the journal step is guarded, and that is the correct scope. The base-plane step (watcher hints for live base
sources) is deliberately left running for a pinned view, because a pinned view *is* affected by base drift: a
snapshot serves its own pinned extents plus the base's unpinned ranges read from the disk "only while the disk
still holds the snapshot's witnessed base, else `BaseDrift`" (A-49, `base.rs` `view_read`). So when an outsider
edits a base entry, the pinned view's next read of that entry's unpinned ranges turns from witnessed bytes into a
`BaseDrift` refusal — a real change in what the view serves, whose kernel cache must be invalidated. The journal,
by contrast, records current-*head* mutations, which a pinned view never sees at all. Hence the journal step is
gated on `View::Current` and the base-plane step is not. (An earlier draft flagged the base-plane step as an
unproven question for Ada; reading A-49 resolved it — it belongs for pinned views.) The `ScopedBridge` scope
filter still confines base-plane hints to in-scope entries, since it filters the whole appended tail.
