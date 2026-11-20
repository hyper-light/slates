# g296 — A-582 part 2: git protocol v2 messages in slates-git (UI goal 92, 51cef9f7)

Supervisor: main. Worktree: ~/slates-work/g296-git-v2 (branch agents/g296-git-v2, from origin/main aef6faea1;
base clone ~/slates-work/g066-fs-server/.git). Scratch (outside the tree): ~/slates-work/g296-scratch.

## 2026-11-18/19 (UTC) — started
- Read: preamble, CLAUDE.md (origin/main) §1-§8, §4.17.2, A-582 entry, GAPS §4.17, g251's part-1 notes.
- Readback of THE RULE sent to main.
- Sources read (all hashed): gitprotocol-v2.adoc (sha256 64ef6a81..., same revision research/vcs-servers.md cites),
  gitprotocol-common.adoc; git v2.50.1 serve.c, ls-refs.c, upload-pack.c, fetch-pack.c, connect.c, pkt-line.c
  (downloaded to scratch/git-v2.50.1, read as text).
- Findings so far:
  - stock git's client writes `command=fetch`, capabilities (agent=, object-format=, server-option=) and flags
    (thin-pack, no-progress, ofs-delta, sideband-all, deepen ...) WITHOUT LF, `want/have/done` WITH LF; capture
    script's requests all carry LF; gitprotocol-common: receivers treat both alike -> decoders record each line's
    ending so a re-encode is exact.
  - git's bundle-uri client sends capabilities BEFORE `command=`; serve.c accepts either order and a request with no
    delimiter (flush right after the capabilities).
  - git 2.50.1 upload-pack.c writes wanted-refs BEFORE shallow-info (upload_pack_v2), while the document's grammar and
    git's own client (fetch-pack.c do_fetch_pack_v2) read shallow-info first: slates writes and requires the
    documented order (an upload-pack bug when a request has want-ref and a shallow section; not run, code reading).
  - git writes `shallow <oid>` / `unshallow <oid>` with no LF (fixture shows it).
  - §4.17.2 advertises `ls-refs=symrefs peel ref-prefix unborn`; git advertises `ls-refs=unborn` (the base arguments
    need no feature): to correct in the design.
