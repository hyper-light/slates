# g300 — Mercurial import of an existing store (kiln UI goal 93, 51cef9f7), A-630 parts 2 and 3

Supervisor: main. Took over A-630 from g291 (its parts 2 and 3; part 1 landed by g291 at 066135675).
Worktree ~/slates-work/g300-hg-import, branch agents/g300-hg-import (pushed, at b3e1c0ba4). Red evidence: branch
agents/g300-hg-import-red (01f892747, stubs, not for main) and its worktree ~/slates-work/g300-hg-import-red, left in
place as g291 left its red branch. THE RULE read back to main before starting.

## Landed (2026-11-19 UTC): main b3e1c0ba4 (a5dffc913..b3e1c0ba4)
Commits: 028ba0c81 (fixture script), cb5455896 (import, file_index, strict requirements, plain encoding, tests,
fixtures module), 4d9967c40 (rustfmt's layout from the Studio), d06c7ee0e (design §4.17.3 status + piece 3, the
A-630 parts 2 and 3 amendment, GAPS, research), merges d04b78ee5 and a2eec04d9 (document conflicts only, both sides
kept, main first; research section renumbered §21), b3e1c0ba4 (merged run recorded).
- `slates-hg::import`: `import(files: &mut impl RepositoryFiles, store, room) -> Result<Imported, ImportRefused<R>>`.
  `RepositoryFiles` = the read-only seam (one whole file under `.hg` a call, given as path names; the server adapts
  §4.15's HostFs to it — owed). Walk: requires (+ store/requires under share-safe) → shape → listing (fncache or file
  index) → changelog, manifest log, each file log in path order, each one HgIndex group; links, manifests, listed and
  non-empty named logs, revlog numbers checked (7.2's verify); censored/ellipsis/extstored/unknown flags refused by kind;
  a refused import releases the new index and gives back every put hold (store left exactly as found).
- `slates-hg::file_index`: version 1 docket (60-byte fixed part + garbage list), used sizes, root checks, paths.
- `slates-hg::store`: `Layout::read`/`shape` strict over all 30 requirements 7.2 knows; `plain_encode`; `Encoding`.
- Fixtures crates/hg/fixtures/import (144 + SHA256SUMS), made on the Studio 05:41:16–05:41:37 UTC at 028ba0c81 by
  make-import-fixtures.sh: dot/hybrid/legacy stores byte for byte fixtures/store's (cmp), v2 (revlogv2 + changelog v2 +
  file index) and fileindex (revlog v1 + file index) stores whole, every revision's debugdata equal to fixtures/store's.
  Deterministic via 7.2's HGTEST_UUIDFILE and devel.fileindex.garbage-timestamp=0.

## Runs (Mac Studio adas-mac-studio.local-2, kiln CLI, sync off, GitHub clone, CARGO_TARGET_DIR ~/.cache/g300-a630-target)
- Red 06:07:57–06:08:18 UTC (load 5.5–7.8): 203 passed, 21 failed, every failure an assertion (all new tests but the
  property test, plus part 1's golden, whose requirements reader was stubbed).
- Green 06:12:15–06:12:32 UTC (load 3.5–3.9): 224/224 at cb5455896, first build; rustfmt patch applied (4d9967c40).
- Merged 06:19:44–06:19:53 UTC (load 11.6–13.0): 249/249 at a2eec04d9, `cargo fmt` clean.
- Studio cleaned: ~/.cache/g300-a630-target (617 MB) and every ~/.cache/g300-* removed.

## Findings (research §21)
- A fileindex-v1 store is named by dotencode's encoding without the dotencode requirement (7.2's makestore).
- 7.2 reads an absent .i as an empty revlog (`_get_data`); `hg init` writes no changelog; verify errors on a named file
  log that is empty or missing.
