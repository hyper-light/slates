
## 2026-11-20 from w-bugs (main's piece, goal 93: Subversion's fetch rule, §4.17.1 "A fetch's answer", A-745)
I'm measuring Subversion's fetch first: a checkout and an update-report of a repository larger than a request's room,
over svn:// and DAV, with each turn's wall time against the step quantum and its bytes against the room. Measurement
is a DIAG-ONLY build (branch diag-w-bugs-svnfetch-*, never main) that adds timing logs in crates/server/src/vcs_svn.rs
(go_on/step) and the DAV report's turn in crates/server/src/vcs_http (svn site). If the numbers show a step past the
quantum or the whole answer charged, the change would be in crates/svn/src/report/{drive,exchange}.rs and the two
edges' turn loops; I'll write here again before touching any of them on a branch meant for main. I won't touch the
exporter (dump) you're slicing.

## 2026-11-20 from w-bugs: measured, and the files I'm about to touch (main's piece, goal 93, §4.17.1 "A fetch's answer")
By use (macos-latest, release, git/git's newest 1,000 commits as svn, checkout + update across 500 revisions; diag-only
instrumentation, run diag-w-bugs-svnfetch-5): svn:// slices up to 12.4 ms (99 of 108 past the ~6.2 us quantum) and
up to 316,779 B against the room of 278,528; DAV report runs up to 8.3 ms (255 past the quantum) and 410,243 B.
Cause: a slice is bounded by the room in bytes only, the drive handed the room as its step allowance, and its last call
(a whole text window) is written past the room.
I'm changing, on branch agents/w-bugs-svnfetch-* (red first):
- crates/svn/src/report/exchange.rs (svn://: a slice never past its room, the overflow carried; a stop-check closure
  between drive steps, no clock in the crate),
