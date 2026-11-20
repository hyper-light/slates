# g083-w1-under-1ms — progress (rewritten 2026-11-18 ~22:07Z after ~/.cache was wiped at 14:04 PST)

Goal: UI goal 87 (kiln ce6a2d3a), "a green's works served off its owner shard (W1, perf), so edit and merge stays
under 1 ms at 1,000+ clients", plus goal 79's 1,000-client part. Bound (A-297): every kind's p99 < 1 ms at >= 10^3
verdicts/s, 1,000 clients, 0 oracle failures, 0 given up/starved. From r155 (Studio): edit/submit p99 250-429 us at
the lightest load (3 of 4 runs), 3-35 ms at load 11-28; the gap is the p99 under load, A-273's target.

Clone: ~/slates-work/g083-w1-under-1ms (branch per piece, made at origin/main by git branch + symbolic-ref).
Scratch (after the wipe): ~/.local/state/g083-scratch/ (commit messages). Studio scripts: ~/.cache/g083-scripts/ there
(bounds2.sh; main's ab2.sh) if the Studio's ~/.cache survived; Studio build dirs are made per piece and deleted.

## LANDED (found by the relaunch at 22:14Z): A-653 continued (4) = part 2; main f05af5288..b94ae6e90
## (e4c6e7cbf ONE commit; merge b94ae6e90 of 6a9d2f9ae), pushed by the predecessor at ~22:07Z before the interrupt
- Checked 22:14Z: e4c6e7cbf's crates/ equal 55f3bb479's but for main's own f05af5288 edits (doc lines in
  upload/mod.rs and vcs_http/git/upload.rs, `pub const AGENT`, a test's doc comment); r101/r102 stand (its message).
- Filed now (the predecessor had not): run-requests 22:15Z (rerun over the landed tree; mutants L1-L6);
  amendments.tsv A-653 records continued (4). Reported to main.
- Cleaned: local worktrees g083-loose-search, -land, -red removed (remote branches kept, cited); the Studio's red/green
  clones and red target were already gone.

## 2026-11-20 ~01:00Z: main's piece "Mercurial inventory" -- DONE and sent; building the first missing piece LEFT for a
## fresh agent (context short). Read at origin/main ~00:55Z (session.rs COMMANDS + module doc, getbundle.rs parts,
## §4.17.3 wire surface / narrow / stream / hgweb, GAPS §4.17.3 "Owed, in order" at line ~1202):
- BUILT: discovery (heads, known, branchmap, branches, between, batch, hello, capabilities, protocaps), listkeys
  (namespaces, bookmarks A-690, phases A-691), getbundle bundle2 (changegroup 01-03, bookmarks, listkeys, obsmarkers,
  phase-heads), served view (A-680 p1), HTTP site on the daemon edge (A-650 p1, A-680 p2a), SSH serve --stdio (A-679)
  + --vcs-ssh (A-684), repository create (A-685), import (A-675/A-678), push machinery + hooks in-crate (A-641, A-657).
- IN FLIGHT: push over HTTP A-680 p2b g303; push over SSH A-696 g304; durable import A-687 g300; clone-over-SSH tests
  + P-256 g299; lookup g258 (next); Basic on loopback A-686 g258; interop issuer exports g309.
- MISSING: stream clones (revlog view, stream_out, stream2, streamreqs); clone bundles (clonebundles,
  clonebundles_manifest, get_cached_bundle_inline); hgweb json/raw + linelog annotate; `slates hg serve --stdio` CLI
  process (OpenSSH forced command); listkeys obsolete dump; learned response compression; held deltas / sliced answers /
  kept heads (perf); push follow-ons (acl identity, push records + replay, hg-worded refusals, pushkey compat hooks,
  sliced inflate, check:phases measure); server-made changesets + rebase on push; markers on push + topics
  (_exttopics_heads, tns_heads); narrow/ellipses/remotefilelog; largefiles + lfs; legacy bundle1 (changegroup,
  changegroupsubset); debugwireargs. By decision: hgtagsfnodes and rev-branch-cache parts not sent.
- RECOMMENDED first build (user impact, not blocked by pushes): stream clones in parts -- p1 the revlog view rendered
  from the index (revlog v1 generaldelta/sparserevlog, fncache, dotencode, phaseroots, bookmarks, obsstore) with stock
  `hg verify` as the oracle; p2 `stream_out`; p3 `stream2` + streamreqs + stream-preferred.

## LANDED 2026-11-20 ~00:40Z: A-691 phase roots; main 2b86aaaa1..23b566fd6 (b4d6bae40 ONE commit; merges 1adda73ea,
## 44d8ed9a3, c43d4ba4b, dbac43b3e, 23b566fd6). Queue from main (A-690 + A-691) DONE.
- Runs (Studio): r411 red 53/1; r421 green hg 91/0 + server 13/0; r451 over main 9ca6e3883 (Probed added) hg 91/0 +
  server 20/0; r466 over 1adda73ea hg 91/0 + server 21/0; server over c43d4ba4b 21/0; hg + server over dbac43b3e 91/0
  + 21/0 (slates-vcs changed); final merge of A-684 (docs/visibility only) stood on that run. fmt clean throughout.
- Filed: amendments.tsv A-691 landed; run-requests 2026-11-20T00:40Z (whole lib, red-checks a/b, gates).
- Cleaned: Studio ~/.cache/g083-target (10G), g083-hg-clone, g083-logs; local worktrees g083-hg-phaseroots(-final,
  -land) removed. Remote branches kept: agents/g083-hg-phaseroots, -run, -run2 (cited). Left on the Studio from older
  pieces: ~/.cache/g083-bins, -diag-src, -runs, -scripts, -src, *.orig (main's A/B may use them).
- Lesson: the MacBook's disk filled mid-write (write_text truncated SLATES_DESIGN.md to 0 bytes on the branch);
  write big files through a temp file + os.replace and check `git diff --stat` before every commit.

## (history) A-691 STATE ~00:2xZ 11-20: green r421 (hg serve:: push:: 91/0; server repository::hg_push vcs_http::tests::hg 13/0;
## fmt clean). DISK-FULL INCIDENT: a full MacBook disk truncated docs/wip/SLATES_DESIGN.md to 0 bytes in branch commit
## 467959c22 (branch only, never main); restored from 2f9bd0664 in 2b1231822 via temp file + rename (write that way now).
## Landing tree: worktree ~/slates-work/g083-hg-phaseroots-land (origin/main 9ca6e3883 + net diff f1fbd2c44..2b1231822,
## design conflicts resolved main-first, + Probed::draft_roots in crates/server/src/vcs_ssh/tests/hg.rs since main's
## A-679 added that implementor) pushed as agents/g083-hg-phaseroots-run 132c4dc0e (never lands). Run r451 over it:
## hg serve:: push::, server repository::hg_push vcs_http::tests::hg vcs_ssh::tests::hg, fmt. THEN: fresh worktree
## from origin/main, git diff --binary origin/main 132c4dc0e | git apply --index, check tree == 132c4dc0e, ONE commit
## with the real message (r411, r421, r451), merge if main moved, push; ledger, run request, report, END.
## (history below)
## IN FLIGHT 2026-11-19 ~23:5xZ: A-691 phase roots (worktree ~/slates-work/g083-hg-phaseroots, branch
## agents/g083-hg-phaseroots, base f1fbd2c44; pushed)
- 2e2bf49af scripts (+ `amended-root`: public root; draft root amended under evolution; draft made secret);
  c396a7391 red (fixture + golden a_hidden_draft_root_is_listed_as_7_2_lists_it); 2f9bd0664 fix (Repository::
  draft_roots; Indexed::still_draft between walk and filter; pull::draft_roots reads it; History/Served/push Views/
  server hg_push View + vcs_http Empty implement it; indexed_tests model phases()/listed_draft_roots() + named test
  a_hidden_draft_root_is_listed_and_one_under_a_secret_root_is_not).
- Repro by use (Studio, stock hg 7.2): hg log --hidden: 3 d6b2f99ba770 secret, 2 d57c1eb0d689 draft (amended), 1
  4226264b8244 draft (hidden), 0 c3daaa7ea5f5 public; phaseroots file "1 4226264b..., 1 d57c1eb0..., 2 d6b2f99b...";
  `hg debugpushkey <repo> phases` and over hg serve (127.0.0.1:58762): 4226264b...\t1, d57c1eb0...\t1. NB 7.2
  dropped the secret-made root from the draft roots (the "still draft" case is covered by the model test instead).
- Red r411 (Studio): `-p slates-hg --lib serve::` 53 passed 1 failed (golden at listkeys phases: d57c1eb0 alone).
- Green r421 running: hg `-- serve:: push::`, server `-- repository::hg_push vcs_http::tests::hg --test-threads=4`,
  fmt -p slates-hg -p slates-server. THEN: docs (bug record docs/bugs/2026-11-19-hg-a-hidden-draft-root-was-not-
  listed.md, design status after A-690's + amendment at the log's end, GAPS fixed entry), landing worktree from
  origin/main (git diff --binary f1fbd2c44 <tip> | git apply --index --3way), one commit, merge, push, ledger
  A-691 landed, run request, 3-line report, END.
- Fixture writer: ~/.local/state/g083-scratch/a691_write_fixture.py (all 22 files SHA-256 checked; SHA256SUMS 13a3bbde).

## QUEUE from main 2026-11-19 ~22:55Z (Ada's ask): two hg mismatches g303 found reading serve/pull.rs, each
## reproduced BY USE with stock hg 7.2 on the Studio first, failing test first, own commit + merge, 3-line report each
## LANDED 2026-11-19 ~23:15Z: A-690 divergent bookmarks; main b288b412e..f1fbd2c44 (9cd6cc6c7 ONE commit; merges
## c488b7b2f, 8afa1583c, f1fbd2c44)
- A-690/A-691 self-taken in amendments.tsv (brief named none). Fixture scripts make-state/make-served-fixtures.sh take
  repository names; new repo `divergent` (feature diverged in a clone, pulled back -> feature@1; @ on root).
- Repro by use (Studio, stock hg 7.2): hg bookmarks: @ 0:227d4fea737b, feature 1:e532f4e59b1a, feature@1
  2:bd211ad51c55; hg serve bound 127.0.0.1:54846; debugpushkey bookmarks: @ 227d4fea..., feature e532f4e5...; clone:
  `* @`, `feature`. slates r340 (red, fixture+golden only): 52 passed 1 failed, listkeys bookmarks had feature@1.
- Fix: crate::bookmarks::divergent (moved from push/classify.rs), bookmark_entries filters; serve oracle generator
  names `mark N@1` / `mark N@`, oracle states isdivergent literally. Green r356 93/0 (serve:: push:: bookmarks::);
  rerun over 8afa1583c (A-664 part 3 changed slates-merge) 93/0. Bug record docs/bugs/2026-11-19-hg-divergent-
  bookmarks-were-listed-to-peers.md; design status + amendment A-690; GAPS fixed entry. Run request 23:15Z.
- Transfer note: the Studio cannot push (no GitHub creds, https or ssh); fixture files came back via output (text
  raw, binaries base64, each SHA-256 checked) with ~/.local/state/g083-scratch/a690_write_fixture.py.
- 7.2 source lines (PyPI 7.2 on the Studio): bookmarks.py isdivergent 471-472, listbinbookmarks 475-484, listbookmarks
  487-491, _diverge 592-614; pushkey.py _namespaces 27-32; exchange.py _getbundlebookmarkpart 2638-2649; phases.py
  listphases 1049-1058 (cl = repo.unfiltered().changelog; roots in _phaseroots[draft] kept when phase(root) <= draft),
  _readroots 195-229 (unfiltered; unknown node dropped, dirty).
- NEXT: A-691 phase roots (worktree to make from origin/main: agents/g083-hg-phaseroots).

## LANDED 2026-11-19 ~22:50Z: main's piece "Mercurial interop tests" (UI goal 93); main e25799e8c..9f349d66b
## (126eb8a83 ONE commit; merge 9f349d66b); the history of the piece follows this list
- Took the predecessor's staged revision (rustfmt layout, main's rulings) + my edits as fa3df8f8f; ignore reasons in
  main's spellings: refusal waits on A-680 (g303) + A-686 (g258); HTTP round trip on A-685 (g305) + A-680 + A-686; SSH
  round trip on A-684 (g299) + A-685 + A-679 (g304; not in main's list, but the SSH site's hg command waits on it).
  port_on reads a port's digits alone (A-684's line names the host key after the port); SSH_USER -> HG_USER.
