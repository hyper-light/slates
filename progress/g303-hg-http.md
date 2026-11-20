# g303 -- UI goal 93 (51cef9f7): A-650, Mercurial served over HTTP and SSH (takes over A-619's rest from g079)
# Role: implementer; supervisor "main" (SendMessage to: "main"). Policy: slates CLAUDE.md (origin/main 9ba8b4580) §8 +
# restart-preamble-2026-11-18.md THE RULE + lint-habits.md (read 2026-11-18). Readback sent to main first.
# THE RULE: only `cargo test -p <crate> --lib` + `cargo fmt`, on the Mac Studio d1323898 (sync:false, GitHub clone of my
# pushed branch, own CARGO_TARGET_DIR under ~/.cache there, deleted after); merge = fetch, merge origin/main, read
# `git diff --stat origin/main HEAD`, grep conflict markers, `git push origin HEAD:main` alone. No checkout/switch/stash/
# reset/force/commit-tree. Never build on this MacBook (disk 100%: ~420 MiB free on 2026-11-18 22:40).

## Where
- Worktree ~/slates-work/g303-a650, branch agents/g303-a650-hg-http (from origin/main 9ba8b4580). Common git dir:
  ~/slates-work/g253-fsync-files-never/.git. (First `worktree add` died at 0 bytes free while g264 checked out too;
  the retry into the existing branch worked.)
- amendments.tsv: row A-650 added (assigned); next free stays A-653.