- 7.2 refuses revision flags outside bits 6..15 ("incompatible revision flag").
- Sibling found, not fixed (A-588 part 2's, report to main): index.rs says log/node rows "never meet an object row's
  20-byte key", but a file log row of a 13-byte path is 20 bytes ('L','f',13 bytes,NUL,4-byte rev). A pusher who finds
  a node beginning `Lf`, with a NUL at byte 15 and a small number in its last four bytes (about 2^48 SHA-1 tries, by
  arithmetic, not measured), then pushes a file named by bytes 2..15 to that many revisions, makes the two keys equal,
  and the second row written is refused RowDiffers: a denial of service on that path. It matters only if object rows
  and log rows share one Index (a quarantine's commit into `HgIndex::rows()`); today nothing writes Mercurial object
  rows there, and the import writes log and node rows only.

## Left (owed, in GAPS)
- Phase roots, bookmarks and markers beside the logs (pieces 5, 7); until then an import holds every changeset, secret
  ones too, and must not be served.
- The import sliced under the shard's step budget and resumable after a host's pending read; revlogs read by ranges
  rather than whole; a rebuild starting from the previous text (7.2's revision cache). A-645's 1 TB target makes these
  due before any large store is imported.
- A whole verify (a manifest's file nodes against the file logs).
- The server's adapter from §4.15's HostFs to `RepositoryFiles` (one name at a time; a non-UTF-8 name, possible only
  under the plain encoding, refused there).
- Run requests filed: run-requests.md "2026-11-19 g300 main b3e1c0ba4" (clippy, xtask, doc, cross-target).

## A-649 (UI goal 93, from main 2026-11-19): the import's rest, four parts
### Landed: part 1 — main 9c7c05e29 (80def213e..9c7c05e29), worktree ~/slates-work/g300-a649-tags
HgIndex keys tagged by kind: `L` log, `N` node, `O` object (`HgIndex::commit_objects` writes a quarantine's objects
tagged and gives its holds back; `HgIndex::object`; `Refused::NotANode`), so a 13-byte path's 20-byte log row can never
meet an object's id. Red (agents/g300-a649-tags-red d947583cc): 2 failed. Green 251/251 (ceb19f2bd), merged 256/256.
### Landed: part 2 — main ae8a9e38a (a2f20ce13..ae8a9e38a), worktree ~/slates-work/g300-a649-states
phaseroots read strictly as 7.2's `_readroots` (malformed line refused `PhaseRoots{line}`, unheld root dropped and
counted, public root passed over); bookmarks leniently as `bmstore` (malformed lines skipped+counted, unheld nodes
dropped+counted, later wins), from `.hg/bookmarks` or the store under `bookmarksinstore` (store.rs `Shape.bookmarks_in_store`);
both returned typed in `Imported` (`phase_roots`, `bookmarks`, counts) for the register, since index rows are never
rewritten; markers checked whole and kept as `M`+number rows (`append_markers`, `markers_len`, `marker`), each alone in
the obsstore's format. Fixtures crates/hg/fixtures/state (36 + SHA256SUMS, run 06:49 UTC at fdc52e9f1). Red
(agents/g300-a649-states-red 18f0ba467): 5 failed. Green 261/261 (8b395716b), merged 270/270. Research §23.
### Plan for the rest (if a successor takes it)
- Part 3a: revlogs by ranges. Seam: `RepositoryFiles::len(path)` + `read(path, offset, len, room)` replacing whole-file
  reads (small files read 0..len). revlog.rs: one algorithm over a `Source` (index/data bytes by offset as Cow), the
  existing slice API kept as the slice source (tests unchanged), the import's ranged source for split and docketed
  revlogs, inline revlogs whole (7.2 keeps them under 128 KiB). Refusals of the ranged source carried typed.
- Part 3b: the import as a state machine (`Import::advance(files, store, Turn { clock, quantum_ns })` -> Progress),
  units: one revision's text, one group's append, one listing step; no borrow held between turns; a `Clock` trait of
  slates-hg's own (git's shape, crates/git/src/store/learn.rs). git's upload (crates/git/src/store/upload.rs) is the model.
- Part 4: crates/server adapter from §4.15's HostFs (crates/vfs/src/host/mod.rs) to RepositoryFiles; SimHost tests.
### Landed: part 3a — main a1932c3ee (b982c336a..a1932c3ee), worktree ~/slates-work/g300-a649-ranges
Seam: `RepositoryFiles::len(path)` + `read(path, offset, len, room)`; small files read 0..len. revlog.rs: one
algorithm `Outline` over `Source` (index/data bytes as Cow); `Slices` (whole, borrowed) behind the unchanged `Revlog`
facade; the import's `Reader` reads split revlogs and docketed files by ranges (entry/chunk per read, within the length
at open or the docket's sizes; docket files held to hold their sizes first), inline revlogs whole. Rebuild cache:
`Outline::text(source, rev, cached, room) -> (text, from_cache)`, counted in `Imported::rebuilt_from_cache`. Red
(agents/g300-a649-ranges-red b7ae56ed3): 1 failed. Green 271/271 (db6b2fa13), merged 276/276. Removed part 2's two
test warnings (unused PhaseHead import; import fixtures' unused `file`). Others' warning seen: slates-merge
`source_of` never used (crates/merge/src/engine/copy.rs:535).
### Landed: part 3b — main 2ca3f94fb (9caa5055d..2ca3f94fb), Windows x86_64 jupiter: red 2 failed (43e391354), green 278/278 (f77f36847); the macOS run owed
Green agents/g300-a649-slices f77f36847 (worktree ~/slates-work/g300-a649-slices), red agents/g300-a649-slices-red
43e391354 (a turn's quantum ignored). Built as designed below: `Import::start`/`advance(files, store, Turn{clock,
quantum_ns})`/`turns`/`units`/`into_imported`/`abandon`, `Clock`, `Turn`, `Progress`; one revision read, kept and
appended a unit; `import()` the facade. Tests: `an_import_a_unit_a_turn_equals_the_whole_import` (dot, v2, states),
`an_import_ended_midway_holds_nothing`, proptest `damaged_stores_never_panic_and_leave_nothing_held`.
### Part 4 — committed and pushed, NOT yet built
Green agents/g300-a649-adapter 5743ef190 (worktree ~/slates-work/g300-a649-adapter, based on part 3b's f77f36847),
red agents/g300-a649-adapter-red 9f9ad5a21 (len/read stubbed to find no file). `crates/server/src/hg_base.rs`:
`HgBase<'h, H: HostFs + ?Sized>` over a lent host and the repository's directory (`.hg` opened under it, each name
under the one before); NotFound -> None, other HostErrors -> `BaseRefused::Host`, non-UTF-8 name -> `BaseRefused::NotUtf8`
before any host call; a file opened once a lend, length by fstat at open, reads clamped to it and charged first
(`admitted`); held: the last file's directory + at most FILES_HELD=2 files (LRU), every other directory closed during
the walk; Drop closes all; `Opened{directories, files, reused}` counted. Wiring: `mod hg_base;` (cfg_attr dead_code) in
server lib.rs, `slates-hg` in root [workspace.dependencies] and server [dependencies], Cargo.lock's slates-server entry
gains "slates-hg" BY HAND (the Studio's `--locked` run checks it). Tests (7, in the module, SimHost): the oracle (states
+ fileindex through the base == the same files in memory, each file opened once, reused > files, nothing left open);
a sliced import lent the host afresh each turn == the whole; absent/wrong kind; non-UTF-8 without a host call; charge
and room; replaced-while-held read as opened (FIRST_LENGTH_CALLS=5 arms the replace before the first read_at); handles
bounded at DEPTH=32; a pending fetch carried through. Docs: §4.17.3 status + piece 3 line, amendment "A-649 part 4"
(its Tests bullet says "Pending the Studio's run": fill it), GAPS built line and owed line, import.rs owed list.
### Overlap with g258's A-662 (agents/g258-git-push, unbuilt; main's notes 2026-11-19)
A-662 adds `slates-hg`, `slates-svn` to the root [workspace.dependencies] and `slates-vcs` (moved from dev-deps),
`slates-hg`, `slates-svn` to the server's [dependencies], each right after `slates-git` (where part 4's lines are), the
same `"slates-hg",` lock line as part 4's plus `"slates-svn",`, and `mod vcs_store;` right after `mod vcs_http;` (where
`mod hg_base;` is). Merge rule from main: one line per dependency; if Cargo.lock conflicts, take main's lock
(`git show origin/main:Cargo.lock > Cargo.lock`), run `cargo update -w` on the Studio (remote-pair.sh's 5th argument
`update`), test with `--locked`, commit the lock that results; never hand-merge lock hunks. Its `VcsStores` holds one
shard content store and per repository an `HgStore { index, derivations }` (`VcsStores::mercurial(id)` lends both a
step): the daemon's import (owed) hands the imported index there; HgBase keeps none (docs say so since 2026-11-19).
### When the Studio answers (the plan, under main's load rule: CARGO_BUILD_JOBS=4, one cargo at a time)
1. `rm -rf ~/.cache/g300-target-red ~/.cache/g300-target-green ~/.cache/g300-run.*` there (left by the cut run).
2. 3b: `bash -s agents/g300-a649-slices-red agents/g300-a649-slices slates-hg < ~/.cache/g300-scratch/remote-pair.sh`;
   apply the rustfmt patch, fill 3b's amendment Tests bullet with the numbers, merge origin/main, diff stat, push.
