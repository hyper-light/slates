# g291 — Mercurial server (kiln UI goal 91, 51cef9f7dd5985fda2de4235a5f32653)

Supervisor: main. Clone: ~/slates-work/g291-mercurial-server, branch agents/g291-hg-design (worktree of
~/slates-work/g066-fs-server's .git). Scratch outside the tree: ~/.cache/g291-scratch (hg72-tests.txt: 7.2's 247
wire-protocol test names; commit message files).

## Landed
- A-499 (2026-11-19 UTC), main f19355908 (my commit 80daafb00; merges 8d70eb12b and f19355908): §4.17.3 "Mercurial"
  in docs/wip/SLATES_DESIGN.md (after g261's §4.17.1 shared mapping and §4.17.2 git), the A-499 amendment (log end),
  a GAPS block after g261's §4.17 entry, docs/wip/research/mercurial-server.md (every source to its file), and
  §4.17's status line naming §4.17.3 as written. Docs only: no code, nothing compiled (main's rule of 2026-11-18).
- Merge conflicts resolved twice: §4.17 (git's §4.17.2 kept before mine) and the amendment log's end (A-499 last).

## Decisions in §4.17.3 (built on §4.17.1, nothing redefined)
Each head of a named branch is a line (a green); bookmarks are greens; tags are `.hgtags` content; phases are
attributes that only move toward public (public never dropped/rewritten, secret refused); markers kept/exchanged,
refused on a public predecessor; topics (and review-marked bookmarks) are review requests = chains of works; stock
pushes judged whole-file, stale ones get Mercurial's own race abort; rebase on push only for clients sending both
pushback and obsmarkers; changegroup 01-03 taken, 04 sent only to clients naming delta-compression (7.2's code made 04
a delta-info header while its doc still says sidedata); HTTP v2 not served (7.2 has no server for it).

## Coordination
- inbox/g261-git-mercurial-subversion.md: four asks of the shared pieces (register entries carrying phase roots and
  markers; checks on greens a push does not move, for check:heads; a submit message + tool identity for server-made
  commits; one LFS store) and one question (a hook deadline's first value). Notes kept in a separate file; asked g261
  to point to it from research/vcs-servers.md §4.
- inbox/g292-a500-subversion.md: §4.17.4 goes after §4.17.3.

## Left (Mercurial's thirteen pieces, §4.17.3 "Pieces that follow", after §4.17.1's seven shared ones)
1 bundle2 + changegroup codecs (pure; golden vectors made by stock hg 7.2; hostile input) — the natural next piece;
2 SSH/HTTP v1 codecs; 3 objects + index + import; 4 read serving; 5 pushes; 6 server-made changesets, rebase on push;
7 markers, topics; 8 access + hooks; 9 revlog view (stream clone); 10 narrow/ellipsis/remotefilelog; 11 largefiles/lfs;
12 hgweb + linelog annotate; 13 main's runs (run-tests.py shim with expected-failure list, differential vs hg serve,
6.x client, tenfold measurements).
Open: whether a 6.x client's changegroup 04 is the old sidedata layout (inferred from 7.2's stale doc; the 6.x run checks).

## Run requests filed
run-requests.md, "2026-11-19 g291 main f19355908": xtask check (docs only), and the kernel → Mercurial conversion.

## Current piece (2026-11-19): A-514, Mercurial piece 1 — bundle2 + changegroup codecs, pure core
Encode + decode; golden vectors made once by stock hg 7.2 on the Studio (exact command recorded) and checked in as
fixtures; hostile tests (len = u32::MAX, truncated part, flipped bit, foreign magic, nested parts past their cap);
every cap from the data or a protocol-stated limit. Only compiling: `cargo test -p <crate> --lib` + `cargo fmt`.