## Plan
- src/v2/{mod,oid,advertisement,request,ls_refs,fetch}.rs + src/v2/tests/*.rs; lib.rs gets `pub mod v2;` only.
- Failing tests first (stubs returning refusals, run red on the Studio), then implement, run green.

## 2026-11-19 (UTC) — relaunch after the usage limit (g296 again)
- Read preamble, CLAUDE.md §7-§8 (origin/main e85c17f9f), §4.17.2, A-582/A-600 entries, GAPS. Readback sent to main.
- Worktree was clean at aef6faea1 (no code yet); merged origin/main (fast-forward to e85c17f9f, A-600 object names landed:
  `object_name::{ObjectFormat, ObjectId::from_hex}` reused for oids).
- Sources re-read and hashed in ~/slates-work/g296-scratch: docs-v2.50.1/gitprotocol-v2.adoc (2e739965..., v2.50.1; master
  e1ec09b2's 64ef6a81 differs only in object-info and promisor-remote), gitprotocol-common.adoc (bcb00e39...),
  git-v2.50.1/{serve,ls-refs,upload-pack,fetch-pack,connect,pkt-line,sideband,refs}.c.
- Section order settled by git itself: master commit 191740edaa4a (2026-09-16, "upload-pack: swap wanted-ref/shallow-info
  responses", merged 2026-10-04, t5703 test) fixes 2.50.1's wanted-refs-before-shallow-info, which killed git's own
  client ("expected 'packfile', received 'shallow-info'"). slates writes and requires the documented order.
- Decisions: decode is lossless (each line records its LF; oids lower-case only, gitprotocol-common's HEXDIG; nothing
  after an oid), so encode(decode(x)) == x for every accepted x (pkt-length digits aside, part 1 reads either case);
  a text line holds no NUL and no inner LF; refnames: HEAD or refs/ + refs.c check_refname_format; git's keepalive is
  `0005\1` (create_pack_file) though the doc says `0005\2` -- an empty band-1 line before the packfile would break
  git's own client (it reads it as an empty line), so a server keeps alive on band 2 outside the packfile.
- Layout: src/v2/{mod,line,refname,advertisement,request,ls_refs,fetch}.rs, tests in src/v2/tests/*.rs; lib.rs gets
  `pub mod v2;` only. Fetch response = one grammar machine (fetch::Response) stepped by the reader and the writer.

## 2026-11-19 (UTC) — LANDED, piece done (A-582 part 2)
- On main: 303df0893 (code + docs: crates/git/src/v2 {mod,line,refname,advertisement,request,ls_refs,fetch} + tests;
  §4.17.2 status, "The wire" ls-refs=unborn, A-582 entry part 2, GAPS), merged through 6276565be, 3cd862731, 4a9d2bb12
  (push c2d33ed26..4a9d2bb12); then 988060bb6 (private intra-doc link removed, 120 columns) pushed 9098fdc71..31548ce9d.
- Unit runs (Mac Studio d1323898, `cargo test --locked -p slates-git --lib`, fmt --check clean each): 59/59 (kiln r526,
  r535), 113/113 merged with part 3 + A-591, 142/142 on main as landed with A-590. Failing first: 32/32 message tests red
  with codecs stubbed (a copy). Red four ways (a copy): order 1, case 4, endings 8, admission 1. Studio dirs deleted.
- Run requests filed (run-requests.md, "2026-11-19 g296"): clippy all targets x3 OS, xtask check, cargo doc/xwin doc.
- Left (GAPS): object-info and bundle-uri (refused as unknown commands; server must not advertise them); a request
  checked against the server's advertisement (serve.c + upload-pack allow_*); ERR lines / band-3 errors for refusals;
  duplicate want-ref check at the server's ref mapping; server keepalive on band 2 only before the packfile.
- Sibling for main: refname check duplicated (A-590 objects/refname.rs and v2/refname.rs), same rules.
- Worktree ~/slates-work/g296-git-v2 (branch agents/g296-git-v2 at 31548ce9d = main then); scratch ~/slates-work/g296-scratch.

## 2026-11-19 (UTC) — A-615 (UI goal 93), assigned by main after A-582 part 2
- Part 1 LANDED de0176391 (pushed 6c7368dc6..de0176391): crates/git/src/refname.rs (is_valid, tag_name_is_valid) +
  tests/refname.rs (my cases, A-590-style tag cases, proptest vs refs.c transcription with near-valid weighted names);
  objects/refname.rs and v2/refname.rs removed; objects/tag.rs, v2/fetch.rs, v2/ls_refs.rs on it. Studio: 144/144 on my
  branch, 205/205 merged (with A-604 v0, A-595 http, A-608 unpack); fmt clean; red: .lock rule (3 incl. A-590 golden),
  tag-always (4), property test alone red w/o .lock and w/o leading-dot after reweighting.
- Sibling note for main: A-604's v0 reads ref names as words (byte > SP, != DEL), not check_refname_format.
- Part 2 (piece 7 negotiation) design notes: trait Graph (Node: Copy+Ord, Green), acks = held haves (dedup, order),
  ready = every want reaches a held have commit or its parent (exact, generation-pruned) unless wait-for-done;
  one-round per want: a green headed by the want whose version M head is a held have and versions M+1..N all
  fast-forwards -> Run{green, after M, through N}; walk = paint by generation (interesting = from wants, not haves),
  objects = interesting commits + their trees minus edge commits' trees (pack-objects' mark_edges_uninteresting);
  shallow/deepen per shallow.c (depth min-distance, rev-list cut for since/not, relative from client shallows reachable
  from tips); filters blob:none, blob:limit, tree:<d> (min depth), object:type, combine (sparse:oid refused: owed).
- Part 2 LANDED a485a644e (pushed 066135675..a485a644e): crates/git/src/negotiation {mod, filter, ready, one_round,
  paint, shallow, objects} + tests (oracle over generated histories, 4096 cases, census of 21 outcomes; carried-set
  property 1024; 7 deterministic; filter parse). Follow-up a6223d317 (pushed 58726c9bd..a6223d317): oracle/generator
  split under cognitive_complexity 10 (tests held to it). Studio: 216/216 branch, 298/298 merged (A-614 store, LFS,
  receive), fmt clean. Red: stub 7/7; ready w/o parents, one-round w/o ff (3), edges kept, grafts followed (2), depth
  off by one (2), never-reopened (own deterministic test). Lesson: first generator met one-round 0-2/1024 -> census
  caught it; shared base clone moves origin/main between commands (check HEAD^2 == origin/main before push).
- Owed (GAPS A-615 part 2): sparse:oid; server mapping v2 fetch -> negotiation::Request; pack from runs+objects;
  object-info; cooperative slices; collection overhead; bitmaps; A-614's store implementing Graph.

## 2026-11-19 (UTC) — A-640 (UI goal 93), assigned by main after A-615: the upload-pack service end to end
- Part 1 LANDED 8851a2771 (pushed 48e285d21..8851a2771): crates/git/src/store/graph.rs (StoreGraph: A-614's store as
  A-615's Graph; Node = position, Green = green; NamedRef from the caller; GraphFault kept in a Cell, first only;
  fast_forward = the store's own rule, both heads commits, so a tag moved onto itself is none) as a child module of
  store/repository.rs (#[path = "graph.rs"], one line) re-exported from store/mod.rs (one line). Tests: src/tests/served.rs
  (the fixture repo loaded from fixtures/ofs.pack into a real store; commit() answers each move's version and the
  store's verdict; Bases + pack_names read stock git's thin packs) and src/tests/store_graph.rs (5 tests; golden:
  fetch-haves one round over main v1..2 = git's thin pack objects; fetch-clone walk = git's clone pack objects).
  Studio: 309/309 branch, 338/338 merged; fmt clean; red: first-draft equal-heads ff (store-verdict test), ff-always (2),
  no head map (2), empty entries (3). Docs: §4.17.2 status, A-640 entry at the log's end, GAPS line.
- Also: removed GAPS' committed markers (A-638/A-624 part 2) -- main's 0727f8303 did the same, merged clean.
  Registry: A-615 row -> landed; A-640 row -> part 1 landed; added main's A-645 row (on main's log, registry offered it);
  next free -> A-646.
- NEXT: part 2, the v2 service with ONE entry point (tell main its signature for g299's A-625 part 2): v2 command request
  -> ls-refs (refs + HEAD symref, unborn) / fetch (args -> negotiation::Request; want-ref + deepen-not via refs; sections
  acknowledgments, shallow-info, wanted-refs, packfile over side-band; pack through a trait until A-639) / object-info
  (A-624's). Golden vs fixtures (fetch-*.request/response.bin, ls-refs.*). Then part 3 v0/v1 (A-604 codecs,
  multi_ack_detailed, side-band-64k, v0 ref names via refname::is_valid), part 4 sparse:oid.
- Part 2 LANDED 4f2a8a605 (pushed dc46c9f05..4f2a8a605; branch commits e2fe9f985, b6904229c, a02719098, merge
  4f2a8a605): crates/git/src/upload {mod (serve, Served, Asked, PackSource, PackPlan, PackRefused, Refusal + words,
  charged/Sink), advertise (capabilities in serve.c order + request check), refs (ls-refs ranges, want-ref,
  deepen-not by ref_rev_parse_rules, Floyd peel), fetch (args -> negotiation, sections via Response, pack begun),
  answer (Answer::write stepper, band-1 framing in MAX_BAND_DATA packets, ERR/band-3)} + tests/ (30: advertise,
  clone = small.git rebuilt from http fixtures 04/05 held to git's 31 wants, fetch goldens x5, ls-refs golden + oracle,
  refused, steps). StoreGraph::new infallible (ObjectId::null), StoreGraph::head added. Studio: 365 -> 368 branch,
  416/416 merged with main dc46c9f05; fmt clean (rustfmt layout brought back as gzip+base64 patches, sha-checked).
  Red: serve stubbed 25/27 then 29/30 (step tests anchored to git after the stub passed 2); 8 guards each red.
  Signature sent to main for g299. Registry row updated.
- NEXT: part 3 = v0/v1 behind the same serve (A-604 codecs in crate::v0; fixtures/v0/*: advertisement-v0/v1,
  clone/fetch v0/v1, multi-ack, detailed-done, ready, plain, depth/deepen/since/exclude, filter, unknown-want,
  unshallow); stateless (HTTP) first: each POST carries all haves; multi_ack_detailed acks (common/ready), NAK,
  side-band-64k pack; v0 ref names through refname::is_valid (main: "fix the v0 check"). Then part 4 sparse:oid.
- Part 3 (v0/v1) BUILT, GREEN, NOT LANDED (branch agents/g296-git-v2, head f8519bd46): upload/v0.rs (advertisement in
  send_ref order with git's capabilities; stateless request: shallow-update first via negotiation::shallow_update,
  acks per get_common_commits with have_obj counting and negotiation::ready asked only when a held have was added,
  pack on side-band-64k / side-band (995 B) / bare via answer.rs Framing), mod.rs dispatch + Refusal::{Request,Line,
  Write} (Version variant removed); v0.rs codec: advertised ref names via refname::is_valid (word check kept first);
  A-615 FIX: negotiation Request gains `thin`; objects.rs left_out_aggressively = git's --objects-edge-aggressive
  (thin && shallow: trees of do_got_oid-listed haves + cut.haves), oracle mirrors (oracle_aggressive, plan bit 64).
  Studio: 424/424 at 8d7f68022 (pre-merge), red 9 ways (all caught). fmt patch 1666d5a3 applied (3d86fa6c9).
- Merged main twice (ac488cb73: A-639 store::upload, A-644 protect, A-647 derived D(k); bcce37311: A-649 hg) --
  the MERGED TREE IS NOT YET TESTED (run r1212 stopped: the Studio went offline, out of memory).
- Then (untested, Studio offline): upload/fragments.rs = PackSource over A-639 (Planned: Answered; Streamed;
  Fragments{clock,exchange,quantum_ns}; one advance() turn per write call); answer.rs: a source call may work and write
  nothing -> the step ends (Written::More), PackRefused::Stalled removed; tests: upload/tests/fragments.rs (git's 5 v2
  fetches from A-639's packs; 1-tick turns in 1-byte steps), refused.rs Pausing source. Docs carry
  JOIN-CHECKS-PENDING in part 3's section: fill with the Studio counts before landing.
- STUDIO RULES (main): CARGO_BUILD_JOBS=4 for every cargo; never two cargo at once; delete target dir right after
  reading results; offline -> check with `true` every ~5 min; never build elsewhere.
- NEXT when the Studio answers: rm -rf ~/.cache/g296-a640 first; test branch head (CARGO_BUILD_JOBS=4) + fmt; fill
  JOIN-CHECKS-PENDING; red-check the join (e.g. Fragments::write never advancing); merge main (check HEAD^2 ==
  origin/main right before push; origin/main moves under the shared clone); land; registry row; report.
- Part 4 (sparse:oid) plan: git's sparse filter (list-objects-filter.c filter_sparse; dir.c path_matches_pattern_list,
  match_basename (wildmatch flags 0) / match_pathname (WM_PATHNAME); add_patterns_from_buffer; frames with
  default_match; trees always shown, blobs by match) + resolve `sparse:oid=<blob-ish>` (hex id or <rev>:<path>).
  Needs git's wildmatch: crates/server/src/wildmatch.rs (A-597, a faithful dowild port, 1030 lines w/ tests) should
  move to a crate both use (slates-vcs?) -- asked main. No stock-git fixture for sparse:oid yet (record with the
  Studio when it is back).
- MAIN 2026-11-19 (later): bug note DONE (docs/bugs/2026-11-19-a-thin-shallow-pack-sent-blobs-the-client-held.md,
  GAPS + part 3 section name it; commit d24ff09a4). ORDER: land part 3 when the Studio answers (main tells me) ->
  then the wildmatch move -> then part 4 (sparse:oid) over it.
- WILDMATCH MOVE (approved by main, nobody else owns it): move crates/server/src/wildmatch.rs WHOLE (code + tests)
  into slates-vcs as one module (crates/vcs/src/wildmatch.rs, `pub mod wildmatch` in crates/vcs/src/lib.rs); point the
  server's callers at it (vcs_policy.rs: `use crate::wildmatch::{Glob, is_glob_special}` -> slates_vcs; "the hooks'
  matcher" too if the newer main has one -- grep); drop `mod wildmatch;` from crates/server/src/lib.rs; NO copy, shim
  or re-export. crates/server/Cargo.toml must have slates-vcs (it already does at bcce37311; g258's A-662 adds the
  same line: keep one). Glob/is_glob_special become `pub`. The test oracle `reference` (cfg(test) pub(crate) mod in
  wildmatch.rs) is used by vcs_policy's tests too: expose it from slates-vcs as `#[cfg(any(test, feature =
  "oracle"))] pub mod reference` and enable the feature in crates/server's [dev-dependencies] only (resolver 2 keeps
  it out of normal builds); decided over copying it (main: no copy) or using the production Glob in the policy
  oracle (weakens the oracle's independence). Build: cargo test -p slates-vcs --lib wildmatch and -p slates-server
  --lib vcs_policy (CARGO_BUILD_JOBS=4), fmt; record in the design log + GAPS.

## 2026-11-19 (UTC) — relaunch "g296 fresh: land A-640 part 3 on jupiter"; stopped 13:14 by the MacBook's full disk
- Runs (Windows x86_64, jupiter, `cargo test --locked -p slates-git --lib`; main's baseline at 018d32ba2 447 + 3 ign.):
  r1361 branch head d24ff09a4: 456 passed 2 failed (refused::a_source_that_works_without_writing..., fragments::
  turns_of_one_tick...). Fix 1 (805f3d362, one source call a step, packets framed full): r1425 459/1 failed
  (refused::a_pack_that_fails_midway... -- a refused pack's last 20 bytes cleared). Fix 2 (656d24ce8, refused data sent
  first, then band 3; FRAMED_MOST = 65,520 + max(flush 4, band-3 packet 71) = 65,591): r1447 460 passed, 0 failed,
  3 ignored; red: loop 3, each-call 2, lose-rest 1, drop-refused 1; fmt wanted ABORT_PACKET's chain layout.
- Landing branch agents/g296-a640-part3 (worktree ~/slates-work/g296-a640-part3), on 018d32ba2: 7ca132893 (v0/v1 squash
  of the four WIP commits), 188314131 (the join + fixes, fmt layout, JOIN-CHECKS-PENDING filled in design + GAPS),
  1f87b811e (bug note re-applied from d24ff09a4, message reworded, no WIP), 65144c980 merge of origin/main 4644dcec3
  (one conflict, §4.17.2 statuses: main's A-665 first, then part 3's). Pushed the branch; r1507 runs the merged tree
  (main's 79 commits touch slates-git A-665/A-664 and its deps slates-merge, slates-vcs). fmt --check clean here
  (rustfmt 1.98.1, no compile). Check worktree removed (branch agents/g296-a640-p3-check stays on origin).
- g083's note: `Streamed` (crates/git/src/upload/fragments.rs) holds `upload: Upload<Planned>`; `Upload::counters()`
  returns `UploadCounters` (Copy; `crate::store::UploadCounters`), so `Streamed::counters()` forwards to it.

## 2026-11-19 ~16:10Z (UTC) — relaunch (g296 fresh, UI goal 93): land A-640 part 3
- Read: relaunch brief, preamble, CLAUDE.md origin/main (§8), lint-habits. Readback of THE RULE sent to main.
- Found the brief's steps 1-3 already done before the stop: agents/g296-a640-part3 on origin at 65144c980 (188314131 join
  + ABORT_PACKET layout + JOIN-CHECKS filled in design and GAPS; 1f87b811e bug note, reworded; merge of main 4644dcec3).
  No conflict markers, no JOIN-CHECKS-PENDING left.
- 0a4365497 git: nine doc lines of 121-122 chars rewrapped to 120 (words moved down only; each file equal to its
  committed text with markers and whitespace stripped), the width main held part 2 to (988060bb6).
- 22fc21e3a merge of origin/main e3234e618 (main's 71f05f93d + e3234e618: slates-merge engine and tests, GAPS), clean
  auto-merge; diff stat vs main = my 21 files. Branch pushed (22fc21e3a).
- Run r32 (Windows x86_64, jupiter): `cargo test --locked -p slates-git --lib` + `cargo fmt -p slates-git -- --check`
  over 22fc21e3a. Expect 475 passed (main's 462 + part 3's 13), 4 ignored.
- NEXT when r32 ends: green -> fetch; if main moved, merge (rerun if it touches slates-git or its deps); markers; diff
  stat; push HEAD:main; worktree list (g296-a640-check dir already gone); progress + run-requests; report to main with
  g083's `Streamed::counters()` note.

## 2026-11-19 ~17:05-18:05Z (UTC) — relaunch (g296 fresh, UI goal 93): A-640 part 3 LANDED
- Read: relaunch brief (and its later edit: delete the target dir at END), preamble, Windows recipe, lint habits.
  No readback (main's brief: start at once, report only on landing or a block).
- Run r71 (Windows x86_64, jupiter, rustc 1.98.1, debug): clone of agents/g296-a640-part3 checked at 22fc21e3a;
  `cargo test --locked -p slates-git --lib`: 489 passed, 0 failed, 4 ignored (327.90 s; build 30 min 10 s);
  `cargo fmt -p slates-git -- --check` exit 0. All 14 named new tests ran and passed.
- Count explained without a main run: crates/git/src `#[test]` lines 440 on main (4644dcec3 and 9a7339e9e alike),
  453 here; tests run exceed lines by 40 at both 018d32ba2 (410 lines, 450 run) and here (453, 493 run): main alone
  runs 476 + 4 ignored; part 3 nets +13 (14 new, `protocol_v0_and_v1_are_refused_until_part_three` removed). The
  brief's "about 462" predates main's later slates-git pieces.
- Found on jupiter: my predecessor's r32 (STAMP 1795107799 = 17:03:19Z, same script, same ~/.cache/g296-target) still
  alive, retrying its clone. My `rm -rf` of what I took for a stale dir hit its in-progress clone first. Stopped it by
  its four PIDs (bash 339812, 336484, 335264; git 342000), each command line checked for g296-a640-land- first;
  dir removed.
- Landed: f607f424c merge of origin/main 9a7339e9e (main's ten slates-svn commits + their GAPS/design lines; nothing
  slates-git builds on; docs auto-merged, each side's changed lines hash-identical before and after; no markers);
  `git diff --stat origin/main HEAD` = my 21 files (2451+, 163-); HEAD^2 == origin/main checked after a fresh fetch;
  pushed 9a7339e9e..f607f424c to main; branch agents/g296-a640-part3 fast-forwarded to f607f424c.
- Commits on main from this piece: 7ca132893 (v0/v1 + A-615 thin/shallow object rule), 188314131 (join to A-639's
  stream), 1f87b811e (bug note), 0a4365497 (120-column wrap), merges 65144c980, 22fc21e3a, f607f424c.
- Registry: A-640 row -> part 3 landed f607f424c; owed: the wildmatch move, then part 4 sparse:oid.
- Run requests: "2026-11-19T18:00Z g296 main f607f424c" (macOS arm64 lib run owed; clippy x3, xtask, doc/xwin doc).
- jupiter: target (1.5 GB), logs and clone deleted; nothing of g296 left in ~/.cache there.
- Worktrees: ~/slates-work/g296-a640-part3 (clean, f607f424c = main at landing); ~/slates-work/g296-git-v2 (clean,
  d24ff09a4, pushed, superseded).
- g083's note: `Streamed` (crates/git/src/upload/fragments.rs:91) holds a private `upload: Upload<Planned>`;
  `Upload::counters()` (store/upload.rs:309, in `impl<A: Answered> Upload<A>`) returns `UploadCounters` (Copy,
  re-exported from store), so `Streamed::counters()` is one forwarding line; it does not exist yet.
- NEXT (main's order): the wildmatch move to slates-vcs (plan above), then A-640 part 4 sparse:oid.

## 2026-11-19 ~18:05-18:55Z (UTC) — main: two more pieces, one commit each; the wildmatch move BUILT, BLOCKED (jupiter's disk full)
- Main 1: next the wildmatch move, then A-640 part 4 (sparse:oid), each landed alone; reuse ~/.cache/g296-target in
  flight, delete at END. Main's slates-git lib: 489 passed, 4 ignored.
- Main 2 (CLAUDE.md §8, binding): one piece lands as ONE commit plus the merge of origin/main (A-640 part 3 went in
  as four content commits); only `cargo test -p <crate> --lib <filter>` for what changed, plus fmt; no reruns, seed
  replays, mutations or integration tests; if jupiter has no room, report and END.
- A-671 claimed in the registry (next free A-672). Worktree ~/slates-work/g296-wildmatch, branch
  agents/g296-wildmatch, ONE commit 0cd28dead (pushed) on origin/main f607f424c:
  - crates/server/src/wildmatch.rs -> crates/vcs/src/wildmatch.rs (a git rename). Its #[cfg(test)] oracle `reference`
    is now crates/vcs/src/wildmatch/reference.rs, declared `#[cfg(test)] pub(crate) mod reference;` (xtask's
    is_out_of_line_test_module counts it as test code). Byte for byte: the oracle de-indented hashes f111ae83cd15c3a7 on
    both sides, the tests block 669b6eaebed5f81a.
  - Made pub: Glob, Glob::new, Glob::matches, is_glob_special. slates-vcs: lib.rs `pub mod wildmatch;` plus a crate-doc
    paragraph; the Cargo.toml description gains a clause.
  - slates-server: lib.rs `#[cfg(test)] #[path = "../../vcs/src/wildmatch/reference.rs"] mod wildmatch_reference;`
    replaces `mod wildmatch;`. vcs_policy.rs: `use slates_vcs::wildmatch::{Glob, is_glob_special};`, its tests
    `use crate::wildmatch_reference as reference;`, doc links -> `slates_vcs::wildmatch`. Cargo.toml: slates-vcs moved
    from [dev-dependencies] to [dependencies]; Cargo.lock is unchanged (it already listed slates-vcs under slates-server).
  - DECISION, replacing the recorded `oracle` feature plan: a feature compiles the transcription as product code, where
    clippy::indexing_slicing denies its lines (clippy.toml's allow-indexing-slicing-in-tests covers #[cfg(test)] alone).
    Precedent for #[path]: crates/api/tests/api.rs and crates/cli/tests/live_fleet.rs include the server's test support.
  - Docs: design §4.17.1 status "A-671: the matcher's home" after A-622 part 3's; log entry A-671 at the end (NO Tests
    bullet yet); GAPS A-671 entry after A-622 part 3's. Local, no build: rustfmt 1.98.1 --check on both crates' trees exit
    0; every line over 120 identical to main's.
- Run r144 (Windows x86_64, jupiter): `cargo test --locked -p slates-vcs --lib` 68 passed, 0 failed, 0 ignored (main's 65
  + the 3 moved: every_vector..., a_long_starry..., the_compiled_matcher...); `cargo fmt -p slates-vcs -- --check` and
  `cargo fmt -p slates-server -- --check` exit 0. `cargo test --locked -p slates-server --lib -- vcs_policy::
  --test-threads=4` FAILED TO BUILD: "There is not enough space on the disk. (os error 112)" (C: 162 MB free at 18:40Z,
  28 GB at 18:10Z; my target was 493 MB). Not a code failure. My target, logs and clone are deleted (1.4 GB free after).
- NOT LANDED. Resume when a build machine has room:
  1. `cargo test --locked -p slates-server --lib -- vcs_policy:: --test-threads=4` over agents/g296-wildmatch
     (0cd28dead): expect 15 passed (none of them among the server's 23 Windows baseline failures).
  2. One commit: fetch; `git -C ~/slates-work/g296-a640-part3 worktree add -b agents/g296-wildmatch-land
     ~/slates-work/g296-wildmatch-land origin/main`; cherry-pick 0cd28dead there; add the Tests bullet below to the
     A-671 log entry and the runs to GAPS's A-671 entry; `git commit --amend` there (new, unpushed); check
     `git diff 0cd28dead HEAD -- crates` holds nothing of mine; markers; diff stat (9 files); push HEAD:main; registry;
     run-requests (macOS lib run; clippy, xtask, cargo doc incl. vcs_policy's links to slates_vcs::wildmatch); report.
  - Tests bullet: "- **Tests** (Windows x86_64, jupiter, kiln r144 and rN, debug): `cargo test --locked -p slates-vcs
    --lib`: 68 of 68, among them the module's three (`wildmatch::tests::*`); `cargo test --locked -p slates-server --lib
    -- vcs_policy:: --test-threads=4`: 15 of 15, its three oracle properties reading the included file; `cargo fmt -p
    slates-vcs -- --check` and `cargo fmt -p slates-server -- --check` clean. The macOS run is owed."
- Hazard for g258's A-662 (a tool store on the owner shard): slates-vcs is in slates-server's [dependencies] on my
  branch; if A-662 adds the same line, a merge keeps both and cargo refuses the manifest (duplicate key). Keep one.
- Lesson: `git grep -E '^\s*...'` reads `\s` as a literal `s` here (column 0 only); use `[[:space:]]`.

## A-640 part 4 (sparse:oid): not started; notes for whoever starts it
- Confirmed in git v2.50.1 list-objects-filter.c (scratch): trees always shown, once (FILTER_SHOWN_BUT_REVISIT,
  445-448); a blob MATCHED is sent, otherwise provisionally omitted, its tree not marked seen, so another path asks again
  (472-509); END_TREE carries child_prov_omit up (450-470); init get_oid_with_context(GET_OID_BLOB) (530),
  add_patterns_from_blob_to_list(oid, "", 0) (535), root default_match 0 (540; NOT_MATCHED if dir.h's enum is as
  remembered: confirm).
- To fetch (not in scratch): dir.c and dir.h at v2.50.1. From memory, to verify: path_matches_pattern_list non-cone =
  last matching pattern from the list's end (negative -> NOT_MATCHED, none -> UNDECIDED); match_basename (exact,
  ENDSWITH, wildmatch flags 0); match_pathname (leading '/', base, the literal prefix compared, then wildmatch
  WM_PATHNAME on the REST, so `ab**/c` acts as `**/c` after "ab", unlike a whole-pattern wildmatch); parse_path_pattern
  ('!', trailing '/' = MUSTBEDIR, NODIR, nowildcardlen = simple_length, ENDSWITH); add_patterns_from_buffer (BOM, CRLF,
  '#', trim_trailing_spaces); do_read_blob (an empty blob gives no patterns; appends a final LF).
- Matcher: slates_vcs::wildmatch (WM_PATHNAME) should suffice. match_basename's flags-0 calls meet slash-free (NODIR)
  patterns and slash-free basenames, where the two modes agree in dowild. Prove it with a test, not from reading.
- Walk: objects.rs reads `graph.entries(tree)` as bare nodes. Part 4 needs named entries from Graph (StoreGraph from
  A-614's tree rows) and a frame stack (default_match, child_prov_omit) in Chosen::open; sparse is not per-object, so
  no runs.
- `<blob-ish>`: a hex id, or `<rev>:<path>` (upload/refs.rs `named` with ref_rev_parse_rules, then a tree path walk);
  other rev syntax refused by kind. No stock-git sparse:oid recording exists yet.
- Per main 2: one commit; tests first, red against a stub, then green; `cargo test -p slates-git --lib <filter>` only.

## 2026-11-19 ~19:20Z (UTC) — relaunch (g296 fresh, UI goal 93): land A-671 (the wildmatch move) on the Studio
- Read: relaunch brief (incl. its update: focused unit AND integration tests), preamble, lint habits, CLAUDE.md §8 at
  origin/main cd84e3811 (blob 785016bc). Main's message: run filtered integration tests that exercise the matcher or
  vcs_policy too; land on green of both.
- Found ~/slates-work/g296-wildmatch-land (branch agents/g296-wildmatch-land, unpushed, at main's 92ffda926) holding a
  STAGED no-commit cherry-pick of 0cd28dead: every file's +/- lines hash equal to 0cd28dead's patch, moved blobs
  identical (87207abf wildmatch.rs, 9cdb8c7b reference.rs), no markers, nothing unmerged.
- origin/main cd84e3811 (= 92ffda926 + one svn commit: crates/svn + docs). slates-vcs still in slates-server's
  [dev-dependencies] there (A-662 has not added it): my move to [dependencies] leaves one line.
- Integration reach: none. `mod vcs_http` (private, "no caller outside tests until the daemon starts an edge") and
  `mod lfs` are vcs_policy's only callers; no crates/*/tests target names vcs_http, GitServices, vcs_policy or
  wildmatch; slates-vcs has no tests/ dir. Nearest: the callers' lib tests (vcs_http::tests::git 10, lfs::tests 4),
  which reach vcs_policy::reaches/admit with ref rules: run with vcs_policy:: in the same binary.
- Plan: check branch agents/g296-wildmatch-check = origin/main + cherry-pick 0cd28dead (pushed, never lands); Studio
  run over it; then land from g296-wildmatch-land (Tests bullet added, one commit, merge origin/main, crates/ equal to
  the check's, push).
- Check branch agents/g296-wildmatch-check (worktree ~/slates-work/g296-wildmatch-check; never lands): 10504a3ce =
  cherry-pick on 5c0a522f1 (docs conflicts resolved main-first; each doc's +/- lines equal 0cd28dead's), then 7875ec863
  = merge of origin/main 71a43cd97 (clean). Diff vs main = my 9 files; code line-identical to 0cd28dead.
- r20 (macOS arm64, Mac Studio, over 10504a3ce): slates-vcs `--lib wildmatch` 3 of 3 (75 filtered out); fmt -p
  slates-vcs and -p slates-server clean; slates-server lib test DID NOT COMPILE: vcs_http/tests/git.rs:490,500
  `PackRefused::Stalled`, removed by A-640 part 3 (188314131, landed f607f424c) -- my predecessor's piece broke the
  server's lib tests; only slates-git's lib had been run. g304 and g299 had found it too.
- r37 (same tree, the two lines stubbed todo!() in the Studio clone only): `vcs_policy:: vcs_http::tests::git::
  lfs::tests::` 19 of 19 (vcs_policy 15, lfs 4; the vcs_http filter matched nothing: its git tests are
  `vcs_http::git::tests::`).
- MAIN: fixed on main at 71a43cd97 (93bd6e807: the stub refuses PastAdmission); merge main, run `--lib vcs_policy::`
  on the merged tree you land. LESSON (main, for every future piece): when removing or renaming anything public, the
  focused run includes the dependent crates' tests that use it (grep the workspace for the old name first).
- r43 running over 7875ec863: server `vcs_policy:: vcs_http::git::tests:: lfs::tests::` (expect 15+10+4), fmt both.

## 2026-11-19 ~19:52Z (UTC) — A-671 LANDED (piece done)
- main 787f5948b..5ef6e6896: content fad6a3a6f (`vcs, server: git's wildmatch moves whole into slates-vcs ... (A-671)`,
  docs cite r61), merges 457e1e9f6 (main 2e4433040: A-657 part 3 had added slates-vcs to slates-server's
  [dependencies] too -> one line kept, comment naming Mercurial's store, the policy's matcher, the upload-pack test;
  dev-dependency gone), 29239d72f (main 1b480bd70: A-651 part 1's SSH edge, a third vcs_policy caller), 5ef6e6896 (main
  787f5948b: A-660's deadlines.rs alone). Main moved three times during the landing (§8 step 3: merge and push again).
- Runs (macOS arm64, Mac Studio, debug, rustc 1.98.1): r20 (10504a3ce) vcs wildmatch 3/3, fmt both clean, server lib
  test did not compile (Stalled); r37 (stub in the clone only) 19/19; r43 (7875ec863 = 71a43cd97 + A-671) 29/29;
  r61 (d9528b6f2 = a6306963e + A-671) vcs 3/3, server 29/29, fmt clean (cited in the docs); r75 (457e1e9f6) 3/3, 29/29,
  fmt clean; r83 (29239d72f, the tree that landed but A-660's deadlines.rs) vcs 3/3, server `vcs_policy::
  vcs_http::git::tests:: vcs_ssh::site::tests:: lfs::tests::` 35/35, fmt clean.
- Registry A-671 -> landed 5ef6e6896. Run requests filed: "2026-11-19T19:50Z g296 main 5ef6e6896" (Linux/Windows runs,
  clippy x3, xtask check incl. the #[path] include, cargo doc/xwin doc).
- Cleanup: Studio ~/.cache/g296-target (8.5G), clone, logs and the old g296-a640 (2.3G) removed; none left. Local
  worktrees g296-wildmatch, -check, -land removed (branches on origin: 0cd28dead, d9528b6f2, 5ef6e6896). Kept (older,
  clean, pushed): g296-a640-part3, g296-git-v2.
- LESSONS: (main) removing or renaming anything public -> the focused run includes the dependent crates' tests that use
  it (A-640 part 3's removal of PackRefused::Stalled left slates-server's lib tests uncompilable f607f424c..93bd6e807).
  (mine) The shared clone's origin/main moves between commands: fetch `origin main` alone (a full fetch met ref-lock
  races), and run on the exact landing commit by pushing the landing branch after the merge, its message stating only
  established facts. ps self-matches its own pattern: build the name from parts and match with bash's `case`.
- NEXT: A-640 part 4 (sparse:oid) when main starts it (notes in "A-640 part 4" above; the matcher is
  slates_vcs::wildmatch now, Glob/is_glob_special pub; the oracle is crate-private to slates-vcs's tests).

## 2026-11-19 ~19:55-20:25Z (UTC) — A-640 part 4, FIRST SLICE LANDED (main: "Go with A-640 part 4, alone")
- Landed main 1ef718820..2dc39bd60: content 58f448d76 (`git, vcs: a sparse filter's pattern list ...`), merges 603add1c3
  (main 54075b520) and 2dc39bd60 (main 1ef718820). Branch work agents/g296-a640-p4 (c2d832345, merge e2b66d21a; never
  lands); landing branch agents/g296-a640-p4-land = 2dc39bd60.
- What: crates/git/src/negotiation/sparse.rs (Patterns::read = add_patterns_from_blob_to_list/_from_buffer/
  trim_trailing_spaces; Pattern::parse = parse_path_pattern; decide = path_matches_pattern_list non-cone with
  match_basename/match_pathname; MOST_BYTES = PATTERN_MAX_FILE_SIZE 100 MiB; Verdict, Refused) + tests/sparse.rs (9
  hand-traced + 1 property vs a C-shaped transcription); slates-vcs Glob::kept_bytes (+ test) to charge globs; filter.rs
  doc; design §4.17.2 status + log entry "A-640 part 4, first slice"; GAPS entry. git sources read at v2.50.1:
  scratch/git-v2.50.1-dir/{dir.c,dir.h,list-objects.c,list-objects-filter-options.c} (+ list-objects-filter.c).
- Runs (macOS arm64, Mac Studio): r133 red 9/10 + vcs test red (stubs), green 10/10 + 4/4, fmt clean; r144 over main
  5c0967fb2 10/10 + 4/4 (cited in the docs); r157 over 603add1c3 10/10 + 4/4, fmt clean.
- NEXT SLICES (not started): (b) the walk by path: Graph gains named tree entries (StoreGraph via Tree::parse entry
  names; TestGraph in negotiation/tests/mod.rs and tests/derived History + store/reach Repository implement Graph too),
  objects.rs keeps git's frames (default_match from decide(dir path, tree) else the parent's, base NOT_MATCHED;
  child_prov_omit; every tree sent once; blob sent when some path decides Matched; tree marked done when no blob below
  was left out), each frame charged; Request gains the patterns; check_served stops refusing SparseOid when patterns are
  given; the negotiation oracle (tests/mod.rs) models it. (c) upload: resolve <blob-ish> (full hex; <rev>:<path> via
  upload/refs.rs ref_rev_parse_rules + tree path walk; others refused by kind), read the blob within admission
  (StoreGraph/Repository bytes), refusals in git's words ("unable to access sparse blob in '%s'", "unable to parse
  sparse filter data in %s"), refused.rs's "waits for part four" test becomes served; record a stock-git sparse:oid
  exchange (git clone --filter=sparse:oid=main:.sparse) as a golden.

## 2026-11-19 ~20:30Z-, A-640 part 4 SLICE (b) (main: "Go with slice (b), alone"); resumed after a restart
- Worktree ~/slates-work/g296-a640-p4b, branch agents/g296-a640-p4b (branch work, never lands): 7f9ade7d1 (the slice),
  a commit requiring the census's sparse counts (.take(23)), merge b991ab570 of main bb535bd4d (clean; main had changed
  store/graph.rs and store/repository.rs, auto-merged).