3. 4: merge origin/main (3b landed) into agents/g300-a649-adapter, then red/green with CRATES="slates-server slates-hg"
   (red: slates-server only); `--locked` checks the hand-made lock line; fill its Tests bullet; land the same way.
4. File run requests for 3b and 4 (server clippy watch: hg_base's fns; the two known dead-code warnings are others').
### The part 3b design, as planned (kept as the record)
`Import` state machine: stages Requirements, Listing, Changelog, Manifests, CheckManifests, Files(i), PhaseRoots,
Bookmarks, Markers, Done; a unit = one revision rebuilt, put and appended at once (no group vector; HgIndex appends one
row a call, numbering continues; the whole import stays all-or-nothing by releasing the index on refusal), one
changeset's manifest check, one state file; `advance(files, store, Turn { clock: &mut dyn Clock, quantum_ns })` runs
units until the quantum is spent (at least one), no borrow held between turns; `abandon(store)` gives back all;
`import()` kept as the thin facade running it to Done (tests unchanged). New tests: a sliced import (a clock that ends
every turn after one unit) equal row for row to the whole import, its turns counted; abandon mid-way leaves the store empty.

## A-667 (bug from main, 2026-11-19; g304 found it): LANDED main cde634359 (b79ece5d5..cde634359), Windows x86_64 jupiter: red 6 failed, green 284/284, merged 284/284 (slates-merge gate removed in the clone only: main's break c370dddbe, reported); A-641's push-check oracle stated the old rule too, corrected; the macOS run owed
Green agents/g300-a667-branch-heads a02a5a9dd (worktree ~/slates-work/g300-a667, from main 018d32ba2), red
agents/g300-a667-branch-heads-red 5c88fd3e9 (tests alone; worktree ~/slates-work/g300-a667-red). `branch_head_revs`
(crates/hg/src/serve/discovery.rs) now ends a revision a later one on its branch descends from (branchmap.py 422-424),
via `Edges` + per-branch walks of 7.2's `uncertain` (`end_reached`, `mark_ancestors`), `RevSet::remove`; A-619's
oracle in serve/tests.rs corrected; new serve/discovery_tests.rs (g304's history, two histories measured against
Homebrew's 7.2 where 7.2's shortcut keeps a head, a closed head, a generated oracle, non-vacuity from fixed seeds);
docs/bugs/2026-11-19-hg-branch-heads-were-judged-by-a-child-on-the-branch.md, §4.17.3 status, amendment, GAPS fixed
entry (Tests bullet says pending: fill it). Build order when main says the Studio answers: 3b, then part 4, then A-667
(`slates-hg` only), landed alone. Siblings: check:updated-heads fixed with it; reject_new_heads/head count owed with
the landing (topological open heads, not branch heads); obsolete-visible heads owed (view has no obsolescence).
Main's rule now: no Studio watchers; main messages when it answers.

