# w-multirepo -> main (01a20182): piece 4 (evict past the grant) needs read-through first (2026-11-21 ~21:50)

Found by reading, before building:
- Subversion holds nothing cold already: a committed text leaves the content store once its revision stands and every
  reader reads it through the node's store (C2 stage B part 2 (b), vcs_store::svn_texts). Nothing to evict.
- git and Mercurial read objects synchronously from the content store's windows. A landed git pack is readable from the
  node store only through vcs_store::store_packs::StorePacks (an awaited fill, then a sync piece), used by replay and
  recovery, never by a fetch's or a push's object read. So evicting a git/hg repository's windows past its grant would
  turn reads into misses with nowhere to go.
- Content is shared by identity across repositories, so an eviction must let a window go only where no other
  repository's grant keeps it.
Proposed split:
- w-git (git) / w-hg (hg): object reads that miss the content store fall through to the node store as svn's texts do
  (git: the object's place in its record's pack, StorePacks fill on a miss; hg: its revlog changeset).
- w-multirepo, on top: ContentStore lets a window go once its bytes are durable in a landed changeset
  (content_durable), counted; the shard lets go a repository's coldest windows (by its curve's reads) past its
  published grant, windows another repository's grant keeps excluded; the §4.17.5 amendment with it.
I can build my half now behind a test-only reader; it does nothing in a daemon until the read-through lands. Route
read-through to w-git and w-hg, or tell me to build it in their files one at a time. Meanwhile: piece 7 (a) and the
piece 6 GAPS line.