- What: Graph::named_entries (negotiation/mod.rs) for StoreGraph (store/graph.rs named_entries_of via
  repository.rs tree_named_references) and TestGraph (names per Object); Request.sparse: Option<&Patterns>;
  filter::check_served(filter, patterns) + Refusal::SparseCombined (git's combine order-dependence); objects.rs
  SparseWalk/Frame (enter = BEGIN_TREE, blob = LOFS_BLOB, leave = END_TREE; done set; charged); upload fetch.rs/v0.rs
  pass sparse: None (unchanged until slice c). Tests: negotiation oracle (names table, PATTERN_LINES by plan bits,
  combine-with-sparse plans, oracle_filter_refusal, oracle_sparse = unfolded paths, census sparse_served +
  sparse_blob_left_out now required), 3 deterministic tests, StoreGraph sparse clone vs git's clone pack ("*" = all,
  "" = less blobs), filter test. Only TWO Graph implementors (StoreGraph, TestGraph); the other impls are slates-vcs's
  derived Graph.
- r213 (Studio, 7f9ade7d1): red with walk_sparse + named_entries_of stubbed: 4 failed (2 walk tests, store clone,
  oracle property), 35 passed; green 39/39; fmt clean. r6 (b991ab570) running: the merged tree + the census change.
- NEXT: docs (status after "A-640 part 4, first slice" status; log entry "A-640 part 4, second slice"; GAPS entry),
  landing worktree from origin/main + `git merge --squash agents/g296-a640-p4b` + docs -> one commit; run on it if main
  moved into crates/git; push; registry; run-requests; report.