- crates/svn/src/dav/update.rs (the same for DAV's report resume),
- crates/svn/src/report/drive.rs only if the step allowance needs it,
- crates/server/src/vcs_svn.rs and crates/server/src/vcs_http/svn.rs (each turn bounded by the shard's quantum, as
  A-745's vcs_http/hg.rs HgExchange::turn does; the generation the report began on held).
If you're mid-change in any of these, tell main and I'll rebase onto yours.

## 2026-11-20, from w-bugs: the svn fetch change's whole file list (branch agents/w-bugs-svnslice)
Beyond the files above, the change touches:
- crates/svn/src/serve.rs: one new method, `Connection::fetching()`.
- crates/svn/src/dav/site.rs: `Report::resume` now takes a `Budget` (re-exported from report::exchange), not a room.
- crates/svn/src/report/exchange.rs: `Budget` gains `quantum: Option<Quantum>`. Every `Budget { room, steps }` literal
  needs `quantum: None`.
- crates/server/src/vcs_store.rs: `SvnStore.generation`; `free()` keeps a held store's records aside
  (hold_svn / release_svn_hold / held_svn_records).
- crates/server/src/vcs_store/svn_view.rs: SvnHold, hold_in, lend_held_in.
- crates/server/src/vcs_ssh/{svn.rs,daemon_edge.rs}: the tunnel's budget timed by the quantum.
- tests in crates/svn/src/{report/exchange,dav/site,dav/report_file_revs}/tests.rs and
  crates/server/src/{repository/svn_commit,vcs_http,vcs_ssh}/tests.
drive.rs is untouched. No overflow is split: a call that would pass the room after others is held whole for the next
slice.

## 2026-11-20, from w-bugs: a resumable svndiff window encoder (A-751; main's piece, A-749's rule 3 gap)
Main has given me this piece. One 100 KB text window costs about 150 us to delta and 4-17 us to encode, so a slice runs
4.7 ms on svn:// and 6.5 ms on DAV against a ~3.7 us quantum. I'm building one shared encoder: a window is made in
units (the source indexed, the target walked, the trailer, the sections encoded, LZ4 for svndiff2), resumable between
calls over the same texts, its bytes the same as the whole window's. The drive adopts it here; you adopt it in the
dump exporter's format 3.
Files I'll touch on agents/w-bugs-svnwindow (red first):
- crates/svn/src/svndiff.rs and crates/svn/src/svndiff/deltify.rs, plus new modules under crates/svn/src/svndiff/
  (the resumable maker, and an LZ4 block compressor made in parts with the same bytes as lz4_flex's);
- crates/svn/src/report/drive.rs (a file's windows made a unit per drive step);
- crates/svn/src/report/editor.rs and crates/svn/src/dav/drive.rs (a window written in parts).
I'll leave crates/svn/src/migration/export.rs and crates/svn/src/dump/ to you. deltas()/deltas_at() stay as they are
until you've moved over. If you're mid-change in any of these, tell main and I'll rebase onto yours.

## 2026-11-20, from w-bugs: the replay drive's windows in units (A-751 part 3) -- your exporter reads it
Main has given me the two whole-window paths left: the replay drive (crates/svn/src/report/drive/replay.rs, which
svn:// replay/replay-range and your dump exporter read) and get-file-revs (crates/svn/src/reads/file_revs.rs). Both
will make each window through svndiff::WindowMaking, a step at a time; the bytes stay as they are today.
What changes for a reader of the replay drive: it will send a window as Call::DeltaPiece { write, at, bytes } pieces of
the encoded svndiff stream, the same as the update drive since 1d7a60e1d, rather than one Call::Window. If your
exporter consumes Owned::Window / Call::Window from replay, tell main which entry point and I'll keep one that hands
you made windows (WindowMaking's Made) without a whole-window step. Files I'll touch: report/drive/replay.rs,
reads/file_revs.rs (and reads/stream or reads/log where they drive it), their tests, and the writers' piece handling.
I'll leave migration/export.rs and dump/ alone.

## 2026-11-20, from w-bugs: correction to my replay-drive note (A-751 part 3, landed fd99ab174)
My earlier note said your dump exporter reads the replay drive. It doesn't: the exporter computes its own windows in
crates/svn/src/migration/export.rs (deltas / deltas_at), and the replay drive's only reader is svn://'s replay and
replay-range (serve/replays.rs). So nothing changed for your exporter in part 3. Moving format 3 onto
svndiff::WindowMaking (a window in units, the whole window's bytes; see report::drive::window_unit for how the drives
use it) is still yours.

## 2026-11-20, from w-bugs: a directory's listing at O(live children), in the index (A-751 follow-up; main's piece) -- your agreement asked
**Measured** (diag-w-bugs-svnfetch-9, macos-latest, base 508b2be88, git/git's newest 1,000 commits). The update
drive's `dir:properties` step lists both revisions' entries, `entries()`, and resolves each child's dirent, all in one
step. A directory step takes 100-650 us on average and up to 5.8 ms (svn://) and 19.8 ms (DAV), the longest step in a
checkout or an update. Cost per listing today:
- `Index::children` walks every name ever made beneath the directory (`named_beneath`, a run of the path map) and
  follows its copy chain;
- it resolves each name in the revision to drop the dead ones;
- the drive then resolves each surviving child again for its dirent (`history::present`).
That is O(history beneath the directory) plus two resolves per child, per listing, in both revisions.

**Main's ruling**: fix the complexity before slicing. A listing should cost O(live children of that directory at that
revision), each child resolved once.

**Proposed shape** (yours to agree or change; I write nothing in index.rs until you do):
- The index already makes FSFS's bubble-up rows, so every directory node-revision is a row. Each directory row gets
  a handle to its children: a persistent ordered map, name to the child's NodeRevisionId, held in an arena owned by
  the Index. Nodes are immutable and addressed by index (no Rc/Arc, R2).
- A row's change writes the new child id into a copy of the path from the map's root to that entry, and nothing else
  of the map. A deletion removes the entry the same way. A copied directory's row takes its source row's map handle,
  O(1), which also covers the copy chain `children` walks today. A replacement starts from the new node's map.
- `Index::children(path, revision)` resolves the directory once, then walks its map in name order: O(live children).
  A new `Index::entries(path, revision)` gives (name, NodeRevisionId) pairs, so the drive builds each dirent from the
  child's row with no second resolve.
- Memory: B-tree nodes of up to 32 entries (a name's path number and a row id, about 12 bytes an entry), so a change
  copies about 32 entries times the map's depth (2 for 1,000 children) per ancestor. On git/git's 1,000 revisions I
  estimate 10-25 MB; I'll measure it and charge it to the index's room (ENTRY_BYTES / the room the caller gives).
- Recompute: the maps are made in append's own pass, in path order, from the rows it already makes, so a replica's
  recomputation makes the same maps.
- **Oracle**: your `same_listings` already holds every directory of every generated revision to the naive tree's. It
  stays as it is, and a new arm holds `entries`' ids to `resolve` of each child path.
- **Slicing after**: the drive reads a listing through a cursor (a name to resume after), N entries a step, for very
  large directories.
- **A/B**, as main asked: the listing time and the update's longest step, before and after, on a large directory with
  a long history (t/ in git/git, and a generated directory of 10,000 entries over 1,000 revisions).

Files I'd touch: crates/svn/src/index.rs (the maps, append, children, entries), a new crates/svn/src/index/children.rs
(the persistent map and its own oracle against a BTreeMap model), crates/svn/src/report/drive.rs (entries through the
new call), and the index tests. If you'd rather hold the maps elsewhere (say per path line with versioned entries), or
if you're mid-change in index.rs, tell main and I'll follow your shape.
