# g297-svn-reads -- progress (UI goal 93, kiln 51cef9f7; A-613 part 2's rest, taken over from g262)

Owner: Claude subagent g297 (supervisor "main"). Worktree ~/slates-work/g297-svn-reads (branch agents/g297-svn-reads,
pushed to origin). Sources: ~/slates-work/g297-sources/svn-1.15.0/ (Subversion 1.15.0 files from the GitHub mirror,
sha256 list in ~/slates-work/g297-sources/svn-1.15.0.SHA256SUMS). Transcript viewer: python3 -I
~/slates-work/g297-scripts/items.py FILE [MAX]. Commit messages in ~/slates-work/g297-msg/.

Scope (main, 2026-11-18, Ada's correction): server answers only -- get-file, list, get-location-segments,
get-deleted-rev, log (with include-merged-revisions), get-file-revs, then read access through A-606's authz.
`svn mergeinfo --show-revs` is DROPPED (the stock client computes it from get-mergeinfo, log and segments).

## Fixtures (all recorded already, one run, covering every remaining command)
- Recorder crates/svn/fixtures/reads/make-fixtures.py gained revisions 8-11 (big.txt 10,000 bytes and its r9 change,
  empty.txt, README.TXT, trunk/dir/b.txt re-added, branches/b1/a.txt replaced by copy of trunk/a.txt@5, r10 without
  author changing trunk/a.txt props, r11 removing tags/t1's mergeinfo) and sessions: get-file, hostile-get-file, list,
  hostile-list, get-location-segments, get-deleted-rev, log, log-merged, hostile-log-{word,path,seven}, get-file-revs;
  stock svn-cat, svn-cat-big, svn-ls, svn-log, svn-log-g, svn-blame, svn-blame-g, svn-mergeinfo-eligible (2
  connections), svn-mergeinfo-merged (3 connections).
- Recorded on the Studio (run at 3925e795f), 85 files, moved here by a kiln code_run program (Studio cat of base64 ->
  env var -> machines.run on this MacBook e30189ea), out.tgz sha256 964ae112..., every file checked against SHA256SUMS.

## Landed on main
- 48e285d21 (pushed 4b5530a7d..48e285d21, 2026-11-19): slice 2 of part 2, get-file + Stream (reads/file.rs,
  reads/stream.rs; Repository::text seam; Outcome::Streaming; Failure::value; owned_ancestors/inherited_value shared
  with get-dir). Studio: cargo test -p slates-svn --lib 287/287 on the branch, 319/319 on the tree merged with main,
  fmt clean, fixture sums 85 OK; red-checked 6 mutants (STRING_BYTES 4000; dir not refused; empty MD5; no empty string;
  inherited inverted; get-file handed elsewhere), each failed named tests. Docs: §4.17.4 A-605/A-613 status + piece 9
  (show-revs = client's work), A-613 entry bullets, GAPS, notes §13 (sources + hashes).
- 7f6e1e919 (pushed c314b4d8b..7f6e1e919): slice 3, list (reads/list.rs: svn_repos_list walk as a Stream, slices
  bounded by bytes written + entries visited; DirentFields; authz's fnmatch re-exported pub(crate)). Studio: 323/323 on
  the branch, 330/330 merged with main, fmt clean; 8 mutants (L1-L7 failed named tests; L8 equivalent output, caught
  after adding an_empty_pattern_list_answers_without_a_walk). GAPS: non-ASCII fold owed (utf8proc tables).
- 87e21b01a (pushed d449a10c7..87e21b01a): slice 4, get-location-segments (reads/segments.rs: serve.c range checks
  after done; segment_turn = one loop turn, shared with log later; Stream a turn a step). Studio 331/331 branch and
  merged, fmt clean; 5 mutants all failed the session replay.
- e931886a5 (pushed 7d0550ae6..e931886a5): slice 5, get-deleted-rev (reads/deleted.rs; closest_copy factored out of
  previous_location in reads.rs). Studio 332/332 branch + merged; 5 mutants all failed the replay.
- 3fc080a36 (pushed 659b6bf14..3fc080a36): slice 6 part (a), log WITHOUT merged revisions (reads/log.rs frames:
  Every/Walk/Ascending; reads/history.rs = FSFS history_prev on the index; merged -> SVN_ERR_UNSUPPORTED_FEATURE 200007
  after done until part (b)); reads now calls A-616's ONE copy-root walk (crate::report::drive::history: closest_copy
  via wrapper, deleted_revision, youngest_copy_root opened pub(crate) by me); hostile proptest fixed (seed cc 8c15aaa6 in
  reads/tests.proptest-regressions, suite seeded; handler was right, test bound wrong; pinned by
  a_list_with_a_word_for_its_depth_is_three_items_as_svnserve_answers_it). Studio 409/409 merged; 10 mutants (8 red,
  2 equivalent: standard-trio shortcut removed, root special case kept). Test fixture: Add/Replace with empty props ->
  properties None (FSFS prop_mod false).
- 7d3e1db3b (pushed 9ef49984b..7d3e1db3b): slice 6 part (b), log's MERGED revisions (reads/log/merged.rs;
  mergeinfo.rs gained mergeinfo_as_held over a shared nearest_mergeinfo walk). Studio 414/414 branch, 423/423 merged;
  13 mutants all red (4 new tests reach what the fixture doesn't: sent once ascending, two runs walked from the last,
  segments as mergeinfo, combine's intended removal).
- Reported to main: GAPS.md on main had committed conflict markers (§4.17.2 A-638/A-624 part 2); main fixed it
  (0727f8303).

## Next, in order (each its own landing) -- STATE 2026-11-19 ~09:40Z: Studio d1323898 OFFLINE (OOM), checked
## every ~5-30 min with `kiln run true on d1323898 sync false`; main's rules: CARGO_BUILD_JOBS=4, one cargo at a time,
## delete each target dir after reading; a "did not answer" run waits a minute, reruns once, counts neither way.
5. get-file-revs: branch agents/g297-svn-reads at 30ff3a4d8 (pushed), NEVER COMPILED. Commits: 87d6dc91a (the slice),
   8fef4b6bc (same_text = svn_fs_fs__file_text_rep_equal: missing text == zero-length text, then MD5+SHA-1; svndiff 0
   strings as send_simple_insertion_window writes them -- header+integers+instruction one string; tests
   a_text_made_empty_from_none_is_the_same_text, svndiff_0_strings_are_the_writes_svndiff_c_makes (bytes derived from
   svndiff.c); replay compares svndiff 0 deltas raw, svndiff 2 by string count + decoded windows; recorder gained
   session get-file-revs-svndiff0 (stock greeting minus svndiff1/accepts-svndiff2); docs written, tests marked owed),
   30ff3a4d8 (doc rewrap). When the Studio is back:
   a. clone agents/g297-svn-reads, `CARGO_BUILD_JOBS=4 cargo fmt -p slates-svn -- --check; cargo test -p slates-svn
      --lib`, delete target; fix compile errors; bring rustfmt's diff back by code_run + git apply.
   b. record from this branch (python3 -I crates/svn/fixtures/reads/make-fixtures.py OUT WORK, WORK under ~/.cache),
      tar|base64 back, check SHA256SUMS; add `(transcript!("get-file-revs-svndiff0"), 5)` to the scripted replay and
      "get-file-revs-svndiff0" to every_fixture_has_its_recorded_sum; run again.
   c. red-check (mutants: same_text old key; simple_insertion false; no delta when unchanged ignored; merged path rule;
      interleave order; svndiff version by capability; prop diffs), docs numbers (status, entry bullet, GAPS, notes),
      fetch+merge origin/main (markers, diff stat), rerun if slates-svn changed, push HEAD:main.
6. read access (A-606): branch agents/g297-svn-authz at e0880548f (pushed; includes 5), NEVER COMPILED. Session.authz
   (Option<&PrincipalRules>), must_read (170001 "Authorization failed", nothing before), readable(), revision_access
   (svn_repos_check_revision_access), RevisionAccess::{Full,Partial,Nothing}; get-dir/list entries skipped, iprops
   ancestors skipped, get-mergeinfo requested paths filtered, rev-proplist/rev-prop censored, locations/segments
   (220001 "Unreadable path encountered; access denied" at the peg, stop at hidden source), log (220001 "Item is not
   readable" at hidden path/root shortcut; history done at hidden location; detect_changed censoring + copyfrom drop;
   -g refused at hidden path via history_as_mergeinfo), get-file-revs (stop at hidden; empty mainline -> svnserve
   ABORTS (no malfunction handler; error.c default abort) -> Progress::Close). Recorder: second repo `locked` (hotcopy,
   authz-db = authz, rules: / hidden, /trunk r, /trunk/dir hidden, /trunk/dir/b.txt r, /tags r), written as fixture
   `authz`; scripted locked-reads (29), locked-log (8), locked-file-revs (4), locked-file-revs-abort (0); stock
   locked-svn-{ls,log,log-g,blame-g,cat,cat-denied,proplist} (their replay entries + counts to add after recording).
   Test a_copy_from_a_hidden_source_shows_no_source_and_the_author_and_date_alone (rev 12 committed onto the fixture).
   The branch does not compile until the recording adds fixtures/reads/authz and the locked transcripts.
   When 5 has landed: merge origin/main into agents/g297-svn-authz, record from it, add the stock locked sessions with
   their counts and every new name to the sums test, run, red-check (must_read always true; entries not filtered;
   copy source ignored in revision_access; no censoring; history hidden check removed; segment stop removed; abort as
   done; iprops not filtered; mergeinfo paths not filtered), docs numbers, land.
7. Owed after 6: the mid-command authentication request (a denied anonymous read in a pwdb/SASL/tunnel session:
   auth_request with needs_username, then the command without the trivial auth request) -- needs the connection loop
   and the handshake's exchange; `list`'s non-ASCII fold (utf8proc tables).

## Relaunch 2026-11-19 ~16:20Z (fresh g297, after the usage-limit stop)
- Branch agents/g297-svn-reads at 9f1efcc40 (pushed; the predecessor's merge of main 4644dcec3, still main's head).
  Worktrees clean. Readback of THE RULE sent to main.
- Reviewed file_revs.rs against every API it calls (reads.rs, history.rs, index.rs, mergeinfo.rs, svndiff, handshake,
  report::editor) and against 1.15.0's rev_hunt.c/serve.c/svndiff.c: no type error found by reading.
- jupiter run r47: first compile (fmt check, rustfmt diff captured, cargo test -p slates-svn --lib). jupiter's disk is
  99% full (14 GB free): delete clone AND target after every run.

## Run requests filed
- 2026-11-19 run-requests.md: slices 2-6 (clippy, xtask, doc, cross-target) + the editor's svndiff 0 sibling.

## Relaunch 2 (fresh g297, 2026-11-19 ~17:00Z): step 5 compiled and green on jupiter
- r79 (jupiter, branch 9f1efcc40): first compile OK; rustfmt rewrapped 10 code lines in file_revs.rs (applied here,
  sha256 matched jupiter's 320dd4b7...). slates-svn lib 442 passed, 2 failed: both transcript replays, from a HARNESS
  bug: `same_answer` returned the normalized length (get-file-revs folds a delta's strings into one entry), so `at`
  lagged 12 items per answer. Fixed (advance by items written).
- 6d3b573cb (pushed to agents/g297-svn-reads): fmt + harness fix. r125: slates-svn lib 444/444, fmt clean, Compiling
  slates-svn seen. Every scripted get-file-revs command (13) and stock svn-blame / svn-blame-g match svnserve.
- New tests (uncommitted until r152 is green): a_property_taken_away_is_sent_as_a_change_without_a_value (no fixture
  revision deletes a file property) and a_mainline_revision_goes_before_a_merged_one_of_the_same_number (tie rule
  `main_pr->revnum <= merged_pr->revnum`, never tied in the fixture).
- r152: green with the new tests, then the red (87d6dc91a's same_text + delta under 8fef4b6bc's tests) and 18 mutants
  (patches from ~/slates-work/g297-scripts/make_mutants.py, bundle in g297-scripts/bundle.b64), reads:: filter,
  hostile proptest skipped.
- Still owed (needs the Studio's stock svnserve): recording session get-file-revs-svndiff0 and adding it to the replay
  and the sums test; the svndiff 0 path is held today by svndiff_0_strings_are_the_writes_svndiff_c_makes alone.

## Relaunch 3 (fresh g297, 2026-11-19 ~19:20Z): step 5 lands from a squash onto main, run on the Mac Studio
- Found agents/g297-svn-reads at 04d2188d7 (pushed; f106d7ee3 docs of the jupiter run + 04d2188d7 merge of main
  bf9d39139 with serve.rs sites fixed), not on main. Main now cd84e3811 (A-616 continued (6): the editor's svndiff 0
  writer, a private `simple_insertion` = file_revs' own copy).
- Worktree ~/slates-work/g297-file-revs, branch agents/g297-file-revs from origin/main cd84e3811:
  `git merge --squash agents/g297-svn-reads` (clean, no markers), file_revs now calls the editor's
  `simple_insertion` (made pub(crate); file_revs' copy and its Instruction/Window imports removed), and the two tests
  from ~/slates-work/g297-scripts/tests.patch applied (they were never committed). Commit 4162d589d (tree e3c59ae8),
  pushed to origin agents/g297-file-revs for the Studio run.
- Main's correction (Ada): focused unit AND focused integration tests. slates-svn has no tests/ dir and no crate
  depends on it, so no integration target exercises get-file-revs; the ra_svn replays are lib tests (reads::, serve::).
- Landing plan: green run -> fresh worktree from origin/main, squash agents/g297-file-revs, docs numbers, one commit.
- LANDED 2026-11-19 ~19:35Z: d0ceb580e (pushed 5c0a522f1..d0ceb580e), step 5 = A-613 part 2's seventh slice,
  get-file-revs, ONE commit over main (squash of agents/g297-svn-reads 87d6dc91a..04d2188d7 + the two tests from
  g297-scripts/tests.patch + file_revs using report::editor::simple_insertion, its own copy dropped). Evidence: macOS
  arm64, Mac Studio, kiln r18 over cd84e3811 (4162d589d, crates/svn byte-identical to d0ceb580e): fmt clean;
  `cargo test --locked -p slates-svn --lib -- reads:: serve:: report::editor::` 58 passed, 0 failed, 413 filtered out
  (the two new tests, both get-file-revs replays, the sums test, the hostile proptest, serve 19, editor 8 among them).
  No integration target exists for slates-svn. Docs: GAPS §4.17.4 entry, A-613 seventh-slice entry (run, the two tests,
  the shared rule), research notes (the editor's version-0 header line updated for A-616 continued (6)).
- Cleaned: Studio ~/.cache/g297-{target,test,rec} + logs deleted (~3.1 GB); worktrees g297-file-revs and
  g297-file-revs-land removed (their commits on origin: agents/g297-file-revs 4162d589d, main d0ceb580e).
- Run request filed (run-requests.md, "g297 main d0ceb580e"): whole slates-svn lib (expect 471) + gates; the
  get-file-revs-svndiff0 recording; red-checks.
- Still owed for step 5: the get-file-revs-svndiff0 recording + its replay entry and sums name; red-checks.
- Step 6 (A-606 read authz, agents/g297-svn-authz at e0880548f, never compiled; worktree ~/slates-work/g297-svn-authz
  kept): needs main's go for the recording. Machine and tools: the Mac Studio d1323898 has them all (Homebrew
  Subversion 1.15.0: svn, svnserve, svnadmin, svnlook in /opt/homebrew/bin; /usr/bin/python3 3.9.6; port 36913 free);
  nothing to install. Order: merge origin/main (now holding step 5 as d0ceb580e, not the branch's commits: expect
  conflicts where e0880548f merged agents/g297-svn-reads; take main's file_revs.rs and re-apply authz's edits), record
  from it (OUT/WORK under ~/.cache), add the stock locked sessions + counts and every new name to the sums test, run
  `cargo test --locked -p slates-svn --lib -- reads:: serve::`, docs numbers, land.
- Sibling finding for main (not in the piece): the svndiff 0 write shape (header joined to integers+instruction for a
  simple insertion, else alone; empty sections skipped) is written twice, report/editor.rs EditorWriter::window
  (textdelta-chunks) and reads/file_revs.rs delta (strings); only the predicate is shared now.

## Step 6 (main's go, 2026-11-19 ~19:40Z): A-613 part 2's eighth slice, read access by A-606's rules
- Worktree ~/slates-work/g297-authz, branch agents/g297-authz from origin/main b1926d201 (NOT the old
  agents/g297-svn-authz, whose merge of the unsquashed step-5 commits must never reach main): `git cherry-pick -n
  f0ab655cc` (conflicts: file_revs.rs signature -> authz's; tests.rs both sides kept; GAPS/design both slices).
- serve.rs wiring (new in this landing): Established.read_rules (ReadRules {principal, rules}) compiled from
  policy.authz once per principal by Established::reads_session (recompiled when the principal changes), used by
  both read sites; reads::Progress::Close -> Refused::Aborted (new variant). serve tests: the hidden-listing
  allowance (HIDDEN, HIDDEN_LISTINGS=1, without_hidden) removed, every item equal; new test
  the_reads_judge_each_principals_paths_by_its_own_rules (keyless anonymous refused/check dir/log in SMALLEST slices;
  alice's tunnel reads secret).
- Recording: kiln r88 on the Studio at a49364b7c: recorder 0 in 3 s, no svnserve left, 110 sums OK; transferred by
  code_run to ~/slates-work/g297-fixtures-in/authz-r88 (27 files, 26 sums OK). Into the tree: authz + 22 locked-*
  transcripts; sessions.txt as recorded less get-file-revs-svndiff0; SHA256SUMS 108 lines all OK. The svndiff0
  transcripts are kept in authz-r88 for main's next piece. Stock counts: ls 2, log 4, log-g 4, blame-g 5, cat 3,
  cat-denied 2, proplist 4 (test stock_locked_sessions_are_answered_as_svnserve_answered_them).
- Scratch commits on agents/g297-authz: a49364b7c, 3ac54f162 (first build: kiln r97). Landing = fresh squash.
- LANDED 2026-11-19 ~20:00Z: effb30ac2 (pushed af520142f..effb30ac2), step 6 = A-613 part 2's eighth slice, ONE
  commit over main (squash of agents/g297-authz: f0ab655cc cherry-picked + serve wiring + fixtures + abort fix + docs).
  Evidence: recording kiln r88 (Studio, Homebrew svn 1.15.0, 110 sums OK, no svnserve left); first build r97 52/54
  (abort replay: svnserve writes NOTHING for an aborted get-file-revs, its auth request dies in the unflushed buffer --
  the slice had written it; serve test's 7-item log malformed); fixed; r110 on f284443fc (crates/svn byte-identical
  to effb30ac2): fmt clean, `cargo test --locked -p slates-svn --lib -- reads:: serve::` 54/54, fixtures 108/108 OK.
  serve's replay now every item equal (hidden-listing allowance removed). Docs: §4.17.4 + A-656 status, A-613 eighth
  slice entry, GAPS, research notes.
- Cleaned: Studio g297 dirs (~1 GB); worktrees g297-authz-land, g297-authz, g297-svn-authz removed (branches kept:
  agents/g297-authz pushed at f284443fc, agents/g297-svn-authz at e0880548f -- NEVER merge it: unsquashed history).
- Run request filed ("g297 main effb30ac2"): whole lib expect 475 + gates; red-checks; findings (a) recompile after
  mid-session auth untested, (b) update's Guards::may_read vs reads' Policy::authz rules = two sources.
- Owed next (main starts): get-file-revs-svndiff0 replay entry (transcripts + sums in
  ~/slates-work/g297-fixtures-in/authz-r88); step 7: the mid-command authentication request for reads; list's
  non-ASCII fold.

## Step 7 (main's go ~20:05Z): mid-command auth for reads, list's non-ASCII fold, finding (a)'s test
- Worktree ~/slates-work/g297-step7 (agents/g297-step7, scratch commits 9ba6f305b, b438b6159, 2f6009de8, pushed).
- Red first, kiln r149 (9ba6f305b): the three new serve tests answered "Authorization failed" where svnserve asks.
- Recording r149 (Studio): report recorder `read-auth-cat`, `read-auth-ls` (alice), `read-auth-cat-bob` (bob added
  to PASSWD; `stock()` honours named sessions that need no WC); svnserve stopped by the recorder, none left. Fold
  oracle crates/svn/fixtures/fold/make-fold.py: Homebrew utf8proc 2.12.0 via ctypes (Unicode 18.0.0), 5,760 values +
  28 names. CaseFolding.txt 17.0.0 (sha256 ff8d8fef..., 1,585 C+F) in crates/svn/data/unicode. Patch moved by
  code_run (189,326 bytes, sha256 77d14967...).
- Built: reads Authentication {Settled, Available, Done}, Outcome::Authenticate; serve mechanisms_for (one rule for
  ask and reads), ask_for_read, read_again, Interrupted::Read; reads::fold (unicode-normalization 0.1.25 = Unicode 17,
  added to slates-svn + Cargo.lock by hand); list.rs uses it.
- Green: r179 (b438b6159, 64/64 with rustfmt applied), r189 (2f6009de8 fmt clean, 64/64), oracle 1,112,006 compared,
  58 skipped (Unicode 18 additions).
- Merges: main's bdc73e368 (shared svndiff writer) -> squash 83a92a80a, r203 73/73; then main's f0f110cf3 (A-656
  continued (2), finding (b): ReadRules::for_principal, compiled_read_rules, FixtureGuards locks only) -> merge
  7a82f1e31 (r224: CHECK_SECRET defined twice -> removed mine) -> a1140e595, r228 106/106 (reads:: serve:: report::),
  fmt clean. Landing = squash of a1140e595 onto latest main (worktree ~/slates-work/g297-step7-landing).
- Found, for main: crates/svn/fixtures/reads/SHA256SUMS holds sessions.txt's pre-bdc73e368 sum (shasum -c fails 1).
- LANDED 2026-11-19 ~20:50Z: a5ceb9c02 + merge 1561487f0 (pushed 1fdebac50..1561487f0), step 7 = A-613 part 2's
  mid-command authentication for reads, list's non-ASCII fold, finding (a)'s test. crates/svn byte-identical to the
  tested a1140e595 (kiln r228: reads:: serve:: report:: 106/106, fmt clean). Studio cleaned (~1.5 GB); step-7
  worktrees removed (branches on origin: agents/g297-step7, -merge, -merge2, -merge3).
- Run request filed ("g297 main 1561487f0"): whole lib + gates; red-checks; the reads SHA256SUMS finding.
- A-613's UI goal (93) per main: steps 5, 6, 7 landed. Left: the 58 Unicode 18 code points (crate tables are 17),
  red-checks of slices 7, 8 and step 7 (main's).

## Fixture sums piece (main's go ~20:52Z; kiln and Claude Code restarted once on the way)
- Worktree ~/slates-work/g297-sums (agents/g297-sums, pushed: 360fac1cb test only, 3e4f165e6 with the fix).
- Test crates/svn/src/fixture_sums.rs (#[cfg(test)] mod in lib.rs; std::fs reads are test-exempt): all 7 fixture
  dirs (commit, dump, fold, handshake, hooks, reads, report), recorder inputs named per dir.
- Red r13 (360fac1cb): one problem, reads/sessions.txt. Fix: its sum da3c353a... (was 66daf529...).
- Green r23 (3e4f165e6): fixture_sums reads:: serve:: 67/67, fmt clean, shasum -c clean in all 7.
- Main moved: 8727ff426 (one record of locks, crates/svn src, no fixtures) -> landing squash onto cb9d7f5a5 in
  ~/slates-work/g297-sums-land, to be run again before landing.
- LANDED 2026-11-19 ~21:05Z: 560767b0a + merge d015143ac (pushed 4dca63df7..d015143ac). r38 over cb9d7f5a5: 68/68,
  fmt clean, shasum -c clean in all 7. Studio cleaned (~1 GB). Run request filed ("g297 main d015143ac").

## A-625 (DAV site on the shared HTTP edge) -- NOT STARTED: context short, handed back to main (2026-11-19 ~21:10Z)
- Main's brief: scope A-625 into parts; part 1 = read side (OPTIONS, PROPFIND, GET, REPORTs over the index) through
  the edge's authentication and the compiled read rules (A-656 continued (2)'s ReadRules / authz::may_read); take an
  A-number; record stock sessions; failing tests first; land part 1; tell main the remaining parts.
- Checked for the next agent (kiln, Studio d1323898): Apache httpd 2.4.66 at /usr/sbin/httpd with
  /usr/libexec/apache2/mod_dav.so, but NO mod_dav_svn.so (Homebrew subversion 1.15.0 ships only
  include/subversion-1/mod_dav_svn.h; brew httpd not installed). So no stock Subversion DAV server on the Studio as
  is: either A-609's and A-627's golden exchanges (main's stated fallback), or a Linux image with apache2 +
  libapache2-mod-svn (Docker is authorized for Linux validation), or building mod_dav_svn (a tool install: ask).
- No worktree, branch or kiln run exists for A-625. Base worktree ~/slates-work/g297-svn-reads kept.

## A-681 (relaunch 4, 2026-11-19 ~21:30Z): Subversion's DAV site on the shared HTTP edge, part 1 = read side
- A-number A-681 taken from amendments.tsv (A-625 is the edge itself). Worktree ~/slates-work/g297-dav-site, branch
  agents/g297-dav-site from origin/main 3612d1ef9. Oracle per main: A-609/A-627 golden exchanges + a stock `svn`
  (Studio Homebrew 1.15.0) loopback test that skips loudly when svn is absent; never build/install mod_dav_svn.

## Relaunch 5 (fresh g297, 2026-11-19 ~21:25Z): A-681 part 1
- Found ~/slates-work/g297-dav-site orphaned: its .git pointed at g262-slates-has-library/.git/worktrees/g297-dav-site,
  which did not exist (the predecessor died in or right after `git worktree add`); no file edited after the checkout
  (find -newer: only checkout-order tail files), local branch agents/g297-dav-site = origin/main 3612d1ef9, no commits,
  never pushed. Removed the orphan (121 MB) and re-created the worktree on the branch: clean at 3612d1ef9.
- Capture (kiln, Studio, svn 1.15.0 vs my responder ~/slates-work/g297-scripts/dav-rec/, 2026-11-19 ~21:45Z): session
  open = OPTIONS (activity body) + chunked probe OPTIONS + OPTIONS again; info = PROPFIND Depth 0 of 6 dirent props on
  !svn/rvr/REV/path + PROPFIND lockdiscovery on the public URL; cat/export = PROPFIND allprop + resourcetype on rvr, GET
  rvr; ls uses list-report when `list` is advertised, else PROPFIND Depth 0 + Depth 1; ls -v adds get-locks-report;
  log = log-report; info -r N = get-locations; Basic sent after one 401, then on every request.
- Scoping decided: part 1 = the site + OPTIONS/PROPFIND/GET (advertise only served capabilities); part 1 continued =
  the REPORTs (log, locations, segments, deleted-rev, dated-rev, mergeinfo, iprops, locks, list, file-revs); part 3 =
  update-report/replay; daemon composition (site router by tool + server svn source + principal->svn user) owed.
- MAIN (msg ~21:50Z): after part 1 lands, go STRAIGHT ON to part 2 = the write side (MKACTIVITY/HTTPv2 txn, CHECKOUT,
  PUT, PROPPATCH, MERGE, DELETE, LOCK/UNLOCK via A-627 + A-655's commit path); g307 writes stock-svn interop tests.
- Edits so far in the worktree (uncommitted): http-edge connection.rs reply_status + test
  a_service_names_its_reply_status_as_its_reply_begins; http reply.rs Status::MULTI_STATUS; vcs_http.rs
  Exchange::reply_status.

## Relaunch 6 (fresh g297, 2026-11-19 ~22:13Z): A-681 part 1, first slice LANDED
- Found agents/g297-dav-site at 9b1177d93 (pushed scratch: site.rs, vcs_http/svn.rs, edge reply_status; Studio logs
  of the predecessor: edge 2/2, dav:: 73/73, vcs_http:: 44/44, fmt diff not applied, 2 dead-code warnings NoSvnStores/
  NoGreen). Fixed: weekday from DAY_NAMES.len(), test until_the_stores_land_every_request_a_principal_may_make_is_503
  (hg's NoStores pattern; clears both warnings), doc rewrap; fmt by the Studio (patch sha256 1784bfec..., applied).
- LANDED 2026-11-19 ~23:00Z: 022866602 + merges c95335c6e, 8e340d6d5, 4fd033280, 693bc8feb (pushed
  e8fbe9116..693bc8feb). Evidence: kiln r252 (over b94ae6e90), r282 (over 9f349d66b), r301 (c95335c6e): fmt clean,
  edge 2/2, dav:: 73/73, vcs_http:: 47/47, stock svn info/cat/ls/proplist read over HTTPS loopback, hidden cat E175013.
  Docs: §4.17.4 status + piece 13, A-681 entry (log end), GAPS §4.17.4, research §3 (responder e7e534ec22cf47c7).
- Run request filed ("g297 main 693bc8feb"). Worktrees: g297-dav-site (scratch branch, pushed 9e429bd1b), g297-dav-land
  (landing branch agents/g297-dav-land). Studio: ~/.cache/g297-{dav,target,logs} kept for the next slice.
- Main (msg ~22:50Z): route the DAV site into DaemonSite (crates/server/src/vcs_http/daemon_edge.rs: git's site alone
  today, so a stock client of another tool meets 404) in part 1 if part 1 serves reads end to end, else in part 2.
- NEXT (part 1, slice 2+): the REPORTs so `svn log` and `svn checkout` reach a repository:
  a. log-report: a receiver seam in reads::log (LogStream writes ra_svn items straight into reads.out in
     changed_path/entry/end_marker/finish): a trait the ra_svn writer and a DAV writer (A-609's response::log pieces
     made incremental) implement; LogRequest from request::LogReport (paths relative to the report's target); the site's
     Answer gains a streamed report variant stepped a reply run at a time (store lent each run, like Text).
     mod_dav_svn sources are NOT local (g297-sources holds libsvn_repos_log.c etc.): fetch reports/log.c, update.c,
     util.c, version.c from the apache/subversion GitHub mirror at 1.15.0 into a new empty dir, hash them.
  b. update-report (checkout): request::UpdateReport -> A-616's report::drive over the index -> A-627's dav::Drive;
     SVN-Allow-Bulk-Updates and the capabilities listed as served; then DaemonSite routing (svn site composed beside
     git's, real SvnStores over vcs_store's Subversion store (A-655 part 2b on main)).
  g307's crates/cli/tests/svn_interop.rs has an ignored HTTP round-trip leg naming A-681, A-686, A-685.

## A-681 part 1, second slice LANDED 2026-11-19 ~23:50Z: the log-report
- 4a7437f0c (squash of agents/g297-dav-land's scratch 4bf3fb4ac..9303e18a4 onto main 5206419be) + merges, pushed
  075e36b52..9ca6e3883. reads/log.rs: Writer {Items, Dav} (receiver seam); reads/log/dav.rs: DavWriter, DavLog,
  DavLogProgress (re-exported from reads); dav/site.rs: REPORT -> Answer::Report(Report), ReportProgress
  {More, Done, Refused(HttpAnswer), Broken}, log-revprops served; dav/response.rs: LOG_REPORT_START, LOG_ITEM_START,
  LOG_REPORT_END, log_item_end, changed_path pub(crate); dav/locking.rs: convert_status(code, status) full table;
  server vcs_http/svn.rs: Answered::Report, served_of, first_slices (yield_now between slices); tests: reads
  a_log_report_answers_the_entries_svnserve_answers (9 cases vs ra_svn + shapes), a_negative_log_limit_is_no_walk;
  site a_log_report_streams_its_walk_and_is_refused_before_its_first_byte, a_report_is_refused_by_its_resource_...;
  server a_log_report_is_streamed_by_the_principals_rules; stock test + svn log / log -v.
- Evidence: r393 (server 48/48 + stock svn log), run over 211f141c0 (svn reads:: dav:: 118/118, fmt clean), r435 on
  the landing tree 4a7437f0c (fmt clean, svn 118/118, server vcs_http:: 51/51, stock). Sources fetched:
  ~/slates-work/g297-sources/dav-1.15.0 (mod_dav_svn reports/log.c, update.c, version.c, repos.c, util.c,
  get-locations.c, dated-rev.c; ra_serf log.c, update.c, options.c, util.c, serf.c) + httpd-2.4.66 mod_dav.c, sums
  beside them.
- Worktrees: g297-dav-site (slice-1 scratch), g297-dav-land (slice-1 landing + slice-2 scratch, branch
  agents/g297-dav-land at 9303e18a4), g297-dav-land2 (slice-2 landing). Studio: ~/.cache/g297-{dav,target,logs}.
- NEXT (part 1, slice 3): the daemon routing + real stores, then checkout:
  a. main's ToolSites (vcs_http.rs, A-680 part 2a) routes hg wire requests to hg, else git: add svn by request shape
     (method git never sends -- anything but GET/HEAD/POST -- or a `!svn` path segment), DaemonSite = ToolSites<git,
     hg, svn>.
  b. production SvnStores over vcs_store's SvnStore.records (slates_svn::migration::Repository): implement
     reads::Repository + mergeinfo::Contents for migration::Repository (properties via migration::properties::entries,
     text via content(identity).get(offset..), revision_properties from &Properties); Contents::entries(directory, rev)
     needs an index listing (none exists: Index has node(path, rev) and history(path) only) -- A-553's domain; the
     svn:// listener (vcs_svn.rs, A-689) needs the same lender ("lends the repository's sources there with every call",
     owed). Read rules: where a repository's authz file and a principal's svn user come from on the daemon is open.
  c. the update-report (checkout): request::UpdateReport -> A-616 report::drive over the index -> A-627 dav::Drive;
     SVN-Allow-Bulk-Updates; `me` resource for the report target.
  d. the exchange's Text reply takes whatever Repository::text returns from the offset; over records holding whole
     texts that is the whole rest of a file in one run: cap each run at the room (found by reading; tell main).