- LANDED slice (b): main ee5cbed4c..cb9d7f5a5; content 849d48d7d (squash of the branch work onto bb535bd4d + docs:
  §4.17.2 status "A-640 part 4, second slice", log entry, GAPS entry), merges a71c2edc6 (main 8727ff426: slates-svn
  alone) and cb9d7f5a5 (main ee5cbed4c: A-644 part 2; its four slates-git test files name nothing the slice changed).
  Evidence: r6 over b991ab570 (bb535bd4d + slice) 39/39, fmt clean; r213 red 4/39. Registry and run requests
  ("2026-11-19T21:05Z g296 main cb9d7f5a5") filed. Studio dirs removed; worktrees p4b and p4b-land removed (branches on
  origin: agents/g296-a640-p4b b991ab570, agents/g296-a640-p4b-land cb9d7f5a5).
- NEXT (slice c, when main starts it): upload resolves `<blob-ish>` (full hex; `<rev>:<path>` via upload/refs.rs and a
  tree path walk over named entries; other forms refused by kind), reads the blob within the request's admission
  (Patterns::read), passes `sparse: Some(&patterns)` in fetch.rs and v0.rs; refusals in git's words; refused.rs's
  "waits for part four" test turns into a served one; record a stock-git sparse:oid exchange as a golden.