- Branch merge ddbded697 (main b94ae6e90; cli.rs conflict = both sides appended after g299's test: main's whole file,
  my 316-line block after it). Studio kiln r261 over ddbded697 (macOS arm64, rustc 1.98.1): fmt clean; plain 3 ignored
  with reasons; skip run (no hg on the path) 3 passed with skip lines; stock hg 7.2 --ignored 3 failed, each for its
  reason (unknown flag --vcs-ssh; repository create: unknown flag --tool; abort: HTTP Error 404: Not Found).
- Landing: fresh worktree agents/g083-hg-interop-land at b94ae6e90, `git apply --index` of b94ae6e90..ddbded697,
  one commit 126eb8a83; main had moved 7 commits (A-651 part 3, A-655 part 2b; no crates/cli, flags still unknown,
  DaemonSite still git-only) -> merge 9f349d66b citing r261 standing; pushed e25799e8c..9f349d66b.
- Filed run-requests 22:50Z (Linux optional; the un-ignore duty of whoever lands last; VCS_SSH_LINE for g299).
- Cleaned: Studio ~/.cache/g083-target (9.5G), g083-hg-clone, g083-logs; local worktrees g083-hg-interop, -land.
  Remote branch agents/g083-hg-interop kept (cited by the landing message).
- Reported to main (3 lines) with the reading findings: DaemonSite serves git's site alone (whoever routes hg's site
  there is what the refusal test waits on); g258's A-686 flips an_anchors_vcs_http_flag_starts_its_daemons_edge's
  `!head.contains("basic")`; A-679 named on the SSH test.

