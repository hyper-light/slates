# A scoped export leaked out-of-scope cache invalidations (inode numbers, activity, and file names)

- **Date:** 2026-10-13
- **Area:** `crates/bridge-core/src/scoped.rs` (`ScopedBridge::invalidations`), §4.6 scoped exports (AUD-29-76)
- **Severity:** moderate — a confinement break. Not a data-byte read, but a cross-scope information-disclosure
  side-channel: a tenant confined to one directory could observe the inode numbers, change activity, and the
  **file names** of other tenants' directories in the same volume.

## Description

Found while hardening the VFS against escape attempts (the `/goal` loop), by a real run at the bridge seam
(`crates/bridge-core/tests/volume_bridge.rs::a_scoped_view_never_leaks_an_out_of_scope_cache_invalidation`): a
`ScopedBridge` rooted at `/shared`, asked for its owed cache invalidations after another attachment changed the
out-of-scope `/private/secret`, returned an `Invalidation::Inode { ino: <secret> }` naming that out-of-scope
inode. Creating `/private/leaked` likewise produced an `Invalidation::Entry { parent: <private>, name: "leaked" }`
and an `Inode { ino: <private> }` — leaking a parent inode number **and a real file name** from outside the
scope.

Run evidence (pre-fix): the assertion fired with
`Inode { ino: 281474976710661, data: true }` — `281474976710661 = 0x1_0000_0000_0005`, i.e. volume prefix 1,
counter 5, the out-of-scope `secret`'s inode.

## Root cause

`ScopedBridge` enforces the scope on every operation by calling `admit`/`within_admitted`/`shown` — every data
and metadata verb checks its object with `Bridge::within(object, scope)` before acting, and `readdir` filters
its entries. Its own module doc states the invariant: "A lookup's result and a listing's entries are held to the
same rule … nothing outside is ever shown."

But `invalidations` and `seen` were forwarded straight to the inner bridge with no filtering:

```rust
fn invalidations(&mut self, cx, cursor, out) -> Result<InvalidationCursor, VfsError> {
  self.inner.invalidations(cx, cursor, out)   // unfiltered
}
```

`VolumeBridge::invalidations` draws its records from the **whole volume's journal**
(`self.volume.op_log().since(cursor.journal_seq)`) and emits an `Inode { ino }` or `Entry { parent, name }` for
every change to any inode in the volume, across every scope. A scoped export is a *sub-directory* of a volume
(AUD-29-76, "grant a tenant only one directory"), so the rest of that volume is another tenant's or the owner's
data — and the unfiltered stream handed all of its invalidation traffic to the scoped holder. `seen` returns only
a cursor (two journal/hint sequence numbers), so it carries no object identity and leaks nothing.

## Impact

A malicious scoped client that parses its raw invalidation stream learns, for every change anywhere in the
volume: the changed inode number, whether it was a data or a metadata change (timing/rate included), and for a
namespace change the parent directory's inode and the **file name** created/renamed/removed. That is an
existence-and-activity oracle and a filename disclosure across the scope boundary — a break of the strict
confinement the scoped export promises. A well-behaved client would ignore invalidations for objects it never
cached, so there is no correctness impact for honest clients; the defect is the disclosure to a hostile one.

## Fix

Filter the stream in `ScopedBridge::invalidations` through the same `within` check every other verb uses: after
the inner bridge appends its invalidations, keep only those whose named object (`Inode.ino`, or `Entry.parent`)
lies within the scope; drop the rest. `Entry`'s parent is the test: a name is shown only when the directory it
lives in is in scope, which is exactly what `readdir` already enforces for the same entries. The check is the
bounded parent-chain climb `within` already performs; correctness is not weighed against its cost (R4).

Exact edit (`crates/bridge-core/src/scoped.rs`, `ScopedBridge::invalidations`):

```rust
fn invalidations(&mut self, cx, cursor, out) -> Result<InvalidationCursor, VfsError> {
  let start = out.len();
  let next = self.inner.invalidations(cx, cursor, out)?;
  // The scope invariant holds here too: an invalidation names an inode or a directory entry, and a scoped view
  // must reveal neither an out-of-scope inode nor a name in an out-of-scope directory (§4.6). Keep only those
  // the scope admits, by the same `within` climb every other verb uses.
  for invalidation in out.split_off(start) {
    let object = match &invalidation {
      Invalidation::Inode { ino, .. } => *ino,
      Invalidation::Entry { parent, .. } => *parent,
    };
    if self.inner.within(ObjectId::new(object, 0), self.scope, cx)? {
      out.push(invalidation);
    }
  }
  Ok(next)
}
```

## Sibling sweep

The other `ScopedBridge` methods that forward without `admit` were reviewed:
`release`/`forget` give back a reference or handle this view already took (they reach nothing new);
`flush`/`fsync` act on a file handle this view opened through an admitted `open`/`create`, and flush no data
outward; `now` carries no object; `seen` returns only a cursor. `cache_lifetime` forwards an object to the inner
policy without `admit` — it returns a duration hint, not object content or existence (the inner policy answers
the same hint for a live-source vs overlay inode regardless of the caller), so it is not a disclosure; left as
is and noted for Ada. Only `invalidations` revealed out-of-scope identity.