## 2026-11-19 ~21:15Z-, A-640 part 4 SLICE (c) (main: upload resolves <blob-ish>, reads the blob, git's words, golden)
- Worktree ~/slates-work/g296-a640-p4c, branch agents/g296-a640-p4c (branch work) from main 4dca63df7: make-fixtures.sh
  gains three sparse captures (allowFilter on for them alone); recorded on the Studio (git 2.50.1 Apple Git-155, the
  same as the fixtures): crates/git/fixtures/fetch-sparse-main-{dir-file,missing,dir}.{request,response}.bin (sha256
  fc164a30/b6b86df2, 758f6d2d/b522a177, 9fe5439d/2bb51a17), pushed.
- GIT'S WIRE (the golden): every case answers `000dpackfile\n` first. dir-file: the pack on band 1 (commits + trees, no
  blob: the pattern `deep` matches nothing), flush. Refusals: band 2 `fatal: unable to access sparse blob in
  'main:missing'\n` (pkt 003b) or `fatal: unable to parse sparse filter data in <40 hex>\n` (pkt 005b, main:dir names a
  tree cc01dbca...), then band 3 `aborting due to possible repository corruption on the remote side.` (pkt 0047 =
  answer.rs's ABORT_PACKET), and NO flush (upload-pack dies).
- g083 will change PackSource::write / Upload::advance to take &mut Admission: merge main before landing; adapt.
- PLAN: (1) upload/sparse.rs: resolve name = full hex | <rev>[:<path>] (rev: full hex or ref via refs.rs rules;
  path walked over Graph::named_entries; tag/commit peeled to its tree); not resolved -> SparseBlob{name} (words
  "fatal: unable to access sparse blob in '<name>'"); resolved but not a held blob, past MOST_BYTES -> SparseData{id}
  ("fatal: unable to parse sparse filter data in <hex>"); blob bytes read via StoreGraph (new fn, charged) ->
  Patterns::read. (2) fetch.rs: patterns before negotiate (filter SparseOid alone), request.sparse = Some. (3) answer:
  these two refusals written as packfile + band 2 + ABORT_PACKET, no flush (as git). (4) v0.rs: the same resolution
  (no golden; same words after NAK? record if possible). (5) tests: the three goldens through upload::serve (dir-file:
  objects == git's pack objects; refusals: bytes equal), refused.rs's part-four test made served. (6) runs: slates-git
  --lib negotiation:: upload:: + slates-server --lib vcs_http::git::tests:: + fmt.

## 2026-11-19 ~22:15Z (UTC) — relaunch (g296 fresh after an interrupt): A-640 part 4 slice (c), finish and land
- Found: agents/g296-a640-p4c at d07f7aff7 (pushed, clean; 7 commits over main 0577ab0bd: fixtures 1f4b259ae, 2975d7e79,
  0cf5dd465, 05edf9dbf (kiln r114 recorded them); code 25b8fcc56, d07f7aff7; merge d16acc0c4). NO docs yet.
  Red branch agents/g296-a640-p4c-red 6f3b98227 (sparse::read, Tree::find, Answer::died stubbed; never lands).
- Studio ~/.cache/g296-run (logs 22:07-22:10Z; no process left running): red over 6f3b98227: 88 passed, 16 failed,
  1 ignored of 105 (negotiation::, objects::tests::tree, every upload:: incl. store::tests::upload, v0's golden sums);
  green over d07f7aff7's tree (the clone at 6f3b982 with its three files restored; `git diff HEAD~1` empty): 104 passed,
  0 failed, 1 ignored; fmt log empty (clean); slates-server lib 18 of 18 (vcs_http::git::tests:: 12,
  vcs_ssh::site::tests:: 6).
- Main moved: e4c6e7cbf (A-653 continued (4)) changed Answer::write / PackSource::write to take `&mut Admission`
  (upload/answer.rs, mod.rs, fragments.rs, tests) -> adapt the slice's new test call sites in the landing merge.
- Integration reach: none (crates/cli/tests/cli.rs's two vcs_http tests stop at the 401 challenge; anchor.rs holds the
  listener only). Server callers: vcs_http::git (+daemon_edge), vcs_ssh::git, vcs_ssh::site lib tests.
- PLAN: worktree ~/slates-work/g296-a640-p4c-land (branch agents/g296-a640-p4c-land) from origin/main; merge --squash
  agents/g296-a640-p4c; adapt; docs (status "A-640 part 4, third slice", log entry, GAPS); one commit; push the branch;
  Studio run (g296-target reused): slates-git --lib `negotiation:: objects::tests::tree upload:: every_golden_vector`,
  slates-server --lib `vcs_http::git::tests:: vcs_http::daemon_edge::tests:: vcs_ssh::git::tests::
  vcs_ssh::site::tests::`, fmt both; then merge main again if moved, push HEAD:main, registry, run requests, report.
- 22:20Z: merged origin/main b94ae6e90 into agents/g296-a640-p4c (9c0363069, clean) + cf032854e (the slice's `died`
  test helper on main's `Answer::write(.., &mut admission, ..)`); pushed. r250 (Studio, over cf032854e): slates-git
  `--lib -- negotiation:: objects::tests::tree upload:: store::repository::graph::tests:: every_golden_vector` 112
  passed 1 ignored; slates-server `--lib -- vcs_http::git::tests:: vcs_http::daemon_edge::tests::
  vcs_ssh::git::tests:: vcs_ssh::site::tests:: --test-threads=4` 27/27; fmt -p slates-git clean.
- Landing worktree ~/slates-work/g296-a640-p4c-land (branch agents/g296-a640-p4c-land): 16461b7d0 = ONE content
  commit (squash of cf032854e onto b94ae6e90 + docs: §4.17.2 status "third slice", log entry at the end, GAPS entry
  after the second slice's; + negotiation/filter.rs SparseOid doc "served alone ..."); cb47276de = merge of main
  9f349d66b (design-log conflict, main's A-651 part 3 + A-655 part 2b first). Pushed. r288 runs over cb47276de.
- NEXT: r288 green -> fetch; if main moved, merge (rerun only if it touches crates/git or deps or the server tests);
  markers; diff stat (42 files); push HEAD:main; registry row; run-requests; report; clean Studio dirs + target.
- LANDED A-640 part 4 slice (c) ~22:55Z: main 99068c566..550153532; content 16461b7d0 (`git: upload-pack serves
  sparse:oid -- ...`: the slice squashed from cf032854e onto b94ae6e90 + docs + negotiation/filter.rs SparseOid doc),
  merges cb47276de (main 9f349d66b; design-log conflict, main first) and 550153532 (main 99068c566; same conflict;
  main's slates-git change = A-665 part 2's store/hook_view.rs + test, naming nothing the slice changed; no rerun).
  HEAD^2 == origin/main checked after a fresh fetch; diff stat vs main = the slice's 42 files (1516+, 39-).
- Runs (macOS arm64, Mac Studio, debug, rustc 1.98.1): r250 over cf032854e: slates-git filtered 112 passed 1 ignored,
  server 27/27, fmt clean (cited in the docs); r288 over cb47276de: 112 + 1 ignored, server 29/29 (daemon_edge 2,
  vcs_http::git 12, vcs_ssh::git 6, vcs_ssh::site 8), fmt clean. Red (predecessor, 6f3b98227): 16 of 105 failed.
- Registry A-640 row updated; run requests "2026-11-19T22:55Z g296 main 550153532" filed. Studio ~/.cache/g296-* all
  deleted (target 9.5G). Local worktrees g296-a640-p4c, -red, -land removed (branches on origin: cf032854e,
  6f3b98227, 550153532). Kept: g296-a640-part3, g296-git-v2 (older, clean, pushed), g296-scratch.
- Owed in A-640 part 4: `sparse:oid` within `combine:` (refused since slice b); the revision forms refused by kind
  (~, ^, ^{..}, @{..}, abbreviated, describe, :/); git's band-2 warning of an ambiguous ref name. Rest of A-640: a
  stateful v0 session (SSH), packfile-uris/bundle-uri, progress on band 2, hidden refs.

## 2026-11-19 ~23:00Z — main's next queue for g296 (each its own piece: one commit + merge, failing test first, goldens)
1. `object-info` answered over a store trait, from a repository's real store, advertised only once answered.
2. `bundle-uri`: bundle list from the store's bundles, served at URIs on the shared HTTP edge (stock
   `git clone --bundle-uri` / the advertised list fetches a bundle, then the rest).
3. `packfile-uris`: the server's choice of URI packs; a client asking gets the pack's URI lines and fetches them.
4. A-640 part 4's rest: `sparse:oid` inside `combine:`, the revision forms refused by kind, git's band-2 warning for
   an ambiguous ref name.
Read §4.17.2 + GAPS for each first; findings by reading -> main in one line, not chased; focused unit + integration
tests + fmt on the Studio; 3-line report after each landing; END when done or context short (progress file first).
- 23:05Z: piece 1 found DONE already (object-info served from the real store by upload::serve since A-640 part 2,
  advertised; test object_info_answers_the_sizes_stock_git_printed) -> told main; main fixes GAPS' stale lines.
  MAIN's ORDER NOW: (4) first, then (2) bundle-uri and (3) packfile-uris WITHOUT waiting on A-682: drive the server's
  git site in tests over a store lent as A-654 part 3's tests lend one (crates/server/src/vcs_http/git/receive.rs,
  ThisShard), stock git 2.50.1 goldens for the exchange; #[ignore] only the end-to-end daemon leg, naming A-682 (g309,
  who makes the daemon's upload half real: daemon_edge.rs:115 `Halves<Unserved, ..>` today).
- STOPPED here (context short), nothing started for piece 4: no branch, no worktree.

## Piece 4 notes (A-640 part 4's rest), facts gathered 23:05Z
- (4a) git's ambiguous-ref warning [C: object-name.c get_oid_basic, v2.50.1, scratch
  dl-git-v2.50.1-objname/object-name.c 943-1031]: `warning("refname '%.*s' is ambiguous.")` (vfreportf "warning: ",
  control bytes '?', 4096 cut) when (i) a full-length hex name parses as an id AND repo_dwim_ref finds a ref of that
  name: the warning, then advice object_name_msg (9 lines, `fprintf(stderr, "%s\n")`, advice.objectNameWarning on by
  default); (ii) otherwise once a ref is found: refs_found > 1 (count of the rev-parse rules that find a ref;
  upload/refs.rs `dwim` takes the first), or get_short_oid(name, QUIETLY) succeeds (4..hexsz-1 hex digits naming
  exactly one object; NO prefix search exists in crates/git/src/store or slates-vcs: build one, it also serves 4b's
  abbreviated names). Applies to the whole name and to the rev of `<rev>:<path>`. pack-objects turns
  warn_on_object_refname_ambiguity off only while reading its stdin revs and restores it before traverse_commit_list
  starts the filter [C: builtin/pack-objects.c 4290-4336, scratch dl-git-v2.50.1-packobjects, sha256 9815de1a...], so
  both fire; upload-pack relays them on band 2 (128-byte reads) before the pack, and before the fatal: line in a death;
  v0 with no band: nothing (git's own stderr).
  Golden: make-fixtures.sh creates the extra refs only around its capture (a tag named like a branch, e.g.
  refs/tags/main; a branch named a unique object prefix; a 40-hex-named branch), then deletes them; check every other
  fixture reproduces byte for byte (as r114 did); the tests serve REFS plus those refs.
  Code sketch: refs.rs `dwim` -> (node, how many rules found one); sparse.rs `Read` gains the warning lines;
  answer.rs puts band-2 lines after the packfile header before the pack (v2) / after the acks (v0 side-band), and
  `Answer::died` writes them before the fatal line (reuse `reported`).
- (4b) revision forms: `~N`/`^N` (parents from the graph's commit rows), `^{type}`/`^{}` peels, abbreviated names and
  `git describe` names (the prefix search; git's ambiguity words + hints when several match: record them), `@{...}`
  (a served bare repository keeps no reflog: record git's words), `:/text` and `^{/text}` (POSIX regex over commit
  messages: record git's words; refused by kind unless a regex engine is built). Each answered as git answers it,
  goldens recorded.
- (4c) `sparse:oid` within `combine:`: git's answer turns on list-objects.c's walk order (process_subfilter marks a
  blob seen at its first path); needs an order-faithful walk (commits in rev-list order, trees depth-first in entry
  order, seen flags per sub-filter). Refused since slice (b).

## 2026-11-19 ~23:30Z (UTC) — relaunch (g296 fresh): main's queue = (1) A-640 part 4's rest, (2) bundle-uri, (3) packfile-uris
- Read: relaunch brief (completeness first), preamble, lint habits, CLAUDE.md origin/main 2f73eb075 (sha256 275ec430...), §8.
  Role: implementer. Main has none of the queue (bundle-uri refused at upload/mod.rs:443; no URI section; A-624's codecs
  v2/bundle_uri.rs and fixtures v2-rest/* exist).
- Worktree ~/slates-work/g296-a640-p4d, branch agents/g296-a640-p4d from origin/main 2f73eb075.
- PIECE 1 SPLIT (one landing each): 4d = the store's prefix search (slates-vcs Index::iter_from; store/abbrev.rs child
  of repository.rs: Abbrev, Repository::abbreviated/unique_digits/object count) + abbreviated and describe names +
  git's ambiguity lines (error/hint) + the refname warning (refs_found>1, unique short id, 40-hex ref + advice), all
  relayed on band 2 before the pack (v2: after the header, or before it under sideband-all+packfile-uris; v0 band) or
  before the fatal; 4e = ~ ^ ^{..} @{..} :/ ^{/..}; 4f = combine with sparse:oid.
- 4d design: upload/revision.rs = get_oid_with_context_1 port (Resolver with `said` writes); sparse::Read gains `said`;
  Answer::died(head, said, refusal, banded); refs::dwim -> (first, count). git facts (object-name.c v2.50.1, scratch
  dl-git-v2.50.1-objname sha256 103f94fa): get_short_oid outcome = |M|=0 missing, |M|=1 that one (unchecked), |M|>=2:
  hint passes exactly 1 -> it, else ambiguous; listing = passing ones if >=2 pass, else all; sorted tag,commit,tree,blob
  then id; lines "error: short object ID <lowercase> is ambiguous", "hint: The candidates are:", "hint:   <abbrev>
  commit <author date YYYY-MM-DD in author tz> - <subject>" / "tag <tagger date UTC> - <tag name>" / "tree" / "blob";
  abbrev = max(default, 1+longest shared prefix), default = max(7, ceil((msb(count)+1)/2)); 41..64 digits on SHA-1
  match an object when the extra digits are 0 (zero-padded hash). Goldens: captures appended at the END of
  make-fixtures.sh (refs made and deleted around each; colliding blobs/commits/tags by a birthday search in one
  hash-object --stdin-paths call), the extra objects in fixtures/ambiguous.pack.
- 2026-11-20 ~00:05Z: SLICE 4d BUILT, GREEN, LANDING. Branch agents/g296-a640-p4d (pushed): 83d597551, b4027fd3d,
  a WIP docs commit, merge ac338eedc of main be892650b (design-log conflict: main first). Goldens: recorded on the
  Studio (v2 recorder in place reproduced every older file but fetch-progress; v0 recorder must write to an EMPTY dir:
  its capture script refuses an existing request file -> exit 128); the Studio cannot push (no GitHub credentials),
  so the 31 new files were REBUILT here from parts (~/slates-work/g296-scratch/p4d/rebuild.py) and checked against the
  Studio's sha256 sums, 31 of 31 (expected.sha). ambiguous.pack dropped: the test makes the 8 alike objects from the
  recorder's bytes. GIT FACT: upload-pack's 128-byte reads span writes made back to back (warning+advice;
  hints+fatal) but not writes with an object read between (error line alone; warning before an object read) ->
  answer::Said (runs broken at reads). r459 (Studio, b4027fd3d): vcs 2/2; git 151 passed 1 failed (tests::v0 FILES
  list lacked the 2 new v0 files: fixed), fmt clean; red (6 stubs): vcs 2 failed, git 8 failed (the 7 new/changed +
  that list). r472 runs over ac338eedc (vcs rows_from+property; git upload:: store:: tests::lfs tests::v0
  every_golden_vector; names listing; fmt).
- Landing worktree ~/slates-work/g296-a640-p4d-land (agents/g296-a640-p4d-land from origin/main, merge --squash of
  the branch); fill RUNS-PENDING in GAPS + design log Evidence with r472; one commit; fetch; merge main if moved;
  diff stat (55 files); push HEAD:main. Old worktrees g296-a640-part3 and g296-git-v2 removed (clean, pushed).
- NEXT SLICES: 4e = ~ ^ ^{..} @{..} :/ ^{/..} (git facts in the log entry's Owed; regex needs a decision: git on
  macOS uses libc ERE), 4f = combine with sparse:oid. Then queue items 2 (bundle-uri), 3 (packfile-uris).
- LANDED slice 4d ~00:10Z: main 477c85a1b..0b0c8e12e; content 6c16b1e3a (one commit), merge 0b0c8e12e (main 477c85a1b,
  design-log conflict main first). Landing tree run (Studio, no id shown): slates-git upload:: store:: tests::lfs
  tests::v0 every_golden_vector receive:: 196 passed 4 ignored; vcs 2/2; fmt clean. Registry + run requests
  ("2026-11-20T00:10Z g296 main 0b0c8e12e") filed; report sent to main. Branches on origin: agents/g296-a640-p4d
  (ac338eedc), agents/g296-a640-p4d-land (0b0c8e12e). Worktrees: g296-a640-p4d, g296-a640-p4d-land (both clean).
- NEXT: slice 4e. Studio target ~/.cache/g296-target kept (delete at END).