## Landed (2026-11-19): A-514, Mercurial piece 1 — main c516c3494 (abd413701..c516c3494)
Commits: ad882e307 (the crate), c5a2cc725 (a test's expected length fixed), 9fe64d42a (design, GAPS, research),
merges 7472a3379 and c516c3494 (amendment log's end both times, A-514 last).
- New crate `crates/hg` (`slates-hg`): bundle2 (`HG20`), bundle1 (`HG10UN`), changegroups 01–05 and mpatch deltas,
  decode + encode, pure; every length signed 32-bit as 7.2 reads it; caps = the input + the caller's Admission (a
  counting pass charges the exact total, then one exact build; no recursion for nested interrupts).
- Golden vectors: crates/hg/fixtures (cg01–cg04 + 7.2's debugbundle listings + log), made once on the Mac Studio with
  `uvx --python 3.12 --from mercurial==7.2 hg` by fixtures/make-fixtures.sh (command in its header); SHA-256 checked.
- Tests: `cargo test --locked -p slates-hg --lib` 39/39 on the Mac Studio (debug, load 18.9–26.4), fmt --check clean;
  red-checked once in a disposable copy (3 guards removed → the 4 tests for them failed, 35 passed). Studio build dir
  and scratch deleted (337 MB).
- Findings recorded (research note §12): chunk length 4 refused by 7.2; the 03+ tree-manifest list always present (doc
  says otherwise); 7.2's debugbundle --all crashes on 04 full texts and lists no files of 03; mpatch's C rules.
- Slip to report: I created the branch with `git switch -c agents/g291-hg-codec` (forbidden; clean tree at origin/main,
  nothing lost). Use `git worktree add -b` next time.
- Run requests filed: clippy -p slates-hg, xtask check, cargo doc -p slates-hg.

## Left (Mercurial)
Piece 1's rest: the parts' payloads (phase-heads, bookmarks, check parts, obsmarkers V0/V1, pushkey values) and
compressed streams (GZ/BZ/ZS, after D-17's engines). Then pieces 2–13 of §4.17.3.

## Current piece (2026-11-19): A-545, piece 1's part payloads (UI 91 part 01a1f1211a4371af93494a6fb3bf603e)
phase-heads, bookmarks, check:heads, check:updated-heads, check:phases, check:bookmarks, obsmarkers (V0, V1), pushkey;
decode + encode; caps from input + admission; golden vectors from stock hg 7.2 (command recorded); hostile cases.
Worktree: ~/slates-work/g291-hg-payloads, branch agents/g291-hg-payloads (made with git worktree add -b).

## Landed (2026-11-19, g291 restarted): A-545, piece 1's part payloads — main ac2434f89 (22b4242ad..ac2434f89)
Worktree ~/slates-work/g291-hg-payloads, branch agents/g291-hg-payloads (pushed). Commits: ff2cd17e0 (predecessor's
unbuilt code and first fixtures), a16d0fc3a (lint-ready + main's gate findings), c926114d0 (first build: a borrow
error, rustfmt), 07acb28aa + 8fd42744f (fixture script cuts replies at the stream's end; executable), 05a7d163b
(fixtures made again + listkeys vectors and tests), cbabc7345 (design §4.17.3, A-545 amendment, GAPS, research §13),
merges 262b653a9, 5bc8f3d5e, ac2434f89 (amendment log's end both times: main's entries first, A-545 last).
- `slates-hg`: phases (phase-heads, check:phases), bookmarks (+check:bookmarks), heads (check:heads,
  check:updated-heads), obsmarkers (formats 0 and 1; opaque metadata `Entries`), pushkey (requests, the `reply:pushkey`
  part 7.2's server writes via `ReplyText`/`reply_part`, listkeys lines, hex nodes). Decoding allocates nothing; every
  encoder has `encoded_len`; refused encodes write nothing.
- Main's gate findings folded in: root no-panic attribute; `chunks_exact` with constant sizes gone (`as_chunks`);
  tests/bundle2.rs:446 and tests/changegroup.rs:151 split; `GeneratedPart` alias; tests/fixtures.rs `node` without
  string slicing. Clippy/xtask/doc re-check filed in run-requests.md for main.
- Fixtures: the script ran twice on the Studio; every file byte for byte alike but the three replies, now cut where
  their bundle2 streams end (the first cut kept the server's answer to `listkeys phases`, which 7.2's client sends after
  every push; 3 of 78 tests caught it). Those answers are now golden vectors of their own.
- Measured on stock 7.2 (probe, research §13): the C and pure format 1 marker readers disagree on a marker's size and on
  parent counts 0 and 4.
- Tests: `cargo test --locked -p slates-hg --lib` 80 of 80 on the Mac Studio (05a7d163b load 18.7; merged 5bc8f3d5e
  load 17.4); fmt clean. Not red-checked (one run per §8); offered to main as optional.
- Studio cleaned: ~/.cache/g291-a545 (368 MB), the probe dir, and the predecessor's ~/.cache/g291-scratch (its script
  copy's code matched the committed first version, md5 f2a6edb2… over non-comment lines) deleted.

## Left (Mercurial), after A-545
Piece 1's rest: compressed streams (GZ/BZ/ZS) once D-17's engines are wired. Then pieces 2–13 of §4.17.3: (2) SSH and
HTTP v1 codecs; (3) objects + index + import; (4) read serving; (5) pushes (the parts' handlers, incl. reply:obsmarkers
and reply:changegroup builders); (6) server-made changesets, rebase on push; (7) markers (format conversion, the
`obsolete` pushkey namespace's base85 dump keys) and topics; (8) access + hooks; (9) revlog view; (10) narrow/ellipsis/
remotefilelog; (11) largefiles/lfs; (12) hgweb + linelog annotate; (13) main's runs.

## Landed (2026-11-19): A-569, Mercurial piece 2, the protocol codecs — main 0d8a6e667 (520a86184..0d8a6e667), follow-up 3f02b3ea3 (b822f00f3..3f02b3ea3)
Worktree ~/slates-work/g291-hg-wire, branch agents/g291-hg-wire (made with git worktree add -b; pushed). Commits:
3164ba680 + 373b7167d (fixture script; first run stopped at the push, fixed), a3e6bd839 (codecs + golden tests),
f4b05da23 (rustfmt; decimal test borrows), 3775908f0 (design §4.17.3, A-569 amendment, GAPS, research §14), merges
1eeaeed8a, ee14dfc28, 0d8a6e667; follow-up e1eeb2106 (clippy type_complexity / collapsible_if pre-empted) + 77f4db7bd
(rustfmt), merged and landed in 3f02b3ea3. Red branch agents/g291-hg-wire-red (d89d89d82, stubs; not for main) and its
local worktree ~/slates-work/g291-hg-wire-red left in place as evidence.
- New modules: ssh (commands, arguments with lengths, dictionary, chunked input, string answers, push answer, the
  out-of-band error form, 7.2's request encoding), http (cmd, query / X-HgArg-<N> / X-HgArgs-Post fields in 7.2's
  precedence and dictionary order, payload, X-HgProto, 0.2 framing, push bodies, form and header splitting), capabilities
  (the line, hello, the bundle2 blob through both quotings), batch (escapes, calls, answers), wire (Spec, Arguments);
  shared quote, lines, decimal (bundle2's stream params and pushkey's listkeys/reply moved onto them;
  pushkey::ReplyText::new infallible, EncodeRefusal::NumberText gone).
- Bounds: lines by the server's own command table/spec and the room's digits; lengths and counts by the room; decoded
  bytes and vectors counted and charged before building; SSH reads say how many bytes they need and charge nothing
  until whole.
- Golden: 103 captures of stock hg 7.2 (fixtures/wire, make-wire-fixtures.sh), SHA-256 checked after the copy.
- Tests: 126 of 126 lib tests on the Mac Studio (f4b05da23, ee14dfc28, 77f4db7bd), fmt clean; red first by stubs, 37 of
  126 failing. Studio dirs deleted (449 MB + 397 MB).
- Findings (research §14): arguments arrive as the client's bytes over SSH and all three HTTP ways; header names travel
  lower-case; a bundle2 push sends heads=force (hex); 7.2 refuses a bundle1 client with application/hg-error over HTTP
  but `abort:` on stderr and a closed session over SSH (not the \n-\n form); getbundle answers 0.2 and chunked.
- Note: the worktrees share one .git with other agents' clones, so origin/main can move between my merge and my diff
  (it did once: crates/merge appeared reversed in the stat); fetch again and re-read the stat before every push.

## Left (Mercurial), after A-569
Piece 1's compressed streams (after D-17). Then pieces 3–13 of §4.17.3: (3) objects + index + import; (4) read serving
(getbundle's argument types; which SSH refusals use the out-of-band form); (5) pushes; (6) server-made changesets,
rebase on push; (7) markers and topics; (8) access + hooks; (9) revlog view; (10) narrow/ellipsis/remotefilelog; (11)
largefiles/lfs; (12) hgweb + linelog annotate; (13) main's runs.

## Landed (2026-11-19, g291 relaunched, UI goal 92 51cef9f7): A-588 part 1, piece 3's objects and nodes — main 553d3d155 (ad2751556..553d3d155)
Worktree ~/slates-work/g291-hg-objects, branch agents/g291-hg-objects (pushed). Commits: the predecessor's a9a221deb,
5a950c742, 0aedadb54 (fixture script), a0e0c1743 (first draft of the four modules), 4dd0c8f47 (rustfmt); mine ea134bdf6
(script on `hg debugdata`/`hg debugindex --debug`), 91f6f5852 (code + fixtures, WIP for the Studio), a6e415673 (rustfmt
+ the one E0716), a83eaace2 (design/GAPS/research + `\[B\]` tags), merges c8a25103f and 553d3d155 (amendment log's
end: main's A-602, A-607, A-591 first, A-588 last).
- `slates-hg`: `changeset` (decode/encode accept the same fields; empty user refused; empty timezone kept; extras
  escape/unescape as 7.2, a key twice not written), `manifest` (lower-case only, as written back), `filerev` (block
  apart; `copy(p1)` as 7.2's `renamed`: null first parent + both keys, half a pair = no copy; `parents_as_stored`, 7.2's
  issue6528 order; a split written back byte for byte; `pack` refuses a key twice), `node` (sha1-checked, detection
  on, safe hash off; `CollisionAttack`; shared `from_hex`/`write_hex`).
- Decided and approved by main (2026-11-19): sha1-checked rather than aws-lc-rs, since stock 7.2 hashes nodes through
  sha1dc (cext.c lines 36 and 83-84; the PyPI 7.2 printed `hashutil.sha1 is <class 'sha1dc.sha1'>`).
- Fixtures: crates/hg/fixtures/objects made twice on the Studio (03:52, 03:54 UTC), 42 files byte for byte alike, all
  SHA-256 checked after the copy (via `kiln run --no-sync` CLI to a local file + base64 tar; no hand transcription);
  crates/hg/fixtures/sha1dc = A-600's two SHAttered cuts.
- Tests: `cargo test --locked -p slates-hg --lib` 158/158 at a6e415673 and at 553d3d155 on the Studio; fmt clean. Red
  not run (one run); the draft's own tests asserted the opposite rules. Studio scratch dirs removed by trap.
- Run requests filed (run-requests.md, "2026-11-19 g291 main 553d3d155"): clippy/xtask/doc, cross-target check.
- Main's notes: A-607 (zlib-rs via flate2.workspace, zstd crate; import tests hold inflated bytes); main's slates-hg
  lint pass is coming (`[B]`/`[C]` escaped in module docs; write new tags as `\[B\]`); merge main before landing.

## Next (brief: straight on, no wait): A-588 part 2, the index's tables on §4.17.1's shared tables
Then the import of an existing store (zlib/zstd revlog chunks through A-607's engines). Scratch: ~/.cache/g291-a588.

## Landed (2026-11-19, UI goal 93): A-611, piece 3's import half, the revlog reader — main 6c7368dc6 (ce7f17865..6c7368dc6)
Worktree ~/slates-work/g291-hg-revlog, branch agents/g291-hg-revlog (pushed). Commits: 816910c28, d92b1f39d, 59bbf094
(fixture script and its two fixes), e7ab4ea78 (reader + tests + fixtures, WIP), 273e72893 (lock, rustfmt, zlib back
through flate2's reader, docs), docs commits, merges 757d09dc3 and 6c7368dc6.
- `slates-hg::revlog`: v1 inline (positions found once, charged) and split; v2 and changelog v2 via `read_docket` +
  `open_docketed`; chunks raw (u, NUL, plain), zlib (flate2 bufread, exact end by total_in, cut stream by flate2's own
  "incomplete deflate stream"), zstd (single frame, `finish()` empty), default mode = docket's engine on the whole
  frame; `chain` as 7.2's `_deltachain`; `text` via delta::apply, length + node checked; censored/ellipsis/extstored
  refused (owed).
- Fixtures: crates/hg/fixtures/revlogs (158 files; script run 3 at 59bbf094, 04:32 UTC; SHA-256 checked after the copy).
  Runs 1-2 recorded in the script header (v2 needs storage.fileindex.slow-path=allow; 300-byte texts gave no `snap`).
- Tests: 166/166 (first build green), 166/166 --locked + fmt clean (273e72893), 182/182 merged (757d09dc3).
- Wrong recollection corrected before landing: flate2 1.1.11's reader DOES refuse a cut stream (zio.rs); research §17.
- Run requests filed ("2026-11-19 g291 main 6c7368dc6").

## Next (main, 2026-11-19): A-588 part 2, Mercurial's index tables on slates-vcs (A-581 landed on main at 0a4f1bbfc)
Start from origin/main in a new worktree; port the parked draft (agents/g291-hg-index e292063e6: HgNaming over
node::compute_prefixed, log rows `L`+log key+rev BE, node rows `N`+node+log key, a log's length by doubling lookups,
groups numbered as 7.2's addgroup) onto main's slates-vcs API (check it against d6cc074c9's, which the draft used);
§4.17.3's index rows read the shared rule (g083 edited them). Golden: the objects fixtures' debugindex p1-rev/p2-rev/
linkrev. Then the import's store walk (fncache, path encodings, v2 fileindex).

## Landed (2026-11-19, UI goal 93): A-588 part 2, the index's tables on slates-vcs — main 81e812ac7 (6ce9e687d..81e812ac7)
Worktree ~/slates-work/g291-hg-index-main, branch agents/g291-hg-index-main (pushed), made from main after A-581
landed (the parked draft agents/g291-hg-index e292063e6, which merged A-581's branch, is superseded; left in place).
- `slates-hg::index`: HgIndex over one `slates_vcs::Index`: log rows (`L` + `c`|`m`|`f`path NUL + rev BE) and node rows
  (`N` + node + log key); `append_changesets`/`append_revisions` number a group as 7.2's `revlog.addgroup` (held node
  passed over; parents in the log or earlier in the group; link a changelog node; refused whole, nothing written);
  `len` by doubling lookups; `HgNaming` = Mercurial's node rule as the shared `Naming` (context = parents + framed
  block; object = bytes alone; `node::compute_prefixed`).
- Also: revlog chunks now inflated by A-598's `compression::inflate` (one inflater, main's ask; `Refused::Stream`);
  main's clippy findings fixed (changeset tests' `written`; revlog.rs Format markers; tests' named types and splits).
- Tests: 188/188, 188, 189/189, and 189/189 --locked + fmt clean merged (38e48e3df).
- Run requests filed ("2026-11-19 g291 main 81e812ac7"), incl. main's clippy re-run.

## Left (Mercurial piece 3 and on)
(3, rest) the import's walk of a store: fncache, path encodings (dotencode, the `dh/` hashed form), a v2 store's
fileindex; feed the revlog reader's revisions into HgIndex; censored/ellipsis/extstored revisions. Then pieces 4-13.
Index extras owed with pieces 5/7: phase roots, markers, branch heads.

## Current piece (2026-11-19, UI goal 93): A-630, piece 3's last part, the import's store walk (row registered by main)
Plan, in landable parts: (a) store path encoding (7.2's store.py: encodedir, _encodefname, _auxencode, _hashencode,
_hybridencode, _pathencode; the hash through node's sha1dc hasher) + fncache read (decodedir of the file's lines),
golden from stock 7.2 repos (long names -> dh/, case-colliding names, Windows-reserved names, dotencode and not);
(b) v2's file index (mercurial/store_utils/file_index.py) and dockets; (c) the import: each revlog through
revlog::Revlog into HgIndex (changelog, manifests, files; links by changelog rev -> node), read through a read seam
(trait in slates-hg the server adapts to §4.15's base), golden: every changeset node and every file's bytes equal to
hg debugdata. Worktree: from origin/main, ~/slates-work/g291-hg-store (agents/g291-hg-store).

## Landed (2026-11-19, UI goal 93): A-630 part 1, a store's names and its fncache — main 066135675 (01f0b6be5..066135675)
Worktree ~/slates-work/g291-hg-store, branch agents/g291-hg-store (pushed). `slates-hg::store`: `encode_dir`,
`decode_dir`, `encode_path(path, dotencode)` (7.2's `_pathencode` / `_hybridencode`, `_hashencode` via `node::sha1`),
`read_fncache` (admission-charged), `read_layout`. Fixtures crates/hg/fixtures/store (94 files, run 05:14 UTC at
b16ddf6): stores `dot/` and `hybrid/` whole (every revlog, fncache, requires), `*-files.txt`, `*-paths.txt` (hex, in
`hg manifest --all` order), and every revision's debugdata once: `00changelog-<rev>.txt`, `00manifest-<rev>.txt`,
`f<n>-<rev>.txt` (n = the path's line in *-paths.txt). Tests 195/195, merged 203/203 (b412d90). Run requests filed.

## Next (A-630 part 2, then part 3), for whoever takes it
Part 2, the import: a `StoreRead` seam in slates-hg (read a store file by its store path; the server adapts §4.15's
base read seam to it); walk: read requires (layout) → fncache → for `00changelog` (.i + .d), `00manifest` and each
fncache entry `data/<p>.i` (+ `.d` when present, encoded by `encode_path`) open `revlog::Revlog`; append to `HgIndex`:
changelog first (`NewChangeset`: text content, manifest node from `changeset::decode`, branch/closed from extras),
then the manifest log and each file log with link nodes = changelog node at each entry's linkrev, file texts split by
`filerev::split` into bytes + block; golden test over `fixtures/store/{dot,hybrid}`: every changeset's node and every
file revision's text equal to the fixtures' debugdata, both stores importing to the same index rows. Part 3: v2's
file index (`mercurial/store_utils/file_index.py`, `file_index_util.py`: docket `fileindex-v1` + list/meta/tree
files) and dockets per revlog (A-611's `read_docket`), fixtures needing `storage.fileindex.slow-path=allow`.
