# To main from w-bugs: A-759 landed (the directory listing at O(live children), then a cursor), 2026-11-20

Goal 93's svn fetch rule, the directory step run 9 named. Please mark it complete in the UI if you agree.

- **Part 1 088bf935b (merged fa3f0f82a).** Every directory node-revision holds a persistent children map
  (index/children.rs, shape agreed with w-svn). `Index::children` resolves the directory once and walks its live
  children, and `Index::listing` gives each child's node-revision, so the drive makes entries with no second resolve.
  The maps are charged to the index's room.
  - A/B, release, M-series Mac, /trunk/big with 10,000 names over 1,000 revisions: children 3.84 -> 0.234 ms (9,001
    live) and 3.84 -> 0.112 ms (4,492 live); the drive's listing 5.87 -> 0.98 ms.
  - Memory: the index holds 65,575 entries (1.6 MB) for that history, the maps about 1.2 MB of it.
  - The naive-filesystem oracle holds every listing and each listed id.
- **Part 2 1417c0868 (merged 0deecda43).** `Index::listing_after` is a cursor; the drive's directory step lists 16
  entries a step. One step takes 1.6-1.75 us whatever the directory's size (release). Red, held by entries per step:
  1,801 in one step before; at most 16 after.
- **Left:**
  - The replay drive's `add_subdir` lists a copied directory whole (now the map walk, O(live)).
  - The by-use re-measure of a checkout's and an update's longest step on CI can't be read until w-perf fixes S4 c2's
    text reads (14215b0c3), which rebuild a whole text on every piece and made fetches 3-12x slower (runs 10/11; in
    w-perf's inbox).
  - Run request: once that lands, diag-w-bugs-svnfetch's checkout and update on macos-latest. Red would be a step
    kind past 1 ms that isn't a text read.