## (history) the hg interop piece before the relaunch of 22:14Z
- Worktree ~/slates-work/g083-hg-interop, branch agents/g083-hg-interop at 07d0c879d (pushed; base origin/main
  34d10acb5). crates/cli/tests/cli.rs (appended after g299's an_anchors_vcs_http_flag_starts_its_daemons_edge):
  STOCK_HG env SLATES_TEST_HG (Studio: `uvx --quiet --python 3.12 --from mercurial==7.2 hg`), stock_hg/hg_in/hg_ok/
  with_config/hg_nodes/anchor_with_edges/port_on/create_hg_repository/hg_round_trip; tests (all #[ignore] with reasons):
  stock_hg_without_credentials_is_refused_in_its_own_words, stock_hg_clones_pulls_and_pushes_over_the_vcs_http_edge,
  stock_hg_clones_pulls_and_pushes_over_the_ssh_edge. GAPS §4.17.3 line before its "Owed, in order" list.
- FINDING (sent to main ~22:2xZ): stock hg 7.2 CRASHES on the edge's Bearer-only 401 (stub probe on the Studio:
  ValueError: AbstractDigestAuthHandler does not support the following scheme: 'Bearer', urllib/request.py:1129 via
  hg url.py:418, exit 1, with or without auth.* creds). Other blockers: no CLI verb makes a repository (assumed
  `repository create NAME --tool hg`); NoStores (g303); daemon starts no SSH edge (assumed `--vcs-ssh`, line
  VCS_SSH_LINE) + A-679 (g304). Asked main for the auth decision and the real spellings.
- Studio r174: fmt; `cargo test --locked -p slates-cli --test cli -- an_anchors_vcs_http_flag stock_hg` (expect 1
  passed, 3 ignored); then with SLATES_TEST_HG `-- stock_hg --ignored --test-threads=1` (expect 3 failed, each for its
  stated reason). Target ~/.cache/g083-target (reused), clone ~/.cache/g083-hg-clone, logs ~/.cache/g083-logs/hg-*.log.
- Then: landing commit (fresh worktree from origin/main, cherry-pick -n 07d0c879d, real message), merge, push; delete
  the Studio target/clone/logs; run-requests (none heavy: Linux run of the cli target owed if main wants it); report.

## PAUSED BY MAIN 2026-11-19 ~22:0xZ, GREEN AND LANDING-READY, OWED: A-653 continued (4) = part 2, the loose path's
## objects past a window searched under the request's admission (main redirected g083 to the hg interop tests)
- Branch agents/g083-loose-search at 55f3bb479 (pushed): code 37985f58d + merge a33dccd81 (origin/main 3612d1ef9) +
  2783b6bfe (citations, doc fixes) + 55f3bb479 (design status block, amendment "A-653, continued (4)" with Runs, GAPS).
  Red branch agents/g083-loose-search-red dfe528437 (never lands).
- Studio (macOS arm64, rustc 1.98.1): r101 red `cargo test --locked -p slates-git --lib -- admission:: store::
  upload::` 129 passed, 3 failed (exactly the 3 new tests: left 40 vs 100; (90,90) vs (100,100); later file Whole vs
  Offset), 4 ignored. r102 green at 2783b6bfe: same filters 132 passed 0 failed 4 ignored; slates-server `--lib --
  vcs_http::git::tests vcs_ssh::git::tests --test-threads=4` 18 of 18; `cargo fmt -p slates-git -p slates-server --
  --check` clean.
- TO LAND (when main says): fresh worktree `git worktree add -b agents/g083-loose-search-land
  ~/slates-work/g083-loose-search-land origin/main`; `git diff bb535bd4d 55f3bb479 -- . ':!<files main changed>'` is
  NOT safe across the merge: instead `git cherry-pick -n 37985f58d 2783b6bfe 55f3bb479` (resolve the amendment log's
  end: mine last), one commit with the real message (area `git, server:`; cite r101/r102); merge origin/main; if main
  touched crates/git or crates/server's vcs_http/vcs_ssh or Cargo.lock since 3612d1ef9, rerun r102's commands first.
- Found: predecessor's pushed commit 37985f58d on agents/g083-loose-search (never compiled, not in this file): Admission
  hold/release (Hold token), Search::held_most + delta reserved whole, store::upload loose_large/Searching/search_unit/
  tried/searched, loose_unsearched counter, PackSource::write + Answer::write + Upload::advance take &mut Admission, all
  implementors (Fragments, test stubs, server NoPacks x2) and the HTTP/SSH edges adapted (SSH keeps the admission as a
  field now); tests: admission x2, store upload an_object_past_a_window_gets_a_delta_exactly_when_its_search_fits_the_admission.
- Done here: merged origin/main 3612d1ef9 (a33dccd81, clean); citations checked against the fetched source
  (~/.local/state/g083-scratch/git-v2.50.1-src-1795121997/pack-objects.c): 2541-2544, 3326, 253, 2913-2919 right; added
  try_delta's accounting 2774/2803/2813 and size filters 2737-2755; "nothing held whole" corrected; two deltas.rs lines
  rewrapped -> 2783b6bfe (pushed).
- Red branch agents/g083-loose-search-red dfe528437 (worktree ~/slates-work/g083-loose-search-red): today's decision
  (past a window sent whole) + release a no-op. Never lands.
- Studio runs: r101 red (`--lib -- admission:: store:: upload::`, target ~/.cache/g083-red-target, clone
  ~/.cache/g083-red-clone); r102 green (git filters, then slates-server `--lib -- vcs_http::git::tests
  vcs_ssh::git::tests --test-threads=4`, then fmt; target ~/.cache/g083-target, clone ~/.cache/g083-green-clone); logs
  ~/.cache/g083-logs/{red-git,green-git,green-server,fmt}.log. Delete all four dirs at the end.
- Left: design status block + amendment "A-653, continued (4)" + GAPS A-653 entry; landing as ONE fresh commit from
  origin/main (cherry-pick -n of the branch's net diff), merge, push; run-requests; report to main.

## HANDED OFF (context short) 2026-11-19: A-653 part 2 -- the loose path's objects past a window searched through
## store::Search under the request's Admission, the fall-back counted; NOTHING WRITTEN YET
- Worktree ~/slates-work/g083-loose-search, branch agents/g083-loose-search at origin/main 901be4f52, clean, never
  pushed; no kiln run started. A fresh agent should merge origin/main there first.
- Main's ruling: no new constant (not git's 512 MiB); the search's base, target and the base's index charged to the
  request's own Admission; an object goes whole ONLY when that admission can't hold its search, and that fall-back is
  COUNTED (non-vacuity); git's rule read from pack-objects.c v2.50.1's SOURCE with lines cited (fetch
  https://raw.githubusercontent.com/git/git/v2.50.1/builtin/pack-objects.c into a NEW EMPTY dir under
  ~/.local/state/g083-scratch/, not /tmp; cite get_object_details's big_file_threshold -> no_try_delta, try_delta's size
  heuristics (max_size = trg/2 - rawsz scaled by depth; trg_size < src_size/32), find_deltas' window and
  window_memory_limit). Failing test first: a large object with a good base in the pack gets a delta only if its search
  fits the admission. Encoder::put stepping through Search is the piece after.
- Read so far (main 901be4f52):
  1. crates/git/src/store/upload.rs `fn loose`: `if row.len > to_u64(cx.store.window())` -> `send_whole` (counted
     `loose_whole`), no delta trial; else `Encoder::put` with `named: None` (loose objects never get a named base).
  2. crates/http/src/admission.rs `Admission`: Copy; `new`, `left`, `charge` (refuses, takes nothing). NO give-back:
     a finished search's charge can only be returned if slates-http gains `release(bytes)` (else charges are permanent
     and over-count).
  3. The stream sees the request's admission only at `PackSource::begin(served, plan, admission)`
     (crates/git/src/upload/fragments.rs `Planned::of` charges the plan's lists); `PackSource::write(served, pack, out,
     room)` and `Upload::advance(repository, store, turn, out)` get none. `upload::serve(served, version, asked, packs,
     admission)` (crates/git/src/upload/mod.rs ~320) takes `&mut Admission`: check whether the Answer keeps it across
     its writes; if so, thread `admission: &mut Admission` through `PackSource::write` (implementors: Fragments, the
     stubs in crates/git/src/upload/tests/*, crates/server/src/vcs_http/tests/git.rs, the server's upload sources) and
     into `Upload::advance`.
  4. Candidates for a large object's base: earlier entries of its kind in the pack; large objects are sent whole today
     and not remembered (the encoder's `recent` holds bytes of objects <= a window). Remember large entries by
     (position, content, len, at, depth) and read them when searched; charge base + target + the index (Search's
     BTreeMap<u64, usize>: derive an entry's cost from the type) before reading, release after.
  5. The delta entry's header needs the delta's size first: hold the finished delta, then deflate it a window a unit
     (send_whole's pending machinery); an object whose search finds no shorter delta is sent whole as today.
  6. Test sketch: an unencoded version with two large similar blobs (> the tests' 1 KiB window); clone through the
     stream: today both whole (loose_deltas 0); with room, the second a delta (loose_deltas >= 1, read back exactly,
     each once); with an admission too small for the search, whole and the new counter moved.

## LANDED 2026-11-19 ~20:38Z: A-653 continued (3) = part 1 of "a delta search for an object past a window":
## store::Search; main 5a13886e4..22855c29e (51148ed82 ONE commit; merges 69d0cd50e, 22855c29e)
- Main approved part 1 and ruled part 2: no new constant (not git's 512 MiB); the search's buffers + the base's index
  charged to the request's own Admission (the upload stream's budget); a big object searched when the admission can
  hold it, sent whole only when it can't, that fall-back COUNTED (non-vacuity); git's rule checked against
  pack-objects.c v2.50.1's SOURCE with cited lines before relying on it. Main starts part 2.
- Built: crates/git/src/store/deltas.rs: Search { lens, blocks, stage Index{from}/Scan{at, rolling}/Done, out,
  inserts_from, counters }, step(source, target, budget) -> SearchStep::{More, Done(Option<Vec<u8>>)};
  SearchCounters { steps, indexed, scanned }; index_from (budget's whole blocks, >= 1, as_chunks::<COPY_BYTES>,
  or_insert first offset); Scan::run(blocks, (at, rolling), stop) -> Scanned::{From, End}; delta_of = one search at
  budget usize::MAX. Re-exported from store. Tests: crates/git/src/tests/store/deltas.rs (reference = the old one-pass
  search kept whole; bounded test 2,000-line text at 64 B a step; property over A-614's cut targets at random budgets;
  scan bound = budget + COPY_MAX + a block (the target's tail goes with the step that reaches it)).
- Studio: r190 red 123 passed 2 failed (both new tests), green 125; rustfmt 3 layouts (714a3afea); r201 over the merge
  with 1c6ac5006 (A-677 slates-merge) 125 passed, 0 failed, 4 ignored; fmt clean. Then main moved to 5a13886e4 (A-657
  part 4, hg + server only): merged without a rerun (stated in its message).
- Filed run-requests 20:40Z (whole lib 517 tests; mutants S1-S4). amendments.tsv A-653 updated. Studio target/logs
  deleted; worktrees g083-delta-search, -land removed.

## SCOPED (history) 2026-11-19 ~20:35Z: "a delta search for an object past a window" (A-639/A-653 owed)
- Main asked for: an object whose best base lies outside the (delta) window gets a delta, as git would. Read at main
  733e6de6b: the owed item is about SIZE. store::upload's loose path (objects of versions not yet encoded) sends any
  object with row.len > store.window() whole (send_whole, deflated a window a unit), with no delta trial and no named
  base (`named: None`); fragments (Encoder::put, store::Encoding) already delta every size. git (pack-objects.c,
  v2.50.1, from memory, to verify against the source): no_try_delta only past core.bigFileThreshold (512 MiB default); try_delta whole
  buffers (index of the whole base, random access to it), same type, max_size = trg_size/2 - rawsz scaled by depth,
  refused when trg_size < src_size/32; window 10 sorted by type, name hash, size.
- deltas.rs's delta_of: index(source) over every 8-byte block into a BTreeMap (O(source/8) entries), scan with a
  rolling hash and `agreeing` reading the source at any offset and the target up to 64 KiB ahead.
- Proposed split (sent to main): part 1, deltas::Search, the search sliced a bounded amount a step, byte-identical
  to delta_of (oracle), delta_of a driver over it; it also slices Encoder::put's trials (A-653 continued (2)'s owed
  "one object a unit"). Part 2, the loose path's objects past a window searched through it, which makes the stream
  hold a base + target + index whole (O(object), today O(window)): needs a derived memory bound (decision).
- Worktree g083-past-window removed unused (nothing written).

## LANDED 2026-11-19 ~20:25Z: A-614 part 3 SUPERSEDED by A-666 (goal 93, A-614's last part), docs only;
## main b13a6e05f..a6e688321 (2456ee4db the docs commit; merge a6e688321)
- Main asked: do A-666 part 2 (derived path history) and part 3 (blame) answer every path-limited walk the server
  makes? Read, not run, at main e50757a1a: YES -> part 3 recorded superseded (no code).
- The filters were planned (§4.17.2 "Operations on the server") only for slates' API `log`/`blame` of a path,
  narrowing versions to commits. A-645 "Queries by API" answers history/blame from derived data; A-666 reads one node
  a version / the file's spans, never walking commits. Unit = version (a ref's head move): a stock push's several
  commits are one version; commit-granular history inside a push is the client's own (git commit-graph
  --changed-paths); server-made commits are one a version (A-664).
- Walks checked: upload-pack (none by path; sparse:oid filters objects over trees; negotiation by generation);
  receive-pack (vcs_policy judges ref names; A-644 walks D(k) for merges/signatures); hooks (git walks the A-665 view,
  no commit-graph served); svn (log, get-file-revs, get-deleted-rev, drive: FSFS node history over revisions,
  crates/svn/src/reads/history.rs); hg (none); server verbs over A-666's types: none yet (A-666 owes them).
- Applied: §4.17.2 item 2 (rows keep no filter), "Operations on the server", piece 5's line, a status block, the
  amendment "A-614 part 3 -- superseded by A-666", GAPS A-614 entry, crates/git/src/store/mod.rs owed list (doc
  comment only; trailing-whitespace checked; nothing compiled). amendments.tsv A-614: complete. Worktree removed.
- Merge conflict: the amendment log's end only (main's entries first). No run requests (no code).

## LANDED 2026-11-19 ~20:10Z: A-653 continued (2) (goal 93): the fragments' encoding sliced (store::Encoding);
## main 545181b30..5c0967fb2 (9f76cd16e ONE commit; merge 5c0967fb2)
- Main started it ~19:52Z ("from A-653's owed list: the fragments' encoding sliced ... failing test first").
- Was: Repository::encode_fragments / rebuild_base read every object's bytes (to_encode), sorted (order), walked the
  trees (named_bases), then fragment::encode encoded all, then keep_both + install, in one call.
- Now: crates/git/src/store/encoding.rs (new): Encoding::fragments / Encoding::base, advance(repository, store, Turn)
  (upload's Turn/Progress: units until the quantum is spent, at least one), EncodingCounters (turns, units,
  trees_walked, rows_read, objects). Units: Walk (one tree pair), Gather (a window's worth of handles of rows into two
  BTreeSet<(Kind, Reverse(len), id, position)>), Encode (one object via the part's Encoder, learning into the
  repository's window/levels), Keep (keep_version: a version kept meanwhile keeps its own / keep_base with spent_ns).
  encode_fragments and rebuild_base = Encoding::...finish (one turn, quantum u64::MAX). Removed: named_bases,
  to_encode, order, Split, fragment::encode; shared: read, named, tree_entries (+ TreeEntries), in_fragment, handles.
- Tests (tests/store/encoding.rs): two versions of 300 files (second changes 150): a turn at quantum 0 = one unit, <= 1
  object, <= 1 tree pair, <= 128 rows; parts equal a one-pass twin's. Red = a first-state Encoding running today's
  encode_fragments/rebuild_base as one unit.
- Studio: r118 red 120 passed 2 failed (both new tests, one-object check), green 122; rustfmt asked two wraps
  (applied, 1e5fec6b4); r123 over the merge with 3329211e0 (A-662 store, A-671 vcs) 123 passed; r127 over the merge
  with 545181b30 (A-658 part 1 slates-merge) 123 passed, 0 failed, 4 ignored; fmt clean.
- Landing: fresh worktree at 1b480bd70, cherry-pick -n 7473b1dd2 095070635 1e5fec6b4, Runs numbers, one commit
  9f76cd16e; merged 545181b30 (5c0967fb2, r127 in its message); pushed 545181b30..5c0967fb2.
- Filed: run-requests 20:12Z (whole lib expect 498 passed + 4 ignored; lints; red-checks E1-E5, E3-E5 possibly unseen
  by the twin test). amendments.tsv A-653 line records continued (2). Cleaned: Studio g083-target (1.6G) + logs; local
  worktrees g083-fragments-sliced, -land. Remote run branch agents/g083-fragments-sliced kept (cited).
- Owed (stated in the amendment): one object, a tree pair and a part's put each still one unit whole (the content
  store has no put by pieces); the owner driving encodings by turns once it holds a git repository (A-625 part 3,
  nothing calls encode_fragments outside slates-git yet); a turn's span measured on the owner.

## LANDED 2026-11-19 ~19:48Z: A-653 continued (1) (goal 93): stock git's clone + fetches through Fragments from the
## store's parts; Streamed::counters(); main 3b357a256..71ebf2a74 (767da49a9 ONE commit; merges 47aa5243e, 71ebf2a74)
- Main started it ~19:35Z. Fresh worktree agents/g083-a653-clones-run at origin/main a6306963e: cherry-pick -n
  95e3efbd3 -> 128590e3a (red: counters a stub); cherry-pick -n 455b876dc + docs (A-653 continued (1) amendment,
  §4.17.2 A-653 status sentence, GAPS A-653 Owed) -> 7ddda1ea9; pushed. Fixture checked: small.git's clone request
  has 32 want lines of 31 commits, thin-pack, ofs-delta, no-progress, done.
- Studio kiln r62 (macOS arm64, rustc 1.98.1), one clone, red file swapped in with `git show HEAD~1:<file> > <file>`:
  `cargo test --locked -p slates-git --lib -- upload::` red 58 passed, 1 failed
  (a_pack_through_the_source_is_read_from_the_kept_parts, every counter 0), 1 ignored; green 59 passed, 0 failed, 1
  ignored, 439 filtered out (the four tests ok); fmt clean.
- Main had moved to 2e4433040 (A-626 part 3: slates-merge's built code, which slates-git links), so the run branch
  merged it (7e86c5ea2, pushed) and kiln r71 ran again over it: slates-merge + slates-git compiled afresh, 59 passed,
  0 failed, 1 ignored; fmt clean.
- Landing: fresh worktree agents/g083-a653-clones-land at a6306963e, `cherry-pick -n 128590e3a 7ddda1ea9`, the
  numbers in the Runs bullet, one commit 767da49a9; merged 2e4433040 (47aa5243e; message carries r71); main moved to
  3b357a256 (the SSH edge: slates-server on slates-ssh; nothing in slates-git's graph), merged as 71ebf2a74; pushed
  3b357a256..71ebf2a74. Each merge's conflict: the amendment log's end only (main's entry first, mine last).
- Filed: run-requests.md 19:50Z (whole lib every platform: expect 495 passed + 4 ignored, superseding 19:40Z's 491;
  red-checks). amendments.tsv A-653 line records continued (1).
- Cleaned: Studio ~/.cache/g083-target (1.4G) + g083-logs; local worktrees g083-a653-clones-run and -clones-land.
  Kept: the clone ~/slates-work/g083-a653-pack-source (its branch's unique commits have now landed; base for the
  next piece's `git worktree add -b`); remote branches agents/g083-a653-{pack-source,partial-parts,run,clones-run}
  (cited by the landings' messages).
- NEXT: none of A-653's left for an agent; owed per the amendment: the live clone over the HTTP edge (A-625 part 3's
  store per repository), a delta search past a window, the fragments' encoding sliced (main to assign).

## LANDED 2026-11-19 ~19:35Z: A-653 (goal 93): a part partly wanted read entry by entry, holders by row, one pack
## source; main 71a43cd97..b1926d201 (b9e46a1a3 the piece as ONE commit; merges b1522b248 (d0ceb580e), b1926d201)
- Relaunch 2 (~19:15Z, fresh g083; main's brief: land A-653's code alone, the four real-clone tests come after).
  Took agents/g083-a653-partial-parts (8b661dcfa = f06fe7ab1..252bdb5dd + merge of main 92ffda926, without the
  real-clone tests); squashed it onto origin/main cd84e3811 in a fresh worktree as agents/g083-a653-run 8decabe11
  (pushed; conflict: the amendment log's end only, main's A-616 continued (6) first); docs brought to the landing
  (A-640 part 3 landed at f607f424c, the third test, A-665's helper, new Runs and Owed bullets, GAPS Owed).
- Ada's rule via main (19:2xZ): focused unit AND integration tests. slates-git has no tests/ and no integration target
  reaches the stream or upload::Fragments (the server's vcs_http upload-pack tests lend a stub PackSource; the piece
  changes no API the server sees: Kept is pub(super), UploadCounters gained one field nobody outside builds), so the
  lib filters over upload and the pack source are the focused set.
- Studio kiln r22 (macOS arm64, M3 Ultra, rustc 1.98.1, load 14) over 8decabe11: `cargo test --locked -p slates-git
  --lib -- store:: upload:: v2::tests::fetch tests::v0` 116 passed, 0 failed, 4 ignored, 375 filtered out (slates-git
  compiled afresh; the three A-653 tests and hook_view's among the passes); `cargo fmt -p slates-git -- --check` clean.
- Landing: fresh worktree agents/g083-a653-land at cd84e3811, `cherry-pick -n 8decabe11`, the run's numbers in the
  amendment's Runs bullet, one commit b9e46a1a3; merged origin/main d0ceb580e as b1522b248 (amendment log's end the
  only conflict: A-672 first, A-653 last; resolver ~/.local/state/g083-scratch/resolve_append_conflict.py); the push
  was refused (main moved), merged 71a43cd97 as b1926d201 (a server test file only); pushed 71a43cd97..b1926d201.
  crates/git byte-identical to the run tree throughout; main's commits touched no slates-git source, no built code of
  its dependencies (slates-merge's copy_tests is #[cfg(test)]), no Cargo.lock or toolchain.
- Filed: run-requests.md 2026-11-19T19:40Z (whole lib every platform: expect 491 passed + 4 ignored; lints; the 11
  mutants M1-M8, X1-X3 as exact substitutions). amendments.tsv A-653 set to landed.
- Cleaned: Studio ~/.cache/g083-target (921M), g083-logs, g083-a653 (1.6G, the 11-18 mutation dir the OOM cut short);
  local worktrees g083-a653-run, -land, -partial-parts. Branches kept: agents/g083-a653-run and -partial-parts are
  cited by the landing's messages, so their commits stay reachable. Left on the Studio from older pieces (not this
  one; main's A/B may use them): ~/.cache/g083-bins (225M), g083-src (158M), g083-diag-src (157M), g083-runs,
  g083-scripts, three g083-*.orig files.
- DONE as A-653 continued (1), section above. Was: the four real-clone tests through upload::Fragments + `Streamed::counters()` (one line,
  `self.upload.counters()`), built on agents/g083-a653-pack-source as 95e3efbd3 (red: counters a stub) and 455b876dc
  (green); worktree ~/slates-work/g083-a653-pack-source. That branch still carries the pre-squash A-653 commits, so:
  a fresh branch from origin/main (`git worktree add -b`), only those two commits' changes (`cherry-pick -n 95e3efbd3
  455b876dc`), run `--lib -- store:: upload::tests::fragments::`, the amendment's Owed line, one commit, land.

## (history) A-653 before relaunch 2; branch agents/g083-a653-pack-source (455b876dc, pushed)
- RELAUNCH (~16:30Z, fresh g083): r22 had died while compiling slates-git (jupiter log ended at "Compiling
  slates-git", exit 1): no mutant results, definitions lost. Merged origin/main e3234e618 (789db078c; docs conflicts
  only: GAPS + design status + amendment log, both sides kept, resolver ~/.local/state/g083-scratch/
  resolve_a653_merge.py). New test 3b79b4c66 a_parts_wanted_entries_are_marked_over_units_and_read_over_words (384
  files, 256 wanted > one unit's 128 handles, 6 words of marks) + files_in_one_version helper.
- Mutants redefined (script ~/.local/state/g083-scratch/a653-jupiter-mutants.sh): M1 take_part empty-keep guard, M2
  Every-vs-Marked choice, M3 marking skipped, M4 wrong bit set, M5 Kept::order unsorted, M6 Marked reading ignores
  bits, M7 next_bit never crosses a word, M8 holders take nothing; extras X1 marking stops after one unit, X2 moved
  lookup never finds, X3 order charge dropped. kiln r119 on jupiter: green whole lib (expect 464 passed, 4 ignored =
  main's 462 + 2), fmt, then each mutant with `--lib store::`. Target $HOME/.cache/g083-target kept for reuse; logs
  $HOME/.cache/g083-logs (delete at the end).
- r119 found the merged tree did not compile: A-665's test helper git_part (tests/store/hook_view.rs) built a Kept
  without `order` -> fixed 252bdb5dd (identity order; every position there is 0). r130 started, then stopped when
  main said A-640 part 3 landed (f607f424c): merged 35f93560c (amendment-log conflict only), four real-clone tests
  appended to g296's crates/git/src/upload/tests/fragments.rs (streamed split as streamed_from; clone.rs FETCH,
  branches, small, refs pub(super)): red 95e3efbd3 (Streamed::counters a stub), green 455b876dc (self.upload.counters()).
  Pushed. kiln r143 on jupiter: whole lib (expect 495 passed, 4 ignored = main's 489 + 6), fmt, red (HEAD~1's
  fragments.rs, expect only a_pack_through_the_source_is_read_from_the_kept_parts failing), the 11 mutants over
  `store:: upload::tests::fragments::`. Script ~/.local/state/g083-scratch/a653-jupiter-final.sh.
- After r143: docs (amendment Applied/Tests/Owed, GAPS Owed, status sentence), merge origin/main, land, report.
- Done, committed: store::upload reads a part partly wanted entry by entry (Chosen::Marked, marking a window's worth of
  handles a unit via Kept::order, entries in position order, 4 B/entry charged; entries_judged counter); a holders
  stage after the head cover offers each still-wanted object its row's version's parts (partial clone's lazy blob
  fetches); tests a_blob_limit_clone_reads_a_partly_wanted_part_entry_by_entry + only_the_wanted_entries_of_a_part_are_
  judged; docs (§4.17.2 status, piece 7 line, amendment A-653, GAPS).
- Red: f06fe7ab1 (6 failed, 441 passed). Green: c6a9b6c35 slates-git --lib 447/447 (3 ignored), fmt clean (that tree
  had my StorePacks adapter + 4 adapter tests; 7299e2a5e dropped them per main: one adapter = g296's upload::Fragments,
  on agents/g296-git-v2, landing when the Studio returns).
- Studio offline since its OOM; per main, builds on jupiter (Windows x86_64, kiln f766f092..., MSYS bash, sync false;
  clone with -c http.version=HTTP/1.1 -c http.sslBackend=openssl -c core.autocrlf=false; CARGO_BUILD_JOBS=4,
  AWS_LC_SYS_PREBUILT_NASM=1, target outside the clone, one cargo at a time, delete clone+target after; fmt per crate).
  Green of a86f1d9a3 on Windows x86_64 (jupiter): slates-git --lib 443/443 (3 ignored) = 441 + my 2, fmt -p slates-git
  -p slates-vcs --check clean. macOS run owed. Red-checks N1-N4, N7, N8, N10, N11 running as kiln r16 (kiln restarted;
  r1402 lost). Local clone ~/slates-work/g083-a653-pack-source re-created (partial clone) after ~/slates-work/g083-*
  vanished in a cleanup.
- After g296 lands: merge main; add ~/.local/state/g083-scratch/a653_fragments_tests.rs as
  crates/git/src/upload/tests/kept_parts.rs (mod line in upload/tests/mod.rs; clone.rs's FETCH, branches, small, refs
  and fetch.rs's EXCHANGES pub(super)); add `Streamed::counters()` (4 lines) to g296's upload/fragments.rs; red first
  (one test asserting kept parts copied), green, land; update the amendment's Owed line.

## LANDED 2026-11-19: A-639 (goal 93): upload-pack's pack streamed from fragments; main 733da9dd5..659b6bf14
## (15c7763ed red, 0c74e1152 code, b965bd799/02a6ccad3 positions by type, e80c4506d/3f4cd19c8 tests, 2636d7978 and
## a later fix docs, four merges); branch agents/g083-a639-upload deleted; worktree ~/slates-work/g083-a639-upload
- What: store::upload (`Answered` trait over A-615's answer; `Upload::advance` in units under the owner's quantum; runs
  from fragments filtered with the want always; the cover of whole kept parts, a clone = base pack + fragments after
  it; the rest through fragment::Encoder one object at a time; thin/ofs-delta per client, entry-by-entry rewrites, a
  delta whose base the client lacks sent whole; BaseRebuilt, CountChanged). Base pack in two parts; a push's positions
  by type then id (rows still in key order); Positions set ops; vcs ContentStore::window_at.
- Evidence (Studio): merged 2f890e8af slates-git --lib 442/442 (3 ignored), slates-vcs 31/31, fmt clean; store:: 32/32;
  13 mutants red. Run requests filed (git index-pack/verify-pack over the stream's packs; gates).
- Left: a part only partly wanted read entry by entry (blob:limit); delta search past a window; the fragments' encoding
  sliced; PackSource (A-640's seam, "A-639 writes it") over the stream -- offered to main as the next piece.

## LANDED 2026-11-19 ~05:55Z: A-614 part 2 (goal 93): pack fragments per D(k) and the base pack; main
## a485a644e..58726c9bd (e5ed9a6b5, 0c576c8f5, 6dbde92ad, docs commit, merge 58726c9bd); branch deleted
- Evidence (Studio): merged 58726c9bd slates-git --lib 305/305 (2 ignored = main's verify-pack + double-hashing runs),
  slates-vcs 30/30, fmt clean; store:: 21/21; 11 mutations red (P3 depth and P9 named bases only after the tests were
  strengthened). Run requests filed (verify-pack command, double hashing, gates). Studio dirs deleted.
- Left: part 3 (changed-path Bloom filters); fetch's fragment choice within fast-forward runs (piece 7); REF chain depth
  across fragments; slicing; shared base packs across greens.
- Worktree ~/slates-work/g083-a614-fragments, branch agents/g083-a614-fragments, from main f68d32006 (lint pass in).
- Plan: store/learn.rs (Clock trait, Exchange = owner's price of a byte in ns; Window per rank: saved bytes vs trial ns,
  window = deepest paying rank + 1, first fragment tries every candidate; Levels per class: every zlib level sampled on
  one object a class a fragment; Depth = deepest chain the deployment's pushes carried (Incoming.depth), git's 4095
  cap; Rebuild = rent (clone time on fragments after C) or buy (last rebuild's time), first clone builds the first);
  store/deltas.rs (git delta search: block index at the copy instruction's 8 bytes, greedy extension, copies at most
  64 KiB); store/fragment.rs (D(k) split commits/trees/tags vs blobs, sorted kind/size desc/id; REF base = same path in
  the previous head's tree, found by a two-tree diff; OFS bases = window; zlib via flate2, CRC32 via crc32fast; entries
  at offsets as after a 12-byte pack header); base pack = reach(C) self-contained (OFS only); Repository::pack(...)
  assembles header + fragments + trailer via pack::Writer; oracle reads every pack back through unpack::read_whole.
- Also: replace prop_oneof! in the A-614 oracle with a weighted selector (lint habits); lines <= 100 columns; escape
  [A]/[B] doc tags.
- Filed: double-hashing measurement run request.

## LANDED 2026-11-19 ~05:05Z: A-614 part 1 = UI goal 93 (51cef9f7), §4.17.2 git piece 5 part 1, the git object store
## on slates-vcs; main 37c8412e6..d57d24364 (6b66211e9 code+tests, docs commit, merges e4d807b3f, d57d24364; remote
## branch agents/g083-a614-git-store deleted; worktree ~/slates-work/g083-a614-git-store)
- Parts agreed with main: part 1 (this), part 2 pack fragments + base pack (learned window/depth/zlib level/rebuild
  point on A-607's flate2 over zlib-rs), part 3 changed-path Bloom filters. A green made from a commit that is no
  version's head is piece 6's (receive-pack create). A-613 is g262's; next free A-615.
- Ada via main: "do not use mantle it is slow" -- nothing from Mantle was taken (only its README/crate list read).
- Built: `crates/git/src/store/` (naming: GitNaming over object_name, NameRefused::words in git's sha1dc words; positions:
  run-length sets; rows: 'o'+id object rows (kind, len, position, green, version, commit: tree pos, date, generation,
  parents pos) and 'p'+BE position rows; push: Entries trait + Incoming {kind, claimed, content}, Push (hash once,
  claim checked, slates-vcs hold_named), commits read at arrival; repository: seal (generations), admit (D(k) walk
  stopping at reached, trees skipped whole; fast-forward by generation; create/rewrite remade from the nearest green
  head below), commit (positions, rows as one insert_all, versions + head map charged, Moved/ForeignPush/NoMove), reads
  (head, tree, entry by path)). slates-vcs: Quarantine::hold_named, Quarantine::objects. ObjectId: AsRef<[u8]>.
- Evidence (Studio): store:: 14/14 at 6b66211e9; 11 mutations red (1-8 tests each), tree clean, green again; merged
  e4d807b3f: slates-git --lib 219/219, slates-vcs --lib 30/30, fmt --check clean. Filed run requests (clippy x3 OS,
  xtask, docs). Studio dirs deleted.
- Left: part 2 and part 3; walks/commit sliced under the shard budget with transient sets charged; versions' records as
  content; an `Entries` adapter for A-608's `Unpacked` (piece 6); trusting an in-process reader's names only by
  measurement (two hashes per object today).

## LANDED 2026-11-19 ~04:25Z: A-581 = UI goal 92 (51cef9f7), §4.17.1 shared piece 3, the object store; main
## 82c3e2be3..0a4f1bbfc (new crate crates/vcs `slates-vcs`; remote branch agents/g083-a581-object-store deleted)
- Predecessor (stopped by the usage limit) built a first core: 5857a297e, a87917455, 449200987 (pushed), plus uncommitted
  lint-shaped edits + docs, committed as 172977d29. Its elided `RowRef<'_>` in argument-position `impl Iterator` did not
  compile on 1.98.1 (E0658, r461) -> 337205b0c names the lifetime again; its design then 19/19 (r463).