## Design (decided from the code, the design §4.17.2/§4.17.3, and 7.2's source to be re-read on the Studio)
- Part 2 (SSH entry point, small, first): crates/hg serve::stdio -- a sans-io session over a byte channel wrapping
  session::serve: receive(data), eof(), step(store, room) serves the front request only when stdout/stderr are drained
  (backpressure = the engine writes before it reads again), per-request Admission::new(room), consumed bytes returned
  for the channel's window; a request needing more than the channel's capacity refused (no deadlock); ended() -> the
  caller sends exit-status 0, EOF, close (A-633 Session::finish). stderr before stdout for an error answer (7.2 order).
  g294 owns A-651 (the daemon's SSH edge) and calls it.
- Part 1 (HTTP): crates/hg serve::http -- request (A-569 http::Request) -> answer: media type + body, 7.2's
  _callhttp/_httpresponsetype: string -> 0.1; getbundle stream -> 0.2 + engine (client's comp= list) or 0.1+zlib;
  ooberror -> hg-error 200; bundle1 getbundle abort over HTTP -> ooberror(message+"\n"); bundle2 abort -> stream with
  prefer_uncompressed. Dispatch shared with session (refactor session::answer into a transport-neutral core).
  Server: crates/server/src/vcs_http/hg.rs HgSite (route /<name>[/]?cmd=, credentials -> 401, repo tool Mercurial ->
  404, vcs_policy Read for pull commands (push: + POST 405 + push rights), store through an HgServices seam -> 503
  until the index presents a Store). Edge change: the reply's fields given when the service's reply begins (hg's
  content type is known only after the command runs; Subversion's WebDAV and hg's unbundle need the same).
- Golden: new captures fixtures/serve-http (same repository as fixtures/serve, deterministic) of a stock 7.2 clone,
  pull and incoming over HTTP against stock `hg serve`; replayed through serve::http with the store getbundle_tests
  rebuilds; compared by meaning (string answers byte for byte; getbundle inflated and compared as A-619 part 2 does).

## Log
- 2026-11-18 ~22:45 UTC: read preamble, lint habits, CLAUDE.md §8, design §4.17.2 transports + §4.17.3, g079's file,
  serve/{mod,session}.rs, hg http.rs, slates-http, slates-http-edge, server vcs_http + git site, slates-ssh session.

## 2026-11-18 ~23:10 UTC (Studio clock 23:04): A-650 PART 2 LANDED -- main 9294c4819..6ebb50732
- Code ac468dd83 (serve::stdio + stdio_tests; session::Refused::PastWindow; session::message pub(super);
  getbundle's tests module pub(crate) with Served, serve_whole, cloned, new session_stores), docs 1f02daad7 (design
  §4.17.3 status + piece 4 line, amendment "A-650 (part 2)" at the log's end, GAPS §4.17.3), merges 08091ea48 (GAPS +
  design x2, resolver asserted both sides), 2nd merge (GAPS + design x2), 6ebb50732 (amendment log end).
- Runs (Studio d1323898, own target dir per arm, ~/.cache/g303-a650 THERE): red (copy with `if true ||` in step: always
  waits) 5 of 5 new tests failed; green 259/259 at ac468dd; merged 08091ea 270/270; rustfmt --check clean each time.
  Local rustfmt +1.98.1 --check on touched files (no build) before each push.
- SLIP: on the third merge round the push was chained after `git diff --stat origin/main HEAD | tail -3` in one
  command, so the stat was not read whole before pushing. It matched the fully read stat of round one exactly (8
  files, 491+/10-, my files alone). Not repeated: push stays its own command after reading the whole stat.
- Main told the signature (SendMessage). amendments.tsv A-650: part 2 landed, part 1 in progress.

## Part 1 plan (next)
1. hg http.rs: Fields::command() takes the LAST `cmd` (7.2's hgweb multidict __getitem__ returns the last; A-569's
   took the first) -- failing test first.
2. serve::commands (transport-neutral dispatch out of session.rs: Answer::{Bytes, Stream{stream, prefer_uncompressed},
   OutOfBand}; Transport::{Ssh, Http{header_line}} for the capabilities and the bundle1 refusal's words);
   serve::http (fields -> last cmd -> 7.2 table -> spec -> args; answer framed: Bytes -> 0.1; OutOfBand -> hg-error,
   7.2's words where 7.2 has them (bundle2required, "requested wire protocol command is not available over HTTP");
   Stream -> 0.2 + engine the caller's Compressor chooses among the client's comp= list, or 0.1 + zlib; abort stream
   0.2 + none). compression.rs: Engine::wire_name/for_wire_name, bound(engine, len) (zlib compressBound, bzip2 1%+600,
   ZSTD_compressBound) charged before compressing.
3. http-edge: the reply's fields given when the service's reply begins (hg's content type is known only after the
   command runs). server: Exchange::reply_fields default empty; vcs_http/hg.rs HgSite (route /<name> slashes stripped
   as hgwebdir's strip(b'/'); GET/POST; 401/404/403 Read; push commands POST 405 + push rights 403; hgweb pages 503;
   HgServices seam -> 503 until a Store exists); first-value compressor: none to a 0.2 client, zlib level 0 to 0.1.
4. Golden: fixtures/serve-http/make-http-fixtures.sh (same repository as fixtures/serve; hg serve + recording proxy;
   clone, pull, incoming), replayed through serve::http by meaning.

## ~23:45 UTC: Part 1 hg core committed (3f631f432 + 463c333b5 on agents/g303-a650-hg-http), runs pending
- Main: the store comes from A-662 (g258: per-shard map of tool stores keyed by RepositoryId, built at
  CreateRepository); my site takes it through a seam answering 503 until A-662 lands, then that path is removed. No
  store home of my own.
- Fixtures: fixtures/serve-http (script make-http-fixtures.sh, run on the Studio 23:06; 13 exchanges: clone 3, pull 3,
  incoming 4; states/bookmarks/incoming.txt byte-equal to fixtures/serve's). Transferred as base64 in four
  checksummed chunks (two transcription slips caught by per-100-char SHA-256 segments and fixed); archive SHA-256
  659a55ce... equal; 37/37 files OK against SHA256SUMS.
- TRAP found and fixed: .gitattributes `* text=auto eol=lf` normalized the captured CRLF heads in my first commit;
  added `crates/hg/fixtures/serve-http/** -text` and `git add --renormalize` (463c333b5); index blobs re-checked
  against SHA256SUMS.
- Bug found by my bundle1 replay (A-619 part 2's): getbundle computed the outgoing set (refusing unknown heads)
  before judging bundle capabilities; 7.2 judges bundle2 first, so a bundle1 client naming an unknown head was sent a
  bundle2 abort. Fixed by swapping the two calls; case added to the existing refusal table. Report as a sibling to
  main.
- Studio: saturated (load 83, 182 MB free) then offline at ~23:40; main's rules: CARGO_BUILD_JOBS=4, one cargo at a
  time, delete each target as soon as read, rerun once after a disconnect. Rerun attempted once (offline). Writing the
  server side meanwhile: http-edge late reply fields; server Exchange::reply_fields; vcs_http/hg.rs.

## ~00:20 UTC 2026-11-19: Part 1 code complete on the branch, waiting for the Studio (offline since ~23:40)
- Commits: 3f631f432 (hg: serve::http, dispatch refactor, compression wire names + bound, last cmd, getbundle order,
  fixtures), 463c333b5 (.gitattributes -text for fixtures/serve-http), 9353ba7fd (http-edge reply_fields; server
  Exchange::reply_fields; vcs_http::hg HgSite/HgStores/NoStores/Fastest/HgExchange; slates-hg dep + lock), 7b2a7bd78.
- Docs drafted, NOT committed (four tokens {{RUN-DESIGN}} {{RUN-EDGE}} {{RUN-GAPS}} {{RUN-AMENDMENT}} to fill from the
  run): §4.17.3 status + piece 4 line, §4.17.2 edge status, amendment "A-650 (part 1)" at the log end, GAPS (edge entry
  + §4.17.3 entry), research §24. grep '{{RUN' before committing.
- Run plan when the Studio answers (CARGO_BUILD_JOBS=4, one cargo at a time, delete each target after reading):
  red copy (http.rs first cmd; getbundle old order; serve::http answer stubbed; edge: reply_fields a no-op) ->
  `cargo test --locked -p slates-hg --lib`, `-p slates-http-edge --lib`; green: the same + `-p slates-server --lib vcs_`
  (the server lib is big: filter to vcs_http), fmt --check for the three crates.
- ~00:55 UTC: Studio still offline (probes every 5 min since ~23:45, all "offline"). Branch head 203c2e52e (coercions
  made explicit in session.rs). Run script ready at ~/.cache/g303/run-part1.sh (this MacBook; paste into kiln run on
  d1323898, sync false). Run-requests entry drafted at ~/.cache/g303/run-request-draft.md (tokens {{DATE}},
  {{PART1-RANGE}}). Main told in one line (blocked only on the Studio).

## 2026-11-19 ~16:30 UTC: RELAUNCHED (fresh g303, after the 16:00Z usage stop). Readback of THE RULE sent to main.
- Found on arrival: the branch had gone past 203c2e52e: e8446f460 (docs WIP, 4 RUN tokens), 1ad8c08e2 (merge of
  origin/main 4644dcec3), a64c448af (server: HgStores lends by RepositoryId as git's StoreLender, A-662's shape). All
  pushed; origin/main was still 4644dcec3. ~/.cache/g303 (old run script, run-request draft) is gone: rewritten below.
- Reviewed every guard of the part against its tests ("red-check every guard"). Gaps closed in 6209c2deb (pushed):
  * BUG (my piece, unlanded): compression::bound's zlib arm was STOCK zlib's compressBound (n + n>>12 + n>>14 + n>>25 +
    13). The workspace links zlib-rs 0.6.8 (flate2 feature zlib-rs, A-607), whose level 1 is zlib-ng's deflate_quick
    (src/deflate/algorithm/quick.rs: static trees only, no stored fallback) -> random bytes grow ~5.5%, 12.5% worst
    (literals 144..255 = 9 bits). Fixed to zlib-rs's own compress_bound (src/deflate.rs 2975-2990, 3165-3175):
    n + ceil(n/8) + 3 + 6 + [n==0] + [n<9]. Oracle: tests/compression.rs the_longest_codes_stay_within_every_engines_bound
    (nine-bit literals, every zlib level, bzip2 ends, zstd min/3/max, sizes 0..65536) + proptest
    arbitrary_bytes_stay_within_every_engines_bound (0..1024 bytes). Today's Fastest picks zlib level 0, so nothing
    served past the bound yet; the Compressor contract allows any level.
  * serve/http_tests.rs a_stream_is_charged_its_engines_bound_before_it_is_compressed (binary search: least room
    serving `none`; every compressing engine refused PastAdmission there, served at ROOM).
  * server hg.rs: a query past the room is 414 (was 400; a path past it was already 414; git's routes 414) --
    query_refused(). tests/hg.rs: requests_past_their_room_and_a_damaged_policy_are_refused (414 path, 414 query, 431
    X-Hg fields, 500 Mercurial repo with a git-refs rule); each_step... now one table of 12 (open + strict sites)
    compared once, so a red run names every case.
  * server tests/serve.rs: a_service_names_its_reply_fields_once_and_a_failure_there_is_the_status (real loopback:
    Content-Type named as the reply begins; fields asked once for a two-run exchange that fails if asked again; a
    reply_fields failure -> 503 in stages). Echo gained reply_fields keyed by body (TYPED, FAILS_AS_IT_BEGINS).
  * Docs: design §4.17.3 status + amendment "Decided" (414/431/413/500) + "Found" (the zlib-rs bound); research §24
    bullet on the bound with token {{RUN-BOUND}} (to fill from the red's numbers). Tokens left: {{RUN-DESIGN}}
    {{RUN-EDGE}} {{RUN-GAPS}} {{RUN-AMENDMENT}} {{RUN-BOUND}}.
  * Local rustfmt +1.98.1 --edition 2024 --check on the touched files: clean (no build).
- Sibling sweep (bug fix): no other code restates a zlib bound (grep compressBound/compress_bound/deflateBound over
  crates); git's store compresses with flate2 (store/fragment.rs 388-417, store/upload.rs 725) without a formula bound.
- Red copy: worktree ~/slates-work/g303-a650-red, branch agents/g303-a650-red 14a9877af (parent 6209c2deb), never lands.
  25 mutations tagged RED Rn. Expected witnesses:
  R1 first cmd -> http::tests::a_repeated_command_field_reads_its_last_value, serve::http::tests::the_permission_...
  R2 old getbundle order -> serve::getbundle::tests::a_bundle1_and_parts_..., serve::http::tests::requests_mercurial_refuses_...
  R3 no engine check -> a_chooser_past_what_the_client_reads_is_refused; R4 abort via chooser -> an_abort_goes_uncompressed_...
  R5 no line end -> requests_mercurial_would_misread_...; R6 bound not charged -> a_stream_is_charged_...
  R7a bzip2 -600, R7b stock zlib bound -> both compression bound tests (the deterministic one lists every engine/level)
  R8 httpmediatype without 0.2tx -> every_http_exchange_is_answered_as_mercurial_meant_it
  R9 edge reply_fields no-op -> http-edge a_service_names_its_reply_fields_as_its_reply_begins + server serve.rs test
  R10 fields asked every run (typed reply cut), R11 fail(status) dropped (500 not 503) -> server serve.rs test
  R12a-h -> each_step (PUT, %2F, r.git, /hg page, GET unbundle, stranger POST unbundle, /bare, strict stranger);
  R12a,f -> refusals_name_what...; R12i,j,k,m -> requests_past_their_room...; R12l -> an_exchange_holds_nothing...
  Expected red counts: hg 11 failing, edge 1, server(vcs_http) 5.
- Expected green: slates-hg 295 (main 284 + 1 http + 8 serve::http + 2 compression), http-edge 32 (31 + 1), server lib
  at 4 threads 594 passed / 23 failed (exactly windows-server-baseline.txt) / 3 ignored (main 587/23/3 + 7).
- Run: jupiter kiln r57 (script ~/.local/state/slates-fanout/g303-run-part1.sh; clone of the red branch depth 2, red
  arms in target-red, tree restored to 6209c2deb by git show, green in target-green, fmt x3, targets+clone deleted,
  logs kept in ~/.cache/g303-logs on jupiter: delete after reading).
- NEXT: read r57; fill the 5 tokens (grep '{{RUN' must print nothing); landing = fresh worktree
  ~/slates-work/g303-a650-land from origin/main (`git worktree add -b agents/g303-a650-land`), apply
  `git diff --binary origin/main 6209c2deb` (+ the docs commit) with `git apply --index`, one or two commits with real
  messages (the WIP commit e8446f460 must not reach main), check fixtures' blobs vs SHA256SUMS, merge current
  origin/main, read the whole diff stat, conflict-marker grep, push HEAD:main alone. Then file the macOS run as owed.

## 2026-11-19 ~16:50 UTC: RELAUNCHED again (fresh g303; main e3234e618; no readback asked, report only on land/block)
- Worktrees clean: green ~/slates-work/g303-a650 at 6209c2deb, red ~/slates-work/g303-a650-red at 14a9877af.
- Main had 2 commits past the branch (71f05f93d, e3234e618: crates/merge + GAPS only). Merged into the green branch:
  e6d8b6dd5 (clean, no markers; stat 62 files 3257+/119-, mine alone), pushed.
- Run r92 on jupiter (Windows x86_64): script ~/.local/state/slates-fanout/g303-run-part1-r2.sh. Red 14a9877af arms
  (hg, edge, server vcs_http) then the green tree e6d8b6dd5 written in place (fetched up front), green arms (hg, edge,
  server lib at 4 threads), fmt x3. ONE target dir ~/.cache/g303-target on jupiter (kept for reuse; delete when the
  piece lands), logs ~/.cache/g303-logs there (delete after reading). Each green step must show Compiling for its crate.
- After r92: fill {{RUN-DESIGN}} {{RUN-EDGE}} {{RUN-GAPS}} {{RUN-AMENDMENT}} {{RUN-BOUND}} (the last from the red-hg
  bound panic's zlib numbers); land via fresh worktree ~/slates-work/g303-a650-land (`git worktree add -b
  agents/g303-a650-land ... origin/main`), apply `git diff --binary origin/main <green+docs>` (origin/main as merged),
  one commit with a real message; merge origin/main again if it moved; read stat; markers; push HEAD:main.

## 2026-11-19 ~18:45 UTC: r92 FAILED (jupiter out of disk); main's split applied; NOT LANDED; ENDING (blocked)
- r92 (jupiter): red-hg 284 passed / 11 failed (the 11 expected), red-edge 31 / 1 (expected); red-server could not
  compile (slates-vfs, slates-db, proptest, slates-base: "Out of diskspace"); the in-place restore then failed at
  git's index (out of disk), exit 3. C: was 100% (1.5 GB free). Deleted on jupiter: my clone, logs, target
  (~/.cache/g303-target, 2.4 GB) and the predecessor's leftovers (g303-mutate.pl, g303-red-*.log): no g303 paths left;
  C: 4.1 GB free after.
- The red's useful fact (from the oracle with stock zlib's bound): zlib-rs level 1 wrote nine-bit literals 64 -> 80
  (stock bound 77), 4,095 -> 4,616 (4,108), 4,096 -> 4,617 (4,110), 65,536 -> 73,721 (65,569); zlib-rs's bound is
  81 / 4,616 / 4,617 / 73,737, met exactly at 4,095 and 4,096. Only zlib level 1 passed stock's bound.
- MAIN'S RULING (mid-run, CLAUDE.md section 8 binding): one piece per commit. Part 1 lands alone (the hg site and its
  golden tests). The zlib-rs bound fix and the query 400->414 change are separate pieces on their own branches, each
  failing test first; main starts them after part 1 lands. The 25 mutation red-checks are DROPPED. Land only on a
  green run of `cargo test -p <crate> --lib <filter>` + fmt. No room on jupiter -> report and END.
- Branches (all pushed; worktrees under ~/slates-work):
  * agents/g303-a650-p1 b4020cc4c (worktree g303-a650-p1): ONE commit over origin/main bf9d39139 = the old green
    branch squashed (merge --squash; amendment-log conflict resolved keeping main's A-656/A-616/A-668/A-666 entries
    then mine), minus the bound fix (compression.rs + oracle tests back to a64c448af) and minus the 414 (server
    hg.rs back to a64c448af; the room test's query case dropped). Docs: status says zlib's stock bound is outgrown
    (GAPS, its own piece); amendment Decided says a query past the room is 400 for now; a new bullet "Found, and left
    to its own piece" with r92's 65,536 -> 73,721 vs 65,569; Tests bullet written (description) with ONE token;
    Owed lists both pieces; GAPS section 4.17.3 "Open, each its own piece ..." after {{RUN-GAPS}}; research bound
    bullet moved to the bound branch. Tokens left: {{RUN-DESIGN}} {{RUN-EDGE}} {{RUN-GAPS}} {{RUN-AMENDMENT}}.
    rustfmt +1.98.1 --check on its changed Rust files: clean. Never compiled as a whole yet (r92 compiled hg and
    http-edge only, in the red copy).
  * agents/g303-a650-zlib-bound (worktree g303-a650-bound) on p1: 2570f7b52 (the two oracle tests alone, failing
    first) -> 7a6e9095a (compression::bound zlib arm = zlib-rs compress_bound; research section 24 bullet with r92's
    numbers). Owed at its start: A-number, amendment, status sentence, GAPS open item closed (listed in its message).
  * agents/g303-a650-query-414 (worktree g303-a650-414) on p1: 5b2f4cced (room test's query case, expects 414; part 1
    answers 400) -> d30cc46af (query_refused). Owed at its start: A-number, Decided sentence, GAPS item closed.
  * Old branches kept on origin, worktrees removed: agents/g303-a650-hg-http e6d8b6dd5 (superseded by p1),
    agents/g303-a650-red 14a9877af (dropped by main).
- Expected green for p1 (Windows x86_64, jupiter, when C: has ~15 GB): slates-hg --lib 293 (main 284 + 9: 1 http +
  8 serve::http), slates-http-edge --lib 32 (31 + 1), slates-server --lib vcs_http -- --test-threads=4 all passing
  (main's vcs_http count + 7), fmt -p each clean. Then fill the 4 tokens (Windows x86_64, jupiter; macOS owed), land
  p1 squashed with them on a fresh branch from origin/main (cherry-pick b4020cc4c + the fill, amend into one), merge,
  read stat, markers, push HEAD:main.
- Nit for the 414 piece: part 1's route() doc says 400 for an unreadable query and omits the path's 414; the 414
  piece's hg.rs hunk corrects both.
- Tools (outside the tree): ~/.local/state/slates-fanout/g303-tools/ (rewrap.py, commit messages, the research
  bullet). Run script for the old shape: ~/.local/state/slates-fanout/g303-run-part1-r2.sh (red arms now dropped).

## 2026-11-19 ~19:15-19:55 UTC: RELAUNCHED (fresh g303); part 1 run on the Mac Studio and landing in progress
- Found: worktree ~/slates-work/g303-a650-p1land, branch agents/g303-a650-p1-land 929473182 (unpushed): b4020cc4c
  replayed onto cd84e3811 (range-diff: context only). Pushed it.
- r6 (Studio, 929473182): hg 292/293 -- MY golden every_http_exchange_is_answered_as_mercurial_meant_it inflated
  7.2's zstd frames (2 MiB window, no content size: 2,490,432 bytes charged) in ROOM 1 MiB (it never passed before:
  r92's red masked it under R8). edge 32/32. server lib could not compile: MAIN's break (git.rs NoPacks
  PackRefused::Stalled, removed by 188314131) -- told main; main fixed it (93bd6e807 via 71a43cd97). fmt clean.
- Fix c69a246a2 (hg tests: INFLATE_ROOM 4 MiB for inflated()). r33: hg 293/293, fmt clean (server 36/36 there was
  with a scratch stub patch; main said drop it).
- Merge 58a256f4c (origin/main b1926d201) -> r48: server --lib vcs_http 36/36 (git 10, hg 6, serve 11, waits 9), fmt.
- Landing branch agents/g303-a650-p1-final (worktree ~/slates-work/g303-a650-p1final): 22a71aa1c = ONE commit
  (squash of 58a256f4c onto b1926d201, tree verified equal, + the 4 tokens filled; docs differ from 58a256f4c by the
  fills alone, word-checked). Main moved twice: M1 5d07d1ec8 (merge a6306963e, A-657 part 3: manifests both add
  slates-hg -> one entry each, combined comments; amendment log + research: A-657's section 24 first, mine renumbered
  section 25 and my citation updated; GAPS 4.17.3: A-657 Built verbatim, mine, merged Owed list; my 4.17.3 status
  moved after A-657's). M2 33f27dc8c (merge 3b357a256: A-651 part 1 SSH edge + A-626 part 3: slates-ssh beside
  slates-hg; vcs_http `pub(crate) mod git; mod hg; pub(crate) mod waits;`; log A-626p3, A-651 before mine). Both
  merges verified: every line main added kept except the deliberate comment/Owed-list resolutions; pushed.
- r95 (Studio, 33f27dc8c): hg lib whole + server --lib vcs_http + fmt -- RUNNING. Edge stands (inputs unchanged).
- Sibling finding for main: GAPS 4.17.3 Owed (4) still lists "the SSH edge's loop over serve::stdio (A-651, g294)",
  which A-651 part 1 (6141837da) built (it waits on A-662's stores).
- NEXT: on green, fetch; if main moved, merge again (judge reruns by what it touches); read whole stat; markers;
  `git push origin HEAD:main` alone. Then run-requests entry, progress, report, delete Studio target/logs
  (~/.cache/g303-target, ~/.cache/g303-logs, old ~/.cache/g303-a650), END.

## 2026-11-19 ~20:22 UTC: A-650 PART 1 LANDED -- main 733e6de6b..1ef718820 (ENDING)
- The piece: ONE commit c6e3bea98 (hg, http-edge, server: Mercurial over HTTP (A-650 part 1)) on 545181b30, its tree
  the tested 11865db7f plus docs/doc comments, then merges 6fa7344d3 (e50757a1a), 82c964500 (54075b520 A-676),
  b3b349b24 (a6e688321 A-664p1/A-614p3), 1ef718820 (733e6de6b A-616c7). Branch agents/g303-a650-p1-one.
- Along the way (agents/g303-a650-p1-final, never on main): main moved 9 times; merged a6306963e (A-657: slates-hg
  dep both sides; research section 24 theirs, mine renumbered 25), 3b357a256 (A-651, A-626p3), 77e5ffb46, af520142f
  (A-662 stores, A-649p4), 3329211e0 (A-625 part 3: Authenticator::principal -> async prove; Repositories::locate;
  Answer::Move -- the hg site ADAPTED: prove + Unproven mapping (Unanswered 503, else 401), Located::Elsewhere ->
  Move before find; a new test a_request_for_a_repository_another_shard_holds_moves_there_once_proven), 545181b30.
- Runs (macOS arm64, Mac Studio): r6 (929473182: edge 32/32; hg 292/293 the golden's test room; server lib main's
  Stalled break), r33 (hg 293/293 with INFLATE_ROOM), r48, r95, r107, r115 (hg 306, server 36), r128 (561d07d6f,
  adapted: server 40/40, hg 306/306), r137 (11865db7f: server 40/40, hg 306/306, fmt), r154 (6fa7344d3: server
  40/40, fmt). Later merges judged by what they touch (said in each merge message).
- Studio: ~/.cache/g303-target (13G), g303-logs, old g303-a650 (1.3G) deleted; no g303 paths left there.
- Run request filed (run-requests.md 20:21Z, supersedes 18:45Z): Linux/Windows runs of the three commands.
- LEFT (main starts): A-673 agents/g303-a650-zlib-bound (2570f7b52 red, 7a6e9095a green) and A-674
  agents/g303-a650-query-414 (5b2f4cced red, d30cc46af green), both on the OLD part-1 base b4020cc4c: rebuild each on
  main by cherry-pick (hg.rs changed since: answer is async now; the 414 hunk's route() context may differ). Worktrees
  ~/slates-work/g303-a650-bound and g303-a650-414 kept for them.
- Findings for main: (1) GAPS 4.17.3 Owed (4) still lists the SSH edge's loop over serve::stdio (A-651, g294), built
  by A-651 part 1 (it waits on A-662's stores); (2) vcs_http.rs's module doc Owed still lists "the daemon starting an
  edge", built by A-625 part 3; (3) follow-ups: hand the hg site A-662's stores (vcs_store) and drop its 503 path;
  count the hg site's refusals by kind as git's are; (4) 1fb50b1be ("wip") reached main on 11-18; (5) r92's red run
  masked the golden's real failure under its planted R8 mutation: read a red failure's message, not just its name.

## 2026-11-19 ~20:35 UTC: A-673 LANDED -- main 031e04337..1c6ac5006 (ENDING)
- One commit 465b90d42 (hg: an HTTP answer's room is charged at the linked zlib-rs's own compress_bound (A-673)) on
  7d020df47 = 2570f7b52 (oracle) + 7a6e9095a (fix) cherry-picked, labels A-650 -> A-673, docs (amendment A-673,
  4.17.3 status sentence, GAPS open item closed + Fixed line, research section 25 bullet with the macOS run); merges
  5d06ad298 (c3c2c045f) and 1c6ac5006 (031e04337, A-677). Branch agents/g303-a673.
- Run r181 (Studio, scratch clone of main b04c552b0 + the original commits' diffs): red 16/2 (the oracle's two tests,
  zlib level 1 only, r92's numbers exactly; proptest shrunk to 47 bytes -> 61 vs 60), green hg lib 308/308, fmt clean.
- Studio cleaned (target, logs); worktree g303-a673 and g303-a650-bound removed after landing; g303-a650-414 kept
  for A-674 (its branch is on the old part-1 base: cherry-pick onto main; hg.rs's answer is async now).
- Run request filed 20:35Z (other platforms: hg lib compression).
- NEXT (main starts): A-674 (the query's 414), then the hg site taking A-662's stores without its 503 path, then its
  refusals counted by kind. The two stale-line notes are main's (its next docs commit).

## 2026-11-19 ~20:45 UTC: A-674 LANDED -- main b78296d4c..34f655916 (ENDING)
- One commit 4181843cf (server: the hg site answers a query past the request's room 414 ... (A-674)) on 1c6ac5006 =
  5b2f4cced (failing case) + d30cc46af (fix) cherry-picked; module doc conflict resolved (fix's step 1, A-625 part 3's
  steps 2-6); query_refused made exhaustive (PastAdmission 414; PostArguments, MissingArgument, MediaHeader 400);
  test doc relabelled A-674; docs: GAPS open item closed + Fixed line; amendment A-674. Merges 4ffa6006c (901be4f52)
  and 34f655916 (b78296d4c). Branch agents/g303-a674; test branch agents/g303-a674-test 66c71c0f0 (never lands).
- Run r212 (Studio): red 6/7 ([414, 400, 431, 500]); green vcs_http::hg 7/7, slates-hg --lib http 22/22, fmt clean.
- Studio cleaned; worktrees g303-a674, g303-a674-test and g303-a650-414 removed after landing.
- NEXT (main starts): the hg site taking A-662's stores without its 503 path; then its refusals counted by kind.

## 2026-11-19 ~21:20 UTC: RELAUNCHED (fresh g303) for A-680 part 1, slates-hg serve::indexed (main approved; part 2 is main's)
- Found: worktree ~/slates-work/g303-hg-view, branch agents/g303-hg-view ddc11f306 (pushed, clean): five "wip" commits on
  ee5cbed4c holding ONLY the oracle: crates/hg/fixtures/state/make-served-fixtures.sh + 12 captured files (stock 7.2's
  served answers for states and states-store: served c3daaa7 public, baa2576 draft, b252f2a draft; unserved a1286ad
  hidden, 4ed9423 secret; bookmarks one, two (secret-one dropped); phases baa2576 1; heads/branchmap b252f2a) + SHA256SUMS
  (sums equal on this Mac and the Studio: g303-tools/served-sums-*.txt). No Rust written.
- 7.2's source read on the Studio (uv cache .../2dAcTQyJH-IGHy2t/.../mercurial): repoview hideablerevs/pinnedrevs/
  computehidden/_revealancestors/computeunserved; phases mutable/remotehidden/localhidden; obsolete _computeobsoleteset
  (draft+secret, any marker's predecessor, prunes too); listphases (stored draft roots via the UNFILTERED changelog);
  listbinbookmarks (served hasnode, divergent x@y dropped).
- Plan: fresh branch agents/g303-a680 from origin/main (worktree ~/slates-work/g303-a680), fixtures applied by patch from
  ddc11f306, ONE commit. Store trait: texts lent as Cow with the room charged before a copy; file_node dropped (getbundle
  finds paths in the manifest text it holds). serve::indexed: lazy view (filter state only from the first non-public
  root's revision on), dense renumbering, markers lent, counters.
- Findings for main (not this piece): (a) pull.rs listkeys phases computes the served view's draft roots, 7.2 lists the
  stored draft roots through the unfiltered changelog (an amended draft root is listed by 7.2, not here); (b)
  bookmark_entries does not drop 7.2's divergent names (x@y).
- ~21:35 UTC MAIN: after serve::indexed lands, go straight on (no END) to piece 2: the server's lender with NoStores
  dropped, AND pushes: the site's unbundle (POST) through A-657's hg_push (prepare, admission, hooks, landing) so a stock
  `hg push` over HTTP lands; test = a repository whose store holds history being served AND accepting a push. g083 writes
  the stock-hg interop tests against the site.

## 2026-11-19 ~22:55 UTC: A-680 PART 1 LANDED -- main 9f349d66b..770ae8826 (relaunched g303 after the 22:35Z stop)
- Found: worktree g303-a680 clean, branch agents/g303-a680 8c949e4ff pushed; the predecessor's Studio runs (logs
  ~/.cache/g303-logs there): red at 8c949e4ff serve::indexed 3/6 (6 failing as expected), green serve:: manifest::
  61/61, server vcs_http::hg 7/7 (at f3a4d99e4, same server code), fmt clean.
- Landing: test branch agents/g303-a680-test b3aa5cc12 (main b94ae6e90 + the code diff 0577ab0bd..8c949e4ff; never
  lands) -> kiln r256 (Studio): hg serve:: manifest:: 61/61, server vcs_http::hg 7/7, fmt clean. Landing branch
  agents/g303-a680-land: 942b39a1e = the same code + docs (design status block, decision 4 as 7.2 keeps an obsolete
  changeset, piece 4's line, amendment "A-680 part 1" at the log end; GAPS 4.17.3 header/Built/Owed; research §26),
  merge 770ae8826 of origin/main 9f349d66b (A-651p3, A-655p2b, hg interop CLI tests; amendment-log end conflict
  resolved main's first). Docs script: g303-tools/a680_docs.py.
- Findings sent to main (by reading, not chased): (a) pull.rs listkeys phases lists the view's draft roots where 7.2
  lists the stored draft roots through the unfiltered changelog; (b) bookmark answers keep divergent names (x@y) that
  7.2's listbinbookmarks drops.
- NEXT: part 2 (the site's lender over state.vcs_stores, NoStores dropped, unbundle through hg_push; un-ignore g083's
  HTTP clone/push CLI tests whose other needs have landed). Worktrees g303-a680, g303-a680-test, g303-a680-land and
  g303-hg-view can go (all pushed; g303-hg-view's ddc11f306 is superseded).

## 2026-11-19 ~23:30 UTC: A-680 part 2a in flight (the lender + the hg site on the daemon edge; pushes = 2b, after)
- Branch agents/g303-a680-p2 (worktree ~/slates-work/g303-a680-p2) on main 770ae8826: 25a0cd92a (wip, lands squashed):
  * vcs_http/hg.rs: HgStores is a GAT (`type Store<'s>: Store`) whose `lend(id, room, |view, room| ..)` makes the view
    within the request's room; NoStores/NoStore removed; `wire_request(head, room)` (query names `cmd`, hgweb's
    handlewsgirequest rule). vcs_http.rs: `pub(crate) mod hg`; Decided::map; ToolSites<G,H>/ToolExchange (hg for a
    `cmd` query, git otherwise). daemon_edge.rs: DaemonSite = ToolSites<DaemonGitSite, DaemonHgSite> (ServedViews).
  * vcs_store/hg_view.rs (new): ServedViews (HgStores over state.vcs_stores.mercurial_ref + the register's phase roots
    and `bookmark:` greens' heads; publishing = true until the record has phases.publish; not lent while importing or
    when the view cannot be made, counted vcs_store.hg_view.unmade). vcs_store.rs: mercurial_ref. hg_push.rs:
    bookmark_of (inverse of bookmark_name) + tests.
  * Tests: vcs_store/hg_view/tests.rs t_a680_a_view_is_lent_from_the_store_or_not_at_all; daemon_edge test
    a_mercurial_repositorys_store_is_served_over_the_daemons_edge (states imported on the 2nd shard; capabilities,
    heads vs an independent DAG oracle; git repo asked as hg 404; git request still git's 200|503).
- Run r317 (Studio): `cargo test --locked -p slates-server --lib -- vcs_http:: vcs_store::hg_view
  repository::hg_push::tests::names --test-threads=4` + fmt. No integration target: the stock-hg CLI tests wait on
  A-685 (g305) and A-686 (g258) too.
- Coordination: g304's A-679 branch uses `HgStores` (old signature: `lend(id, |store| ..)`) and `NoStores` in
  vcs_ssh/tests/git.rs; whoever lands second adapts (lend gains the room argument; ServedViews replaces NoStores).
- Docs owed for 2a: design §4.17.3 status + piece 4 line, GAPS 4.17.3 (Built + Owed: "A-662's stores handed to the HTTP
  site" done; pushes remain), amendment "A-680 part 2a" at the log end.

## 2026-11-20 ~00:10 UTC: A-680 PART 2a LANDED -- main 0462bd922..b288b412e (ENDING: context short; 2b not started)
- Piece 9a919b371 (server: Mercurial's site on the daemon's edge ... (A-680 part 2a)) + merge b288b412e (A-664 part 3;
  conflicts vcs_store's module list and the log end, both unions). Branch agents/g303-a680-p2-final (worktree
  ~/slates-work/g303-a680-p2final, kept; every other g303 worktree removed, all branches pushed).
- Runs (macOS arm64, Mac Studio): r317 over 770ae8826 46/46; r357 over 2f73eb075 (the landed code byte for byte) 51/51
  (`cargo test --locked -p slates-server --lib -- vcs_http:: vcs_store::hg_view repository::hg_push::tests::names
  --test-threads=4`), fmt clean. Studio cleaned (no g303 paths).
- Also: the stock-hg CLI tests' ignore reasons (refusal: A-686 alone; round trip: A-680 part 2b + A-686). A-685 landed
  (c3f8ac6fb), so the HTTP round trip waits only on 2b and A-686. Neither test could be un-ignored.
- PART 2b, for the next agent (not started; nothing written):
  1. slates-hg: serve `unbundle` (and `pushkey`) over HTTP: today session::dispatch answers them NotServed and the
     capabilities lack `unbundle=HG10GZ,HG10BZ,HG10UN` and the bundle2 push caps (changegroup versions push::verify
     takes, `checkheads=related`, `phases=heads`, `bookmarks`, `pushkey`, `error=abort,unsupportedcontent,pushraced,
     pushkey`). 7.2's `wireprotov1server.unbundle` answers bundle2 as `streamreslegacy`: `application/mercurial-0.1`,
     uncompressed (hgweb `_callhttp`), a refusal before the bundle's handling as `pusherr` / an `error:*` part stream.
  2. server: HgExchange for a push (POST, Permission::Push): body = the bundle (push::read -> Unbundle); on the shard:
     view = ServedViews' Indexed, held = the index (hg_push tests use `IndexHeld::new(&index, &store)`), terms =
     (PUBLISHING true, a fresh 32-byte push name, room); `hg_push::prepare`, `admitted` (needs the proven principal in
     the exchange), hooks (unix; the pending view is piece 9's: refuse a configured hook or run with what exists), then
     `land(state, (principal, repo), prepared, (&mut index, &mut content), room)` through `vcs_stores.mercurial(id)`
     and `vcs_store::settle`. Reply bytes from `Reply::encode` / `Refused`.
  3. ASK MAIN FIRST: A-662 refuses Mercurial records (owed to g304: "hg_push's Objects handed the Mercurial arm"),
     so a push landed into the map's index is not replayed after a restart while the register keeps its entry.
  4. Coordination: g304's A-679 (vcs_ssh/hg.rs) calls `HgStores::lend(id, |store| ..)` and uses NoStores in
     vcs_ssh/tests/git.rs: on main now lend takes `(id, &mut room, |view, room| ..)` and ServedViews replaces
     NoStores.

## 2026-11-20 ~00:20 UTC: A-680 PART 2b built and green; LANDING in progress (relaunched g303)
- Branch agents/g303-a680-p2b (worktree ~/slates-work/g303-a680-p2b) on base f1fbd2c44, wip commits e87153034..(docs)
  pushed; tree tested = bd10c37e0 (+ docs-only commits after).
- What: slates-hg push::read_bundle/Outcome/Refusal::refused; serve::http::unbundle_request/pushed; session HTTP caps
  (unbundle=HG10GZ,HG10BZ,HG10UN; bundle2 +checkheads=related, error=..., pushkey, changegroup=01,02,03), Refused::Push.
  server: hg_push::land over vcs_stores.mercurial (Objects gone), driver hg_push::unbundle (heads judged Preparing ->
  read_bundle -> prepare over hg_view::with_view -> admitted -> hook gate -> land), push_name BLAKE3(repo, bundle),
  PushOutcome alias (vcs_hooks::Outcome clash); hg_view::with_view + ServedViews::push; HgStores::push; site routes
  `unbundle` (Routed.unbundle, HgExchange.pusher, answered_push). Tests: daemon_edge
  a_stock_mercurial_push_over_the_daemons_edge_lands_on_its_repositorys_shard (wire fixture http-push.07 after the
  rebuilt base/second/third history), hg_push a_push_through_the_driver_..._unless_a_hook_waits, hg push/http tests.
  Docs: design status + pieces 4/5 + amendment "A-680 part 2b" at log end; GAPS Built + Open (not durable); cli.rs
  round trip ignore now A-686 alone; vcs_store.rs Owed doc.
- Runs (Studio): r467 red (branch agents/g303-a680-p2b-red 10df7becc, never lands) 0/1 FAILED as expected (hg-error
  command not served here); r467 green hg 93/93, server compile error (Outcome clash) -> fixed; r480 server 64/64,
  fmt clean. Studio paths: ~/.cache/g303-p2b-src, ~/.cache/g303-target, ~/.cache/g303-logs (delete at END).
- Landing: worktree ~/slates-work/g303-a680-p2b-land, branch agents/g303-a680-p2b-land from f1fbd2c44, merge --squash
  of agents/g303-a680-p2b, message ~/.local/state/slates-fanout/g303-tools/p2b-msg.txt. Then merge origin/main:
  conflicts expected in cli.rs, daemon_edge.rs (+tests), hg.rs, tests/hg.rs, vcs_store.rs, hg_push/tests.rs, GAPS,
  design. Must add `push` to A-679's vcs_ssh/tests/hg.rs `Lends` (HgStores impl) -> None. Rerun on merged tree:
  server --lib -- vcs_http:: vcs_store::hg_view repository::hg_push vcs_ssh::hg --test-threads=4 + hg lib + fmt.
- A-687 NOT on main (checked 00:15Z): land without the record; tell main 2c is owed after A-687.