## Part 4 — relaunched g300 (fresh agent, 2026-11-19 ~16:15Z onward)
Readback of THE RULE sent to main. Worktrees ~/slates-work/g300-a649-adapter (green) and -red2 (red), both clean.
- Merged origin/main e3234e618 into green (3d89f0fb2, no conflicts; the GAPS "deletion" seen first was only main's newer
  merge paragraph, 71f05f93d). The test count was 8, not 7.
- Added (0b6399f22, pushed): three tests for guards nothing pinned: `a_file_grown_or_cut_while_held_is_read_within_its_length_when_opened`
  (SimHost write_in_place; the clamp), `a_refused_length_closes_the_file_it_opened_and_is_carried_through` and
  `a_read_answered_in_pieces_is_read_whole_and_a_pending_piece_refuses_it` (test host `Fetching` over SimHost: S3-like
  pieces + PENDING past `cached`, fstat refusal once; the S3 host's `read` really stops at its first uncached chunk);
  module doc + status + amendment bullet + GAPS for the short-read rule; rustfmt 1.98.1 layout applied LOCALLY with
  ~/.rustup/toolchains/1.98.1-aarch64-apple-darwin/bin/rustfmt (formatting only, no build).
- Red: agents/g300-a649-adapter-red2 c818eacc2 = green 0b6399f22 merged under the stub (diff = stub only).
- Mutations (one per guard) in ~/.cache/g300-scratch/mutants (make_mutants.py), script remote-a649-4-win.sh (assemble.py).
  Expected failures: m01 drop: oracle, lent, absent, bounded, refusal-carried, refused-length; m02 walk-close: same six;
  m03 dir-close: oracle, lent, absent, bounded, refusal-carried; m04 eviction: bounded; m05 reuse: oracle, replaced,
  grown; m06 clamp: charge, replaced, grown; m07 utf8-late: not-utf8; m08 file-open errors: absent, refusal-carried;
  m09 dir-open errors: absent; m10 fstat-close: refused-length; m11 swallow: pieces; m12 one read: pieces.
  Red (stub): 23 + all 11 hg_base tests. Green: expect 23 failures, all main's.
- Left after the run: fill the amendment's Tests bullet (still "Pending the Studio's run") + status sentence on the
  mutations; land (fetch, merge origin/main, diff stat, markers, push HEAD:main); run-requests (macOS run owed, clippy
  watch hg_base); report to main.
## Part 4 — g300 relaunched again (2026-11-19, from main e3234e618; no readback per main's brief)
- Checked: both worktrees clean (green 0b6399f22, red2 c818eacc2, both pushed); origin/main still e3234e618 (green's
  merge base); remote-a649-4-cmd.txt decodes to remote-a649-4-win.sh, which embeds the 23 baseline names and the 12
  patches as now on disk; every patch applies to green's hg_base.rs (`git apply --check`).
- Run r86 on jupiter (kiln, sync false, disk=15g), started 17:09 UTC — dirs ~/.cache/g300-a649-4-src/-logs (fresh),
  target ~/.cache/g300-target (kept).
- r86 at 18:01 UTC: cold build still in dependencies (123 crates; other agents' cargo on jupiter too).
- Merged origin/main f607f424c into green while r86 builds: 70ce44708 (pushed). Main's commits since e3234e618:
  slates-svn (A-656 part 1, continued (1), A-616 continued (5)) and slates-git A-640 part 3; one conflict, both new
  amendment sections at the log's end, main's first then A-649 part 4. `git diff --stat origin/main HEAD` = the piece's
  8 files alone (the "A-656 deletion" seen before the merge was only main ahead). No merged run (the brief: merge, land).
## Part 4 — g300 relaunched a third time (2026-11-19 ~19:15Z; brief: Studio run of hg_base + slates-hg filter + fmt, merge, land as one commit plus the merge)
- Found green 70ce44708 pushed, with the predecessor's uncommitted GAPS rewrap (one 141-column line the piece made):
  committed 4d321e691 and pushed. Merged origin/main 0c7a77bd3 (incl. CLAUDE.md §8's new focused unit+integration rule)
  into green: dd80b3e7f, pushed; one conflict, the amendment log's end (main's four sections first, A-649 part 4 last);
  `git diff --stat origin/main HEAD` = the piece's 8 files alone; no markers.
- Integration tests exercising the adapter: none exist. `hg_base` is a private module (`mod hg_base;`, not
  re-exported) with no caller outside its own tests; slates-hg has no tests/ dir; no crates/*/tests/*.rs names
  slates_hg, hg_base, HgBase, RepositoryFiles or Mercurial (only two wire_order.txt files match). Told main.
- Studio run r10 (kiln, d1323898, sync false, CARGO_BUILD_JOBS=2, CARGO_TARGET_DIR ~/.cache/g300-target) of dd80b3e7f
  started 19:19:23 UTC: `cargo test --locked -p slates-server --lib hg_base -- --test-threads=4`, `cargo test --locked
  -p slates-hg --lib import -- --test-threads=4`, `cargo fmt -p slates-server -p slates-hg` (patch returned). It removed
  ~/.cache/g300-target-green and -red (the predecessor's cut run).
- Landing plan: worktree ~/slates-work/g300-a649-adapter-land, branch agents/g300-a649-adapter-land from origin/main,
  `git merge --squash agents/g300-a649-adapter`, fill the amendment's Tests bullet with r10's numbers, one commit, merge
  origin/main if it moved, diff stat, markers, push HEAD:main. Then delete ~/.cache/g300-target on the Studio.
### LANDED: part 4 — main af520142f (acc2ac16f..af520142f), 2026-11-19 ~19:57Z
- Piece commit 37a85fe85 (one commit, squash of agents/g300-a649-adapter onto b1926d201, evidence filled), then four
  merges of origin/main as main moved during the landing: 8bdf05ec8 (a6306963e: A-657 part 3 made slates-server depend
  on slates-hg too; one entry per dependency, both reasons in its comment; lock main's own), f759df0a0 (3b357a256:
  A-651's `mod vcs_ssh` beside `mod hg_base`, slates-ssh before slates-hg), 3bec5945f (1b480bd70), af520142f
  (acc2ac16f: A-662's `mod vcs_store` beside `mod hg_base`, the slates-hg comment naming A-657, A-662 and A-649 part 4).
  Every conflict was a comment, an adjacent module line or the amendment log's end (main's first). Landing worktree
  ~/slates-work/g300-a649-adapter-land2 (branch agents/g300-a649-adapter-land2, pushed). Superseded and left as is:
  ~/slates-work/g300-a649-adapter-land (a staged squash on 5c0a522f1, uncommitted, nothing unique; removing it needs
  --force, main's call).
- Runs (macOS arm64, Mac Studio, kiln, CARGO_BUILD_JOBS=2, rustc 1.98.1):
  r10 (dd80b3e7f = piece over 0c7a77bd3) 19:19–19:22Z: slates-hg --lib import 22/22, fmt clean; slates-server's lib
    tests did NOT compile: main's break (vcs_http/tests/git.rs named PackRefused::Stalled, removed by A-640 part 3
    188314131; the test file came from main commit 1fb50b1be titled "wip"); main fixed it at 93bd6e807 (71a43cd97).
  r25 (bcf1ec4df over 5c0a522f1, a clone-only two-line patch past that break) 19:24–19:27Z: hg_base 11/11 (not used as
    evidence).
  r49 (1da08d51d over b1926d201) 19:32–19:34Z: hg_base 11/11 (645 filtered), import 22/22, fmt --check clean.
  r60 (cb73420e3 over a6306963e) 19:37–19:43Z: hg_base 11/11 (652 filtered), import 22/22 (275 filtered), fmt clean.
  r84 (3bec5945f over 1b480bd70, the landing tree but its last merge) 19:45–19:52Z: hg_base 11/11 (671 filtered),
    import 22/22, fmt --check clean. The last merge (acc2ac16f) touched nothing hg_base builds on (crates/vfs, hg, mem
    untouched; crates/vcs gained wildmatch alone), so r84 stands, as af520142f's message says.
- Integration tests reaching the adapter: none (private module, no caller outside its tests); told main in the report.
- Docs: the amendment's Tests bullet filled (r49's numbers, all 11 tests named); §4.17.3 status says macOS 11 of 11, red
  owed; GAPS's unbacked "red first" (r86 on jupiter was never recorded: kiln had collected it) replaced by the evidence.
- Studio cleaned: ~/.cache/g300-target (9.9 GB) and every ~/.cache/g300-* removed. Scratch here: ~/.cache/g300-scratch.
- Owed (run request filed "2026-11-19T19:58Z g300 main af520142f"): other platforms' hg_base runs, clippy/xtask/doc, the
  red run (agents/g300-a649-adapter-red2 c818eacc2) and the 12 guard mutations (~/.cache/g300-scratch/mutants; their
  expected failures are listed above under "Part 4 — relaunched g300 (fresh agent...)"); hg_base.rs on main is
  byte-identical to 0b6399f22's, which they were written against.
- Left of A-649/§4.17.3 piece 3 (GAPS): the daemon's import of a repository's directory through the adapter, a turn at
  a time on the owner shard, its index handed to vcs_store's HgStore (A-662 has landed: `RepositoryStore::Mercurial`).

## A-675 (UI goal 93; main's next piece 2026-11-19 ~20:00Z): the daemon imports a Mercurial repository's directory
- A-675 reserved in amendments.tsv (next free A-676). Removed ~/slates-work/g300-a649-adapter-land (main allowed --force).
- Worktree ~/slates-work/g300-a675-hg-import-daemon, branch agents/g300-a675-hg-import-daemon (pushed): b25cbaefe (the
  piece, branch commit), eea0749ef (merge of origin/main 54075b520; one conflict, the amendment log's end).
  Red: ~/slates-work/g300-a675-hg-import-daemon-red, agents/g300-a675-hg-import-daemon-red 69995faf9 (begin -> NoStore,
  turn -> None), not for main.
- Shape: crates/server/src/vcs_store/hg_import.rs (child of the store map, uses its private lend/settle and fields):
  Importing {import, host: Box<dyn HostFs>, directory, reserved} in VcsStores::imports; begin (empty Mercurial store,
  Busy, room = one client region's bytes charged as retention, Import::start through HgBase); turn (lend 0, advance,
  make_room(used - charged), settle; Done -> index in the empty index's place; refused -> room back); run (yield + turn
  via with_state); abandon (free calls it first). Tests vcs_store/hg_import/tests.rs (5): oracle vs one-call import with
  the shard's window; refused midway (00manifest.i cut) gives back retained; NoStore/Busy/NotEmpty; freed midway; the
  shard's task (audit_daemon). hg_base's tests: mod/Files/STATES/laid_out made pub(crate). No verb (§4.15 designs none):
  owed with durability (register entry + recovery). Docs: §4.17.3 status + piece 3, amendment A-675, GAPS; three
  RUN-PENDING markers to fill from the run before the landing commit.
- Studio run r160 started ~20:40Z: red `--lib vcs_store::hg_import` (5 must fail), green `--lib vcs_store::` and
  `--lib hg_base`, fmt --check; target ~/.cache/g300-target (red's deleted in-run).
- Integration tests: none reach the import (no verb; nothing calls begin outside tests).
### LANDED: A-675 — main 9d685a9bc (b04c552b0..9d685a9bc), 2026-11-19 ~20:35Z
- One piece commit f1311ebd9 (squash of agents/g300-a675-hg-import-daemon onto main 7d020df47, the run's evidence
  filled) and one merge 9d685a9bc (main b04c552b0, A-651 part 2; conflict only the amendment log's end). Landing
  worktree ~/slates-work/g300-a675-land (branch agents/g300-a675-land, pushed to main only).
- Correction to the entry above: r160 started 20:16:27Z, not ~20:40Z.
- r160 (macOS arm64, Mac Studio, kiln, CARGO_BUILD_JOBS=2, rustc 1.98.1): red 69995faf9 `--lib vcs_store::hg_import`
  0 passed, 5 failed (each at its first begin: Err(NoStore) vs Ok(())), 20:17:25-20:20:02Z; green eea0749ef (main
  54075b520 + piece) `--lib vcs_store::` 18 of 18 (A-662's 13 + the 5 new), 687 filtered out, 20:20:51-20:23:28Z, load
  9.9; `--lib hg_base` 11 of 11; `cargo fmt --check -p slates-server` clean; no warning. The landing merged main twice
  more (7d020df47, b04c552b0); neither touched vcs_store, hg_base, slates-hg, the host seam, the budget, the runtime,
  the config or slates-vcs, so r160 stands (said in 9d685a9bc's message).
- Integration tests: none reach the import (nothing calls begin outside its tests until its verb).
- Studio cleaned: ~/.cache/g300-target (3.7 GB), the red target and every g300-run dir removed.
- Owed (GAPS §4.17.3, design A-675 "Owed"): the verb that asks for a directory's import and answers when it ends; the
  import made durable (the register's first entry with Finished's phase roots and bookmarks; a recovery that imports
  again or replays); a host's pending fetch retried at the next turn; the adapter's handles held between turns.
- Run requests filed: run-requests.md "2026-11-19T20:35Z g300 main 9d685a9bc".
- Left in place: worktrees ~/slates-work/g300-a675-hg-import-daemon (green, pushed) and -red (red, pushed, not for main).

## A-678 (UI goal 93; main's piece after A-675): the import verb, answered when the import ends
- A-678 reserved (next free A-679). Worktree ~/slates-work/g300-a678-import-verb, branch agents/g300-a678-import-verb:
  d0fb2060f (the piece, branch commit), 6d9251fca (merge of main ee5cbed4c; conflict only the log's end). Red:
  ~/slates-work/g300-a678-import-verb-red, agents/g300-a678-import-verb-red 20821ad20 (import_with -> NotFound,
  refusal_of -> ContentUnavailable), not for main.
- Shape: ipc RequestBody::ImportRepository{repository,path} (tag 90), ReplyBody::RepositoryImported (77),
  Refusal::ImportRefused{fault} (78), new enum ImportFault (NotMercurial, NotEmpty, UnderWay, Unawaited, Format,
  Damaged{path}, PastRoom{room}, Pending, Abandoned), registered in tests/wire_order.rs; api Invalid::Import(ImportFault)
  + re-export; server repository/import.rs (import_with seam for tests; owner-only via owned_repository; OsHost::open_root;
  hg_import::begin; reply held: Answer{origin,id,route,path} in VcsStores::answers, acceptance_deferred; task
  answer_at_end -> run -> record_completion in a db txn -> told::deliver_to OUTSIDE with_state; resend rejoins;
  refusal_of/reply_of mapping); verbs.rs route+dispatch+counter name, base_open_refusal pub(crate); told::deliver_to
  pub(crate); hg_import::room_of pub(crate). Docs: §4.17.3 status + piece 3, amendment A-678, GAPS (3 RUN-PENDING).
- Studio run r31 (started ~21:30Z after Claude Code's restart): red repository::import (4 must fail); green: wire_order
  publish (patch comes back base64 -> apply to wire_order.txt), wire_order test, repository::import, vcs_store::,
  slates-api --lib error (compile check of the api arm), fmt --check.
- Next: apply the wire_order patch; fill RUN-PENDING; landing worktree from origin/main, squash, one commit (+merge);
  push; delete ~/.cache/g300-target on the Studio; run-requests; registry; report to main; END.
## A-678 — g300 relaunched (2026-11-19 ~21:20Z; brief: the import verb's focused tests, land, END)
- Found: green agents/g300-a678-import-verb 6a1973542 (wire variants published from r31) and red 20821ad20, both pushed;
  ~/slates-work/g300-a678-land is a directory whose worktree record is gone (its .git names a missing
  g066-fs-server/.git/worktrees/g300-a678-land); its files are main's alone (no squash in it), its local branch
  agents/g300-a678-land = 3612d1ef9 (main), nothing unique: left in place for main. r31's logs were collected (kiln and
  the Studio's run dirs gone), so its numbers stand only in the predecessor's notes.
- Merged origin/main 3612d1ef9 into green: 3e487b4c4 (conflict only the amendment log's end, main first; numstat union
  exact: over main the piece's 45/1 design lines, over the branch main's 177/1). Main had changed slates-server
  (verbs.rs, listing, work_home, work_log) and slates-merge, so the focused tests run again.
- Review edits: b23c7ea51 (answer_at_end lends through with_state_counted; tests' never_opened answered panic! in a
  Result fn, denied panic_in_result_fn: now Err(StaleHandle)); 4fcd4a7a9 (api test
  an_import_refusal_is_typed_invalid_by_its_fault). Branch pushed at 4fcd4a7a9.
- Studio run r93 (kiln, d1323898, sync false, CARGO_BUILD_JOBS=2, CARGO_TARGET_DIR ~/.cache/g300-target, clone
  ~/.cache/g300-run-a678 deleted in-run, logs ~/.cache/g300-run-a678.*.log) started ~21:45Z: repository::import,
  vcs_store::, wire_order every_, api --lib error, fmt --check (patch base64 if not clean).
- Then: landing worktree from origin/main (b5231f0ff or newer), squash, fill RUN-PENDING (adapt a678-fill.py), one commit,
  merge if main moved, diff stat, markers, push HEAD:main; delete ~/.cache/g300-target + logs on the Studio; run-requests;
  registry; report to main; END.
### LANDED: A-678 — main 6a9d2f9ae (3b0b95379..6a9d2f9ae), 2026-11-19 ~22:10Z
- Piece commit db4298935 (squash of agents/g300-a678-import-verb onto main 0577ab0bd, evidence filled), merges
  decdd3377 (main 34d10acb5), ffc113234 (main 225d7eead), 6a9d2f9ae (main 3b0b95379; r186 stands: f05af5288 and
  1aa90e950 reach nothing the tests run). Landing worktree ~/slates-work/g300-a678-landing (agents/g300-a678-landing,
  pushed at ffc113234). Every conflict was the amendment log's end (main first, A-678 last), unions checked by numstat.
- Runs (macOS arm64, Mac Studio, kiln, CARGO_BUILD_JOBS=2, ~/.cache/g300-target): r93 (over 3612d1ef9), r110 (0577ab0bd),
  r152 (34d10acb5), r186 (225d7eead), each: repository::import 4/4, vcs_store:: 18/18, wire_order every_ 2/2, api error
  12/12 (11 + the new typing test), fmt clean. The Studio dropped twice for seconds at ~22:00Z ("not connected").
- Review edits in the piece: answer_at_end via with_state_counted; tests' never_opened no longer panics in a Result fn
  (panic_in_result_fn); api test an_import_refusal_is_typed_invalid_by_its_fault.
- Run requests filed "2026-11-19T22:12Z g300 main 6a9d2f9ae". Registry: A-678 landed; A-687 reserved (durable import).
- Left in place: ~/slates-work/g300-a678-land (a directory whose worktree record vanished; main's files only),
  agents/g300-a678-land (local, = 3612d1ef9, nothing unique).

## A-687 (main's ask 2026-11-19 ~21:50Z, straight after A-678): the import made durable
- Decision (from R8 and §4.17.1 "Durability and residency"): the import's record rides its register entry as a git
  push's objects do (a successor on another host replays the register and cannot read the importing host's directory),
  so recovery replays the record; re-importing the directory at recovery is not taken.
- Design facts read: RepositoryEntry {checks, joined, versions, names, state: StateChange::PhaseRootAdded.., objects};
  vcs_store::recover/replay_into refuse Mercurial records `Owed`; hg_push writes no objects yet (no Mercurial record
  exists); lines (`branch:<label>:<n>`) and bookmarks (`bookmark:<label>:`) are greens whose versions are works laid
  out by slates-hg push::tree and built by A-618's builder (HEAD_ATTRIBUTE = node); A-636 part 2a gives the state logs
  register classes. g303's served view (agents/g303-hg-view) has fixtures only so far.
## A-687 — g300 relaunched (2026-11-19 ~22:50Z; brief: import's register entry, phase roots, bookmarks as greens, replay at recovery; failing test first)
- No A-687 worktree or branch existed (the predecessor had only read). Removed ~/slates-work/g300-a678-land (an
  interrupted checkout of main 3612d1ef9: every file main's blob, one cut empty, 1070 never written) and its merged local
  branch agents/g300-a678-land. amendments.tsv already holds A-687 (next free A-690).
- Worktree ~/slates-work/g300-a687-import-durable, branch agents/g300-a687-import-durable from main b94ae6e90 (pushed).
- A-687 first Studio run r333 (23:55Z start 22:55Z, over main b94ae6e90): slates-hg --lib import 27 of 27 (the 5 record
  tests among them); server: every landing refused ContentUnavailable. Diag (agents/g300-a687-diag 3528cc682): the
  append's second Create refused -- every new branch of one entry minted one volume id. Reported to main; main ruled:
  fix it as its own piece first (A-694), then A-687 over the fixed main; and design A-687's record kind for imports AND
  pushes (g303's A-680 part 2b will record hg pushes with it; name the kind and its replay entry in the landing report).
- A-687 branch state: agents/g300-a687-import-durable at 44ae9880b = piece 09a495d9c + docs c3641f9c1 (RUN-PENDING
  markers) + two diag commits (3528cc682, 44ae9880b: diag tests in vcs_store/hg_import/tests.rs, NOT for main; drop in
  the landing squash).
## A-694 (bug, main's ruling 2026-11-19 ~23:15Z) — LANDED main 574185afa (fc5f4ea98..574185afa), ~23:56Z
- Piece commit 3bd7d093b (squash of agents/g300-a694-mint-ids onto main 2c3fa2bcc), merges a2657ea13 (075e36b52),
  58bfeca9d (9ca6e3883), 574185afa (fc5f4ea98); every conflict the amendment log's end, main first; unions checked
  against MERGE_HEAD (389/16, docs +37/+13). Worktrees ~/slates-work/g300-a694-mint-ids, -land.
- Fix: partition volume_counter (apply of VolumeCreated, snapshot field appended), VolumeId::counter,
  Db::volume_counter_at(offset), verbs::fresh_volume_id_at, push.rs plan mints each Create at its offset.
- Runs (macOS arm64, Mac Studio): r398 red (8dd016acd) server t_a694 0/2; green (d53177e26) db model t_a694+every_recovery
  3/3, db lib snapshot 8/8, server 55/55, green_checkpoint 6/6, fmt one rewrap applied; landing tree 3bd7d093b
  (23:38-23:45Z) 3/3, 8/8, 61/61, 6/6, fmt --check clean. Run requests filed 23:56Z.
## A-687 over the fixed main (2026-11-20 ~00:10Z)
- Diags removed (c7a1…); merged origin/main incl. A-694 and A-655 part 2b (fa6e05428: vcs_store keeps both arms,
  Owed removed); record redesigned per main (one kind for imports and pushes, magic SHGR v1: source byte, room, groups
  of revisions in appended order, markers; `import::record::write` (index), `write_revisions` (push), `replay(bytes,
  &mut index, store)` into a fork; Source::Import replays only into an empty index -> NotEmpty) (8edc833f9); red
  agents/g300-a687-red (replay_hg -> Diverged); green 9a28ff959. Studio run r473 started ~00:05Z (red t_a687; green
  hg --lib import, server vcs_store:: repository::import repository::push repository::hg_push git_push, fmt).
- Then: fill RUN-PENDING (design status, amendment Tests, GAPS), landing squash from origin/main, report naming the
  kind (`slates_hg::import::record`, SHGR, Source::Push, write_revisions, replay) for g303.
- A-687 GREEN and documented: branch agents/g300-a687-import-durable 6c492db95 (pushed). Runs: red r496 (agents/g300-a687-red
  d522589, t_a687 2 passed 1 failed: the recovery test); green 00:20-00:23Z a04f48e7e: slates-hg --lib import 28/28,
  slates-server --lib -- vcs_store:: repository::import repository::push repository::hg_push git_push 61/61, fmt clean.
  Docs filled (no RUN-PENDING left). Includes the fix to main's A-680 part 2 view test (lands the import; serves 3 = 7.2's).
- LANDING (if not done): `git -C ~/slates-work/g300-a687-import-durable worktree add -b agents/g300-a687-land
  ~/slates-work/g300-a687-land origin/main`; there `git merge --squash agents/g300-a687-import-durable`; resolve the
  docs' log-end conflicts main first; check `git diff MERGE_HEAD/origin --numstat` = the piece; one commit (area-prefixed,
  evidence); fetch, merge main if moved, diff stat, markers; `git push origin HEAD:main`. Report names the kind:
  `slates_hg::import::record` (magic SHGR v1; `Source::Push`; `write_revisions((Source, room), &[Group], markers)`;
  `replay(bytes, &mut index, store)` the one entry point; server `vcs_store::replay_hg`).
- STOPPED (context short) at ~00:30Z 2026-11-20: the landing squash onto origin/main (fetched ~00:28Z) conflicts in CODE:
  crates/server/src/repository/import.rs and repository/push.rs (main changed both after 574185afa), plus the design
  log's end. ~/slates-work/g300-a687-land (branch agents/g300-a687-land, unpushed, nothing unique) holds that conflicted
  squash: remove it (`git worktree remove --force`, main's call) and redo: merge origin/main into
  agents/g300-a687-import-durable first, resolve import.rs/push.rs, rerun the focused tests on the Studio (same commands as
  the green run above), then squash-land. ~/.cache/g300-target on the Studio kept for that run (delete after).
## A-687 — g300 relaunched (2026-11-20 ~01:00Z; brief: merge origin/main into the A-687 branch, Studio focused run, squash-land)
- Checked: nothing of A-687 on origin/main (a7b5e4292); branch agents/g300-a687-import-durable 6c492db95 = remote.
- Removed ~/slates-work/g300-a687-land (--force, main's decision): an untouched conflicted squash, its branch head
  a7b5e4292 = origin/main (on the remote). Local branch agents/g300-a687-land (= a7b5e4292) left.