- Redesign (d6cc074c9, fmt ca821c3a6, docs e6e1a1c69, merges f92d6945f, 0a4f1bbfc), against the brief's five bullets:
  - content plane: the store holds no content longer than the owner's window (D-6 chunk, `chunk_bytes(page)`); objects
    past it cut at a volume's window offsets under lists of identities to one root, admitted whole; windows plain BLAKE3,
    lists and tables BLAKE3 derive_key contexts (a pushed window equal to a table's bytes would otherwise dedup with it
    and keep its rows' holds alive past the table -- found while designing); `put_tables` one admitted step.
  - index: runs of <= one-window tables found by fences (charged); tail due to seal once its rows no longer fit one
    table, seals full tables only (`seal`), `seal_all` for forks/snapshots; `compact(j)` merges the newest j runs only
    this index holds; shared runs read as own again once no other index holds them (`relearn`).
  - merge point learned (`rule.rs`): one rent-or-buy problem per j (saving min(p, j) - 1 visits), largest due j; rents
    shift on a run added on top; restart on merge/fork/relearn; untimed lookups dropped when runs change.
  - `Naming { type Id: AsRef<[u8]>; type Refused; fn name(..) -> Result }` (git's `name_of` refuses SHAttered input);
    `AdmitRefused<R>` { IdMismatch, NotNamed{id, reason}, Collision, Content }; `CommitRefused` { Index, Content }.
  - every tunable: table size + tail length derived (the window), merge point learned; only 0/1/2 literals shipped.
- Decided NOT to re-home onto `slates_archive::ContentStore` (unbudgeted, trusts caller identities, pulls zstd's C into
  slates-vcs; no runtime per-shard content index exists yet): one content index for both is recorded as owed (piece 4 +
  Phase 7).
- Evidence (Mac Studio d1323898): `cargo test --locked -p slates-vcs --lib` 29/29 at ca821c3a6 and at merged 0a4f1bbfc;
  `cargo fmt --check` clean (local pinned 1.98.1 rustfmt gave the Studio's exact diff stat); 13 mutations each red (1-4
  tests), tree clean after. Tests split to <= 10 cognitive complexity counting each assert. Studio dirs deleted.
- Filed (run-requests.md 2026-11-19 g083): clippy -p slates-vcs --all-targets on 3 OSes, xtask check, cargo doc + xwin
  doc. Amendments.tsv A-581 line set to landed.
- Left (owed, in the design's status): one content index with the volumes' sealed windows (piece 4, Phase 7); merge /
  seal / release / abandon sliced under the shard budget; store + indexes on the owner shard timed by its clock, each
  tool's naming implemented (git over object_name; hg node, g291 A-588; svn rows from A-553 onto Index); content-defined
  table cuts (rows written apart sharing tables) and large-file CDC, each gated on measured gain; pruning; wire caches;
  fleet placement.

## LANDED 2026-11-19 ~02:50Z: A-567 = goal 85 B8 (FUSE takeover coherence), main 1f712cc28..b7081c388 (b118ecd54
## extraction + failing test, 6bd839e52 fix + docs, f8b4a40e6 test premise, 02c84831c run lines, merges; remote branch
## deleted). Studio: r138 red (Some([])), r258 red on the test's premise (write at 16 into an empty file = extension
## 0..48), r284 green (bridge-fuse coherence 14/14, lib 36/36, fmt clean). channel.rs (cfg linux) is main's to build.
## (Earlier notes below, kept.)
- Cause: channel.rs adopt_restored / adopt_unseen_handle adopted taken-over files fresh as of the mount's start (head at
  its first turn, AFTER the successor replayed the write log) -> replayed writes' bytes judged fresh.
- Fix: cfg-free coherence::{TAKEN_OVER_FROM = 0, TakenOver, taken_over, changes_since}; adopt from before the
  successor's first journal record with every change since (A-231's point). channel.rs (Linux-only, uncompiled by me)
  calls it. b118ecd54 = extraction unchanged + failing test (r138 red: Some([]) for Some([16..48])); 6bd839e52 = fix +
  GAPS + A-567 + bug record docs/bugs/2026-11-19-a-taken-over-mount-took-a-replayed-writes-bytes-as-fresh.md.
- Green run owed: kiln refused twice at ~04:00Z ("not connected to the kiln server").
- Left (stated): write log names no writer (own replayed bytes refused once); pre-publication undelivered changes need
  carried FUSE states (A-231 pattern).

## LANDED 2026-11-19 ~03:10Z: A-559, main efe8ea79c..22b4242ad (2ebf742fb tests first, b92f451ec stamps + split +
## bench, ebf18a48a epoch fix + fmt + run lines, merge 22b4242ad; remote branch deleted) -- goal 87, UI part
## 01a1f1208fc877f5bf27108824ba776f
- Slot's 4 padding bytes = producer's stamp (low 32 bits of monotonic_ns, bit 0 set; 0 = unstamped):
  slates_ipc::slot::{stamp_now, landed_ns}; endpoints stamp each push; Reply.stamp; ClientEnd/Client::request_stamp;
  Client::poll_reply_stamped (pending keeps stamps). slates_machine::attribution {Exchange, harness_part, Verdict,
  judge}; slates_machine::bench::longest_gap_ns. load_bench: latency_server/latency_harness/operation_server/
  operation_harness rows replace floor; per-agent harness accumulator + marks (attempt/op/cycle/round), held sends
  counted; count_harness widens on the host clock (Clock is epoch-relative!); noise row before each level (workers
  threads x 100 ms); bound row: server_worst, harness_bound, noise_floor_ns, met yes|harness|no.
- Studio: r52 failing first (3 attribution tests vs stubs); r80 green (machine attribution 3/3, bench 4/4, ipc lib 69
  +1 ignored, client lib 8/8, server lib 485 +3 ignored), fmt applied. The example itself is NOT compiled by me:
  filed for main (run-requests 03:10Z) with the ring suites, lint matrix, and the quiet-machine A/B.

## LANDED 2026-11-19 ~01:55Z: A-528, main 830ef03fe..ce9b6905e (33223939e removal + docs, d6b0c70ee run lines,
## merge ce9b6905e; remote branch deleted) -- goal 87, UI part 01a1f130f9797157b7d4ed0c360f6721
- Studio r175: server lib 456 passed 3 ignored, merge lib 3/3, mem lib compiled (budget 13/13), fmt clean in my files
  (main's shards_route.rs needs fmt: filed). Conflict at the merge: the amendment log's end only (main's A-500..A-523
  first, A-528 last). Filed 01:55Z: main's lint matrix, merge crate whole + Miri mem + green suites.
- A-491 was done by another agent (main's amendment log has "A-491 — An idle give-back step learns its slice from its
  ..."): my interrupted analysis below is superseded.
- NEXT is main's: profile what keeps norepl's p99 above 1 ms in the contended rounds (goal 87's next piece).
- Main's r100 (Studio, interleaved, 3 rounds z=20000/50000; ~/.local/state/slates-fanout/integrator-logs/
  ab-a502-r100-2026-11-19.txt): replicas lose everywhere but edit p50 (and rebase p50 at z=20): ReadHead/Submit p50
  26.3-56.9 ms vs 0.09-1.27; p99 110-302 vs 0.23-32.9 ms. A-502 did not rescue them.
- Branch agents/g083-a528-no-replicas (33223939e, pushed): deleted green_replicas.rs, merge replica.rs/store.rs/
  tests/replica.rs, mem sealed.rs; merge history.rs + lib.rs restored from 637ba6d78^ (only A-273 touched them since);
  engine.rs: each A-273 commit's engine hunks reversed newest first with git apply -R -3 (all clean); server call sites
  reversed from cc56e002e + 762294937 (one fmt-only conflict in daemon.rs: the stop step's impl dropped); Cargo.toml/
  lock (merge's slates-mem dep) and mem lib.rs + unsafe-budget (68 -> 63, grep count 63 confirmed) reversed; api
  stop_tests doc no longer cites A-457. Docs: §4.16 paragraph now the rejection, A-247 + A-273 status, A-528 amendment,
  GAPS A-273 closed + the owner's-one-thread line, BENCHMARKS record (r406 + r100 tables, ab2.sh command, hypothesis
  labelled: eviction + remake churn). Studio r175: merge lib, mem lib (budget filter), server lib whole, fmt.
- A-491 (bug 6) still interrupted (analysis below).

## LANDED 2026-11-19 ~00:30Z: A-502, main 069303a1d..6d9115aa4 (a92413357 test first, a7eb2e263 fix + docs,
## c74542031 fmt + run lines, merge 6d9115aa4; remote branch deleted)
- Studio r84 (debug): the test failed first on the old code ([2, 2, 2] feed messages before three replies, one per
  replica per version); red with the fix but each applied message acked: [2, 6] for [2, 2]; green 8 of 8
  (`--lib green_replicas`); fmt diff applied. Target dir deleted. Nothing else run (CLAUDE.md §8).
- Merge conflict: the amendment log's end only (main's A-488.. entries first, A-502 last).
- Filed 00:30Z in run-requests.md: the A/B again (same arms: norepl 95bbed246 vs main 6d9115aa4), main's lint matrix,
  the green suites.
- NEXT is main's: read the A/B; if still not green, the replicas come out (measured-and-rejected) as the next piece.

## (history) A-502 as it was started 2026-11-18 ~23:50Z: the submit regression the A/B found (goal 87)
- Main's A/B r406 (Studio, 3 rounds, Z 50/20 ms, load 11-22; table in
  ~/.local/state/slates-fanout/integrator-logs/ab-a273-repl-2026-11-18.txt): NOT green. Z=20 SubmitAccepted p99
  0.81/3.88/13.6 -> 60.8/88.9/78.0 ms, SubmitConflict 0.77/3.79/14.6 -> 74.5/101.7/89.6 ms, ReadHead 0.56/3.02/10.5 ->
  26.7/58.7/25.0 ms. gave_up 0, oracle 0.
- From code: per committed version the owner clones the increment and sends one `Message::Feed` to EVERY replica
  (shards-1 = 22 on the Studio) inside the submit's own turn, then runs one `Passed` ack task per replica per version;
  all on the owner's control path, so verdict asks queue behind them.
- RULE (main, 2026-11-18 ~00:05Z, Ada: "Why are ANY of these compiling?"): the ONLY compiling I do is
  `cargo test -p <crate> --lib <filter>` for my piece, plus `cargo fmt`. No clippy (any), no `cargo xtask check`, no
  xwin/--target checks, no doc builds, no Miri: file them in run-requests.md; main builds and lints everything.
- Built on branch agents/g083-a502-feed-off-path (pushed): a92413357 = counters (feed_sent, acknowledged) + the test
  `a_turns_changes_go_to_each_replica_in_one_message_after_the_turns_replies` on the OLD code (must fail); a7eb2e263 =
  the fix (queue_feed/arm_flush/flush/send_feed/send_turn; Message::Feed carries Vec<FeedMessage>; apply+replay a turn
  in order, one recharge, one Passed; close drops the queue; counter flush_unarmed) + design §4.16, A-273 status, the
  A-502 amendment (Tests/Checks bullets owed after the run), GAPS A-273.
- Studio run r84 (background): first (a92413357's file) / red (each message acked) / green (`--lib green_replicas`) /
  cargo fmt diff back as base64; target dir ~/.cache/g083-a502-target deleted at its end.
- If main's rerun of the A/B is still not green, the replicas come out and are recorded measured-and-rejected.

## INTERRUPTED (not mine any more unless reassigned): A-491 = goal 85 bug 6, recovery test
## `a_boot_give_back_holds_its_shard_a_slice_of_cpu_a_step` failed once under load on adacachy (r271), passed r327
- Read so far (no code changed): crates/server/tests/recovery.rs (the test and
  `assert_each_give_back_step_takes_about_a_quantum`), crates/server/src/daemon.rs (`give_back_recovery_by_steps`,
  `return_dead_recovery`, `give_back_slice`, `learn_give_back_rate`).
- Finding: the TEST judges each step by the shard thread's own CPU time (`registry::shard_cpu`), as the watchdog does.
  The PRODUCT learns its slice size from WALL time (`s.clock` around a step) -- under preemption that only makes the
  learned rate slower, so slices shrink, never grow; it cannot by itself make a step overrun in CPU.
- r271's failing assertion line is unknown (no log kept). Candidates to check with a log: the first window's give-back
  before any rate is learned (a default slice), the quiet window, a stall counted as a step, or madvise/munmap system
  time inflated by TLB shootdown IPIs under load (charged to the thread's CPU clock).
- Next step for whoever takes it: get r271's log or rerun under load with the assertion's numbers printed; a unit test
  with an injected slow clock / starved thread only after the failing line is known.

## Landed (all on origin/main)
- [x] Harness stamps sound (f6d37937b); A-381 (a1d99b6e0); A-388 (57ff29e53); A-400 (c0d386107).
- [x] A-416 the hot set is a set; the bound judges every kind and the cycle (af62ac08e).
- [x] A-421 workers park: landed 656826e26, REJECTED by its A/B (r155), taken back out at c8417f2f9 (lint fixes kept).
- [x] A-273 step 2, replica and feed (081f8b1a7); step 3, content in sealed blocks (48802d1ca).
- [x] The sealed-block leak fix (Miri, adacachy r241): main 218ad8d69..fbee3e79f (166d16d80). Rule: a reader's lifetime
      is bounded by its writer's; `SealedBlocks::end` refused while a reader is registered; a writer dropped with a
      reader frees nothing and is counted (`sealed::dropped_with_readers`). Bug doc
      docs/bugs/2026-11-18-a-sealed-block-writer-dropped-with-a-reader-kept-its-blocks.md. Studio: sealed 9/9, clippy
      mem clean, Miri sealed 8 passed 1 ignored no leak (r289).
- [x] A-273 step 4, the replica's bounds, derived: main 0b0df27c5..367ba28b0 (a2914e0fd, 61b7fcf53, 0549463f9,
      f70eaeabd, dd3452ddc, 367ba28b0). `replica_limit` = (shards-1) x 2 (none with one shard); `replica_budget` =
      min(per-green budget, shard room); `held_limit` = per-green budget - own retention; typed refusals; rebuild path
      tested; sealed tests end their writers; main's four complexity items split. Studio r316: mem sealed 12/12, merge
      lib 16/16, one-crate clippy merge + mem clean, Miri sealed 11 passed 1 ignored no leak; red (eviction past the
      held limit disabled): both held-limit tests + the slowest-reader test failed, the unfed one passed. Remote branch
      agents/g083-w1-under-1ms-bounds deleted.

## Run request re-filed 22:28Z (lost in the wipe): 21:58Z, main 367ba28b0 (A-273 step 4)
- `cargo test --locked -p slates-merge` (whole crate incl. `--test replica`); the server's green suites
  (`cargo test --locked -p slates-server --test green_retention --test green_checkpoint --test green_post_state
  --test green_changes --test merge_origin --test recovery --test works_off_owner --test verdict_pages --test checkout`);
  CI's Miri lane `cargo +nightly miri test -p slates-mem --lib`; the cross-lint lanes over crates/merge and crates/mem;
  CI's doc build (new intra-doc links).

## LANDED 2026-11-18 ~23:11Z: A-476 = the takeover chain bug (not A-273's), main 7113415eb..0ef784f19
- Cause: A-404's held checkpoint lets a holder's ledger positions 1..floor go; an adopted green's chain read
  (`Daemon::merge_chain_identities`) named the adopted versions from the ledger alone -> version 1 missing whenever a
  record the successor recomputed named version 2 committed (timing: 2 of 4). Engine/rebuild/durable state were right.
- Fix: `merge_origin::ledger_identities` (ledger record, else the held checkpoint's `accepted`); daemon.rs uses it.
  Test `a_successors_chain_names_the_versions_its_checkpoint_stands_for` (merge_origin.rs, A-404's helpers +
  `with_increment`). Bug record docs/bugs/2026-11-18-a-successors-chain-lacked-the-versions-its-checkpoint-stood-for.md.
- Studio r436: merge_origin lib 17/17, clippy server clean (main's batch fixed by now), xtask check: structural ok,
  literals 50 in others' files. Red (checkpoint names left out): [(2),(3)] for [(1),(2),(3)].
- Filed 23:12Z: the fleet test and its sibling x20 on Linux (adacachy) and macOS (Studio).
- Held per main: A-473's gap (replicas only where works ask) until the A/B is read.

## LANDED 2026-11-18 ~23:01Z: A-473 = idle replicas given back, main 9bc4011f2..38f0a26b7 (f0d962bdc, the fmt+checks
## commit, merge 38f0a26b7)
- green_replicas.rs: `Held.last_use_ns` + `gap_tail_ns` (longest gap between uses, first from holding) -> idle window;
  `serving` -> `note_use` arms the shard's one `idle_check` task (sleeps to the first `idle_at`, gives back what is
  past it with `Gone::Idle`, ends when no used replica is held); counters idle_given_back, route_idle,
  idle_check_refused; `Tomb {gone, owner, install}` on the replica shard; the first request after an idle give-back
  sends `Message::Wanted` once -> the owner marks `left[from] = Idle{wanted: true}` -> `ensure` remakes (REMADE).
  A never-used replica is not judged idle (no use to derive a window from) -- documented, owed with evidence.
- Studio r417: green_replicas 7/7 (new idle test), merge lib 16/16, clippy merge clean, clippy server clean in my code
  (main's sites only), xtask check: structural ok, literals 52 in other crates' files (none in mine). Red (never give
  back): the idle test fails (nothing given back within the budget).
- The A/B norepl vs repl is main's, running now (Studio target dirs slates-work-target-g083-norepl/-repl are main's:
  left alone). Heads-up from main: fleet test `a_successor_two_versions_behind_adopts_the_versions_between_and_serves_
  the_green` flaky on Linux (successor lacks version 1); main is bisecting across A-273 step 3; if it points at A-273,
  the fix is my next piece.

## LANDED 2026-11-18 ~22:50Z: A-465 = A-273 step 5b, main b3194636f..ef82f7340 (cc56e002e, af29bc700, the fmt+checks
## commit, merge ef82f7340)
- verbs.rs: the owner's answers over any engine: `base_files`, `base_page`, `base_sizes`, `rebase_on`,
  `build_increment_on` (the cores of owner_files/page/sizes, owner_rebase/map_rebase, build_increment).
- work_home.rs: `start_ask` asks the replica first (`answered_by_replica`: Files/Page/Sizes at the work's base version,
  Rebase from the work's base), the answer settled on the same shard by a message to itself (`answer_here`) as an
  owner's answer is; `RebaseAnswer: Debug`.
- green_replicas.rs: `serving` (holds the version: floor <= v <= head) and `route_reason` (route_absent / _behind /
  _refused / _evicted / _over_budget), `Gone` carried home with each registration, tombstones on the replica shard,
  `FedShards.left` and the remake in `ensure` (REMADE; never for budget), give_up asks every shard that held one.
  Counters: merge.replica.served (non-vacuity), route_*, remade.
- Tests (Studio): r390 first run: oracle answers equal but both rebases clean (inserts), gate test waited for journal
  len 1 (an edit making a file records more) -> fixed; r398: green_replicas 6/6 (oracle whole-answer compare, gate
  test: base read + rebase served, served +2, asked_owner unchanged; routes + remake test), merge lib 16/16, clippy merge
  clean (r390). Red (start_ask never asks the replica): gate test fails, (served, asked) (0, 3) for (2, 1).
- A/B arms pushed: agents/g083-a273-after = ef82f7340; agents/g083-a273-norepl = 95bbed246 (ef82f7340 with A-457/A-465
  code files restored from aedce71d0, green_replicas.rs removed; made in a git worktree, removed after).
- Run request filed 22:50Z (green suites incl. works_off_owner, server lib whole, merge whole, the A/B command).
- NEXT (main starts it): idle replica give-back; then main's A/B result decides.

## LANDED 2026-11-18 ~22:28Z: A-457 = A-273 step 5a, main aedce71d0..ae77935b0 (762294937, 2c8ae5244, be71c4418,
## merges 0c4e83457, ae77935b0)
- crates/server/src/green_replicas.rs (new): the owner makes a replica on every other shard (up to `replica_limit`)
  before the green's first change (`ensure`, from the submit ahead of its settle); feeds each version
  (`feed_version`, after `record_accepted`) and each fold (`feed_fold`, the settle's `fold_to_budget_and_feed`) over
  `xshard::hand_reply_home`; `hold_within` sets the held limit at each settle; a replica's shard holds it within
  `replica_budget(green_chain_bytes, Store::admittable)` and charges `charge_replicated` (arena grown first, A-98),
  trued up per message; refusal -> leave, counted; `give_up` retires a destroyed/fenced/re-installed/rolled-back engine
  until every replica left; `close` + `outstanding` used by `Daemon::give_back_replicas` in `stop_parts`. Messages
  carry (green, install); a dropped Deliver/Left/Leave is owed again via a thread-local queue. Counters
  merge.replica.{sent,full,held,over_budget,applied,refused,left,retired,unheld,not_carried,stray}.
  Config: `DaemonConfig::replica_limit` + boot notes replica_limit, replica_budget_bytes, replica_held_limit_bytes.
  Merge: `Green::replicas_registered`.
- Tests (Studio): r350 first run held no replica (budget.admittable() is 0 before the arena grows: A-98) -> fixed
  with `Store::admittable` + `make_room`; r354: server lib green_replicas 3/3, merge lib 16/16, clippy merge clean,
  clippy server clean in my code but FAILS on six sites of main's (fleet.rs make_in_turns, verbs.rs attach, a complex
  type near publish_delta_into, publish_delta_into, a live_ship.rs test closure, a verbs.rs test unwrap) -- reported;
  red (give_up drops at once): giving_the_owner_up_drops_no_reader fails, dropped_with_readers = 1 in the other test;
  r360 on the merged tree 0c4e834: fmt clean, 3/3, 16/16. Studio build dirs deleted (5.6 GB); remote branch deleted.
- Run requests filed 22:28Z (this one, and the 21:58Z step-4 one re-filed).
- NEXT (main starts it): step 5b (serving a work's rebases and named-version reads from its shard's replica; refused
  or evicted replicas routed to the owner, counted); then idle give-back; then main's A/B at 1,000+ clients.
Scope: boot-profile derivations (`replica_limit` from shards; budgets from partition caps + each shard's room, logged
with inputs); the owner makes a green's replicas on the other shards up to the limit, each charged to its own shard's
budget and fed the owner's changes over the cross-shard carrier; the owner holder makes every replica leave before the
owner is given up (destroy, fence, re-install, daemon stop); `sealed::dropped_with_readers` stays 0. Tests first: a
two-shard daemon's green gets one replica on the other shard fed to the owner's head; a one-shard daemon makes none
(R8); giving the owner up drops no reader. Red-check once. Design (§4.16, A-273, A-457), GAPS. Checks on the Studio:
`cargo test --locked -p slates-server --lib -- <filter>`, `cargo test --locked -p slates-merge --lib`, one-crate
clippy of server/merge/mem as touched, fmt. Serving reads from replicas is step 5b (not here).
Plan: new crates/server/src/green_replicas.rs (owner: fed shards, retiring greens; replica shard: held replicas and
their charge via `charge_replicated`); messages carried by `xshard::hand_reply_home` (kept until room, run even when
refused at admission); a guard gives a replica's registration home if it is dropped undelivered; feed at the submit's
accept (after `record_accepted`) and at the settle's fold (`fold_to_budget_and_feed`); held limit set at the settle;
`Daemon::stop_parts` closes every shard's replicas and waits until no owner holds a registration before the runtime
stops; engine/mem: `Green::replicas_registered` (SealedBlocks readers count).
