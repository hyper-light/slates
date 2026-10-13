# A scoped export's readdir lost in-scope files behind a page of out-of-scope entries

- **Date:** 2026-10-13
- **Area:** `crates/bridge-core/src/scoped.rs` (`ScopedBridge::readdir`), §4.6 scoped exports (AUD-29-76)
- **Severity:** moderate correctness bug in the scoped-export feature — a scoped tenant's own directory listing
  silently omits some of its files. Not a confinement *leak* (it shows fewer entries, never more), but it makes a
  scoped `ls`/`readdir` unreliable. Found while hardening (the `/goal` loop) after being told to *exercise*
  claims rather than read them: I had treated scoped readdir as "covered" from the single-page tests and the code;
  paging it for real surfaced this.

## Description

A directory inside a scope can hold entries the scope drops: hard-link aliases of files homed outside it
(`within` is conservative about an alias whose home is out of scope). `ScopedBridge::readdir` fetched **one**
inner page, filtered it, and returned the survivors. When a whole inner page is droppable aliases, the filtered
page is **empty** — and an empty page arriving before the directory's end is read by the transport as
end-of-directory. NFS encodes exactly that: `eof = sent == rows.len() && rows.len() < limit`
(`bridge-nfs/src/procedures.rs`), and `rows` is the scoped (filtered) page, so an empty scoped page gives
`eof = true`. A paging client then stops, and every in-scope entry ordered after that dropped page is lost.

Reproduced e2e (`crates/bridge-core/tests/volume_bridge.rs::a_scoped_directory_paged_by_cookie_never_loses_an_in_scope_entry_behind_dropped_pages`):
in `/shared/d`, 64 distinct out-of-scope inodes linked in plus four in-scope files, all under one name space so
they interleave in directory order; served `shared` scoped; paged by cookie with `limit = 4`. Before the fix the
pages were `[., .., e01] [e02, e03] []` — the empty third page while `e67` still sat behind the aliases — and the
listing **lost `e67`**. (An earlier attempt with aliases of a single inode did *not* reproduce it: those entries
clustered after the in-scope ones, so the only empty page was the real end — which is why reading the code was
not enough; the layout that interleaves droppable entries is the one that bites.)

## Root cause

Filtering one inner page in isolation breaks the pagination contract that the transports rely on: "a page shorter
than the limit (empty included) means the directory ended." A scope can empty a page in the *middle* of the
directory, so a short/empty scoped page no longer implied the end.

## Fix

`ScopedBridge::readdir` now loops: it fetches inner pages, advancing the cursor to each page's last cookie, and
accumulates the in-scope survivors until it has a full `limit` of them or the inner directory is exhausted (an
empty inner page). A short or empty result therefore means a *true* end — never a page the scope happened to
empty — so the transport's `eof` is correct again. The cursor the client resumes from is the last shown entry's,
unchanged; the dropped entries between are simply skipped. A single cookie group that fills a page without
advancing the cursor breaks the loop (the inner bound governs that case, as it already does for the overflow
reply).

```rust
let mut shown = Vec::new();
let mut cursor = cookie;
while shown.len() < limit {
  let page = self.inner.readdir(object, cx, fh, cursor, limit)?;
  let Some(next) = page.last().map(|e| e.cookie) else { break }; // inner exhausted
  for entry in page { /* keep "." , ".." (rewritten at the scope root), and within_admitted */ }
  if next == cursor { break; }
  cursor = next;
}
Ok(shown)
```

## Verification

`a_scoped_directory_paged_by_cookie_never_loses_an_in_scope_entry_behind_dropped_pages` fails before the change
(missing `e67`) and passes after; the unscoped pagination oracle
`a_directory_paged_by_cookie_lists_every_entry_exactly_once` still passes (the cursor/cookie-group semantics are
unchanged); the whole `volume_bridge` suite (34) and `scope` (1) are green.

## Note

Two things left honest rather than rounded up:

- **Per-call cost.** The loop crosses dropped entries to reach in-scope ones. Because the client resumes from the
  last *shown* entry's cookie, each dropped entry is crossed exactly once across the whole listing, so the total
  work to list a directory is O(entries) — the same as before. But a single `readdir` call can cross one
  run of droppable entries, so its worst case is O(run) rather than O(limit): a directory that is almost entirely
  out-of-scope aliases with one in-scope entry at the end makes the call that reaches it O(directory). It is
  bounded by the directory's entry count (itself quota-bounded), not unbounded, and the common case (few
  droppable entries) is ~one inner page. Slicing it under a per-iteration budget would need an explicit
  "more to come" signal added to the readdir contract (today a short page *is* the end signal); that is a
  larger change and the pathological directory is owner-created, so it is noted, not done.

- **Oversized droppable cookie group.** A single cookie group larger than the limit that is also entirely
  droppable stops the loop with a short page (the `next == cursor` break). The inner readdir already signals
  overflow for an oversized cookie group (FUSE `EOVERFLOW`), so this is the inner layer's to bound; no case of it
  was reproduced here.

Both noted for Ada.
