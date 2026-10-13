# A pinned immutable view received the current head's cache invalidations (names and inodes it never sees)

- **Date:** 2026-10-13
- **Area:** `crates/bridge-core/src/volume_bridge.rs` (`VolumeBridge::invalidations`), §4.16, §4.6
- **Severity:** low — a snapshot/green-volume reader already holds whole-volume read authority *as of its
  version*, so the disclosure is only of objects created **after** its snapshot (post-snapshot file names and
  inode numbers) plus the activity timing. It is also a correctness waste: the frozen view is told to drop
  caches that are still valid for its version. Found while hardening after the cross-scope invalidation leak
  (same class, the view axis instead of the scope axis).

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

Only the journal step is guarded. The base-plane step (watcher hints for live base sources) is left as is: it is
reached only for a volume with a host base plane, and whether a pinned view of a base-overlay volume does live
base passthrough (and so legitimately needs base-drift hints) is a §4.4/§4.5 question not resolved here and not
demonstrated to leak. The proven defect — current-head *journal* mutations reaching a pinned view — is fixed;
the base-plane question for pinned views of base-overlay volumes is noted for Ada, unproven.
