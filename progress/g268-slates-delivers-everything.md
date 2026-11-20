# g268 ITEMS (goal c2e4337b: slates delivers everything the Shards, Hex and Mantle agents' kiln work needs)
# One line each; [x] = implemented, unit-tested and on main; runs that prove it are filed in run-requests.md.
# REWRITTEN 2026-11-18 ~22:50Z by the g268 agent from what it held, after ~/.cache was wiped at 14:04 PST (the old
# file is gone). The status entries below are verbatim from this agent's own read of the old file earlier the same
# day; the message log before 19:50Z and any older owed lists that file held could not be recovered (see "Lost").
- [x] 1. Hex: slates-api seam trait + simulator + conformance suite (A-310, ac30fc77a; current at CONTRACT 30, f21bcc893)
- [x] 2a. A-197 copies dormant through the write log's replay (A-389, dd7694113)
- [x] 2b. a held stream logs its FORGETs (A-390, c2be93689)
- [x] 2c. a resynced stream told its lost writes after a log overflow (A-393, 6b418765b)
- [x] 2d. a stream's close publishes nothing the log holds; measured 17.5-20.0 -> 15.7-16.0 us p50 (A-399, 236b2f8d5)
- [x] 2e. the write log on every platform: Windows streams' acknowledged writes logged before the reply (A-402,
      e0b59a1fe; g079's Windows-streams finding; Windows run filed)
- [x] 3. vhost-user device: each WRITE logged in the write log before its used element, or held for the barrier when the
      log cannot take it (A-410, f756f7a62; merges bfff518ea, 3ab8b9528 = main)
- [-] 4. Windows stream resync: DROPPED, g234 claimed it (inbox 19:45Z; told g234 nothing of mine exists, 2026-11-18)
- [x] 5. GAPS' route table (A-379's entry): Windows streams' row -> A-402, vhost-user's row -> A-410 (in A-410's commit)
- [x] 6. Mantle CP-T10.4: export (A-441, a0b569ca7, merge f5e9191c4: Instance::export_archive, contract 38); import
      (A-453, c17716d94, merge 2e41e8028: Instance::import_archive, contract 40)
- [~] 7. Mantle CP-T10.5: an overlay's base read from S3 objects. Part 1 LANDED (A-464, b3194636f): the design and
      pure core. Part 2 LANDED (A-471, 24c90fac3): the network client. Part 3 LANDED (A-524, c8731b725; UI part
      01a1f121 marked met): S3Host (HostFs over the client, misses parked as HostError::Pending/VfsError::BasePending),
      Part 4 A-555 part 1 LANDED (63c83464e; UI piece ce3e002e met): requests park on the fetch they met (NFS),
      the retry at the bound (FUSE EAGAIN, NFS JUKEBOX), the landed fetch held; BaseHost the slot's host.
      SimS3 its oracle, S3Base the library's naming (slates-api contract 46). Owed (GAPS A-464): the volume over an S3
      base: the wire's create + credential hand-off (anchor holds it across restarts), the shard's fetch driver, the
      server's parking on BasePending for every bridge, the base plane's audit for a pending answer
      (fingerprint_at_path's open_file().ok()?), recovery, read-ahead, trust roots, name resolution, the N=1 differential.
- [x] 8. (coordinator's piece) A-422: a left-out volume's logged writes kept across a restart + Windows dead code
      (b57c3b142; merge da3a13add)
- [x] 9. (coordinator's piece) A-429: the replay writes an overlay's logged write through its host (13bc7dfe8; merge
      645211744)
- [x] 10. (coordinator's piece) A-430: a checkpoint carries a left-out volume's record; kept records' blocks stay
      allocated (2936f0a98; merge bd2c385d0)
- [x] 11. (coordinator's piece) A-436: a full log's forced publication settles a write by its own volume (076d6b5e1;
      merge 05f257e2d)
- [x] 12. (coordinator's piece) A-459: export's archive written as a stream, so no snapshot is held whole in memory
      (A-441's gap): aedce71d0 (fast-forward on main).
- [x] 13. (UI goal 92, 51cef9f7) A-592: §4.17.4 Subversion piece 5, the commit rule as a pure function with its FSFS
      oracle: LANDED main 99c72e534 (code 95d997551 + 9ab58863b + c20fa087e; merges 49644159b, c83d0d58f, 99c72e534).

POLICY_READ (Shards, 2026-11-18 ~19:10Z): origin/main 9dcc91bbb, docs/development-rules.md sha256 df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3, AGENTS.md read in full; role implementer, READ-ONLY on Shards (no edits, builds or tests there; owned Shards paths: none); task: derive slates' side of the Windows stream resync (goal c2e4337b item 3) from Shards' --slates-fs-handle route.
POLICY_READ shards origin/main 17c45a2332eda7ee60cebe09d00ae153c2f740eb (fetched 2026-11-18T05:22Z by the fourth g268 agent, restarted on ROOT's answer); docs/development-rules.md sha256 df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3; AGENTS.md sha256 f66d304c033ad4001d3d7347b67faa3bb3137d637860793e8e74f1eb6266db41 (30 lines); docs/development-evidence.md sha256 61613a244106d95b2759b38c8b6660ff29a616983e7afd6e9faabdc38368fb63 (47 lines); all three read in full; role implementer (slates subagent g268; Shards ROOT 01a1e563073f7059b33f854b2d8a83c7 is coordinator: audits, profiling, benchmarking, heavy E2E and every Shards main merge); task g268 (i) slates enforces guest POSIX ACLs, (ii) slates' side of the restart resync channel (ROOT 4a/4b), (iii) Shards branches kept ready for ROOT after v0.8.0 (no tag yet at 05:22Z); owned Shards paths: none (Shards-side work only as signed branches agents/g268-* for ROOT, never main)

# g268-slates-delivers-everything — progress

- Goal id: c2e4337be1898467917d5906bd014b47 (kiln goal 268 / UI goal 90, Ada 2026-11-18). Status in the UI: active, NOT
  met (item 7 left). Marked nothing met (the coordinator asked 2026-11-18 ~22:20Z to mark done goals done).
- Goal: slates delivers everything the Shards, Hex and Mantle agents' kiln work needs from it: each interface or fix they
  wait on (from their plans and handoffs) is built, tested with their side, and recorded in GAPS.
- Agent: Claude subagent g268-slates-delivers-everything of the slates supervisor (kiln 01a14885fc1d72fba8b659837cc43b94),
  on adas-macbook-pro-2.
- Integrator: kiln agent 01a1ec2e937e76aab63110cad91f8fc3.
- Not mine: g265 owns the Shards share API and microVM engine (do not duplicate).
- Peers: Mantle 01a1488745a977f58e4224bc796f3308; Shards ROOT 01a1e563073f7059b33f854b2d8a83c7.
- Studio: runner d1323898b255a3ed435eb83fa0f7b0cf; scripts ~/.cache/g268-run.sh (clone a branch fresh, run commands)
  and ~/.cache/g268-red.py (mutations from a Python literal file) on the Studio; target
  ~/.cache/slates-work-target-g268-slates-delivers-everything (deleted after every landing).

## Clones
- slates: ~/slates-work/g268-slates-delivers-everything (branch agents/g268-slates-delivers-everything-macos-resync,
  at main c8731b725 after A-524)
- Shards: ~/slates-work/g268-slates-delivers-everything-shards (blobless; READ-ONLY)

## Message log (what this agent holds; earlier entries lost in the wipe)
- 2026-11-18 ~19:45Z inbox: g234 claimed item 4 (Windows stream resync); told g234 nothing of mine exists.
- 2026-11-18T21:22Z -> Mantle (agents_send key g268-mantle-archive-export-landed-2026-11-18): export_archive landed
  (A-441, contract 38).
- 2026-11-18 ~22:30Z -> Mantle (key g268-mantle-archive-import-landed-2026-11-18): import_archive landed (A-453,
  contract 40), its refusals by kind, the streamed encoder/decoder still owed.
- 2026-11-18 ~23:59Z -> Mantle (key g268-mantle-s3-base-design-up-2026-11-18): the S3 base design (§4.15 "An S3
  base", A-464) and its pure core are up; parts 2 and 3 follow.
- Shards ROOT: signed branches handed for its coordinator after its v0.8.0 tag (from GAPS):
  agents/g268-slates-rev-bump (808cdc1e4, pinning slates ac30fc77a and Rust 1.98.1) and
  agents/g268-whp-window-edge-cases (the WHP window patches 0001-0003, record renumbered PM M620).

## Shards-owed list (Shards' side, after its v0.8.0 tag, landed by its coordinator from signed branches; GAPS
## "Still owed for Shards", read 2026-11-18 ~22:45Z)
- its device's half of the restart resync (slates' side built: A-316, A-339, A-365, A-389, A-390, A-393, A-399,
  A-402; Windows wire A-418/A-423/A-427/A-433/A-448 by g234);
- its own server enforcing a guest's ACLs as slates does (FUSE_POSIX_ACL, queued by its coordinator);
- its guest test over a slates stream on Linux and macOS (tests/slates_fs.rs; Windows only today);
- the bump of the slates revision it pins (agents/g268-slates-rev-bump, 808cdc1e4);
- the WHP window patches 0001-0003 (agents/g268-whp-window-edge-cases);
- `shards restore` handing a restored VM a served share (--slates-fs-fd, --slates-fs-handle) with a real-VM test of a
  save and a restore.

## Unrun requests (RE-FILED 2026-11-18T23:30Z in run-requests.md; main ran only the server lib on Linux at 3022c6894,
## r241: 420 passed)
- A-436: the Linux FUSE kill test and the stream replays (fuse.rs is Linux-only: its compile + kill tests).
- A-441 and A-453: the whole api suite (`cargo test --locked -p slates-api --test api`), the server lib, the fleet
  takeover tests (restore in turns; `verbs::restore_budget` now shared), slates-archive whole, wire_order; clippy and
  the doc gate (main's).
- A-422: a restart test through a real daemon; A-410: Linux/macOS whole-binary and real-VM runs.

## Lost
- The old file's message log before 19:50Z, the receipts of the first three agents beyond the two above, and any
  owed lists it kept beyond GAPS: gone with ~/.cache at 14:04 PST; GAPS and the design hold every landed item's record.

## Status
- 2026-11-20 ~04:05Z (relaunched g268, A-652 part 3b EXPORT, landing): predecessor had built the export on branch
  agents/g268-a652-3-land (wip 156da11bc, 6bbd03a39, 0c770ddc2; pushed agents/g268-a652-3b-wip). Merged origin/main
  (a03f76c16, one conflict: the design log's end, main first; pushed agents/g268-a652-3b-wip2). Landing worktree
  ~/slates-work/g268-a652-3b (branch agents/g268-a652-3b-land2 from origin/main, the piece's diff staged, uncommitted,
  EVIDENCE_TO_FILL in the design's A-652 part 3 continued entry). Worktree g268-a652-3 removed (landed). Studio run
  (script ~/slates-work/g268-scripts/a652-3b-run.sh, log a652-3b-run.log): fmt, wire_order, server lib
  repository::svn_export vcs_store::svn_import. NEXT if green: fill evidence, commit once, push HEAD:main; then queue 2
  (the lock-expiry case, by use only).
- 2026-11-20 ~01:30Z: A-652 part 3 IMPORT half LANDED on main 477c85a1b (da3349308..477c85a1b; piece cb18b1b24, merges
  6b727ce4d, 488b218da, 940d2a10d, 7d9172bf7 (its push refused), e0349a878 (refused), 477c85a1b). Evidence: Studio r462
  on 940d2a10d: fmt clean; svn commit:: migration:: 94/94; wire_order 2/2; api 2/2; server vcs_store:: repository::
  72/72 (later merges touched none of the piece's files: script g268-scripts/a652-3-remerge.sh checks that, the exact
  stat, markers, then pushes at once since this Mac's fetch+merge takes ~5 min and main moved 4 times meanwhile).
  API contract 55 (main's A-689 took 54). amendments.tsv A-652 row updated (backup .bak-g268-a652-3). Scratch branches
  deleted; Studio clone/logs deleted; target ~/.cache/g268-target (24 GB) KEPT for the export. Report sent to main.
  Worktrees: ~/slates-work/g268-a652-3 (wip branch at 203be07a4; merge origin/main into it for the export),
  ~/slates-work/g268-a652-3-land (= main 477c85a1b; remove after). Removed landed worktrees a592, a617, a655-2,
  a655-2b-final2 (disk was ~7.5 GiB free).
  NEXT: A-652 part 3 EXPORT half: RequestBody::ExportDump{repository, first, last, deltas, name} -> a new volume holding
  DUMP_FILE ("repository.dump"), ReplyBody::DumpExported{id, dump_bytes}; job like image.rs ArchiveJob (measure walk,
  admit exact, write walk a copy slice at a time, hash both walks, publish+completion one atom via image::complete);
  refusals: NotFound/Forbidden/Unsupported(tool)/BadRequest(range, E165004)/AlreadyExists/BudgetExceeded.
- 2026-11-20 ~00:40Z (A-652 part 3 IMPORT, landing): r381 green on 2315a364a (fmt clean; svn commit:: migration:: 94/94;
  wire_order 2/2; api 2/2; server vcs_store:: repository:: 58/58). Landing worktree ~/slates-work/g268-a652-3-land
  (branch agents/g268-a652-3-land from 770ae8826): piece commit cb18b1b24 (tree == wip 203be07a4), merge 6b727ce4d with
  origin/main 5206419be (conflicts: API contract -> main's A-689 took 54, mine 55; vcs_store.rs module lines; the
  amendment log's end). Run r432 on 6b727ce4d in flight. NEXT if green: push HEAD:main (if main moved, merge again and
  judge), report, then the EXPORT half. PROCESS SLIP (report to main): pushed 6b727ce4d to my own scratch branch
  agents/g268-a652-3-wip with --force-with-lease, replacing wip 2315a364a there (nothing lost: the wip commits stay in
  local branch agents/g268-a652-3); scratch pushes go to fresh branch names from now on.
- 2026-11-19 ~23:55Z (A-652 part 3, the IMPORT half, in flight): worktree ~/slates-work/g268-a652-3 (branch
  agents/g268-a652-3 from main 770ae8826), wip commits 659d7fc5c, 2315a364a pushed to agents/g268-a652-3-wip. Design:
  slates-svn Importer hands each record over (Importing::Commit{consumed, record}; pending Begin/End; holding(),
  loading()); Record::Uuid (kind 2), Record::RevisionProperties{revision, properties} (kind 3); Repository::set_uuid pub,
  check_/set_revision_properties (replacing set_revision_zero), commit_record; server vcs_store/svn_import.rs (begin,
  turn, run, abandon; buffer/fill/charge at turn end), svn_commit::record (objects-only entry), push.rs records an entry
  of no change holding its caller's record, replay_svn via commit_record, repository/import.rs dispatch by tool +
  svn_reply_of/svn_refusal_of; ipc ImportFault::Subversion{code, revision: Option<u64>, path} (tag 9),
  ReplyBody::DumpImported{revisions, youngest} (tag 78); api CONTRACT 54. Docs written (status block, piece 6, amendment
  entry with EVIDENCE_TO_FILL, GAPS). Run r349 (659d7fc5c): svn 94/94, wire_order 2/2, api 2/2, server 56/58 (two of mine
  expected revision Some, got None: fixed by Importer.loading kept across refusal). Run r381 on 2315a364a in flight
  (script ~/.cache/g268-a652-run.sh <branch> <sha>; fmt --check now; target ~/.cache/g268-target).
  NEXT: if green, fill evidence, make ONE commit on a fresh worktree from origin/main (git diff origin/main..wip |
  git apply --index), merge origin/main, push HEAD:main; then the EXPORT half (A-652 part 3 second half) as its own piece.
  Findings to send main (one line each): main's non-test server build warns 21x (lfs.rs dead code, vcs_http/git.rs unused
  UploadServices); A-678 changed the API contract (ImportFault re-exported, Invalid::Import) without raising CONTRACT.
- 2026-11-19 ~23:10Z: A-655 part 2b LANDED on main e25799e8c (17f4f7d6f..e25799e8c: piece cb57b107f + merge
  e25799e8c, two conflicts: repository.rs's module lines, the amendment log's end). Evidence: macOS arm64, Mac Studio,
  kiln r241 on the scratch merge f1cab2c1a (= the landing tree but for A-651 part 3, which touches none of the piece's
  files or names; checked by diff: code + GAPS identical to main's own change): fmt --check clean (svn, server, merge);
  slates-svn `commit:: migration:: serve::` 115/115; slates-merge `engine::copy_tests::t_a655` 1/1; slates-server
  `repository:: vcs_store:: lfs:: git_push::` 59/59. No integration target creates a Subversion repository (none run).
  amendments.tsv A-655 row updated (backup amendments.tsv.bak-g268-a655-2b). Remote branches agents/g268-a655-2b{,-land}
  deleted; worktrees -2b, -2b-land, -2b-final removed; Studio clone + logs deleted; target ~/.cache/g268-target (13 GB)
  KEPT for A-652 part 3. Worktree ~/slates-work/g268-a655-2b-final2 = main e25799e8c (reuse or remove).
  NEXT: A-652 part 3 (main's queue item 2).
- 2026-11-19 ~22:55Z (A-655 part 2b, landing; relaunched fresh after the ~22:35Z interrupt): the predecessor's last
  run (Studio, script ~/.cache/g268-a655-2b-run3.sh, log ~/.cache/g268-a655-2b-merged2.log) was green on 38e74afa8
  (merge with main 3b0b95379): fmt clean, svn 115/115, merge 1/1, server 55/55. Main moved to b94ae6e90 (A-678 hg
  import, A-653 cont. (4)). Scratch merge f1cab2c1a on agents/g268-a655-2b-land (two conflicts: repository.rs module
  lines, A-678's `import` kept beside `svn_commit`; the amendment log's end, main first). Studio run kiln r241 on
  f1cab2c1a (same script). Broken -final2 dir (interrupted worktree add, 568 files) removed; re-adding the worktree on
  agents/g268-a655-2b-final2 (= cb57b107f). NEXT if r241 green: in -final2 `git merge --no-commit origin/main`, take
  f1cab2c1a's conflicted files, `git diff --cached f1cab2c1a` empty, commit with the evidence (msg file
  g268-scripts/a655-2b-final-merge4-msg.txt), push HEAD:main. The stale -final worktree (37713ee4e + a staged merge of
  f05af5288) is superseded: remove it after landing.
- QUEUE (main, ~22:50Z, Ada: completeness first): 1. land A-655 part 2b; 2. A-652 part 3; 3. the lock-timing case
  (svn lock.c:457), only reproduced by use first (stock svn against the daemon's svn:// once g234's `--vcs-svn`
  lands, a lock expiring mid-commit); fix only if it reproduces.
- 2026-11-19 ~21:50Z (A-655 part 2b, landing; relaunched fresh): the predecessor's landing worktree was an interrupted
  `worktree add` of bb535bd4d (4,047 of 7,120 files, nothing applied, registration gone): removed. Its leftover local
  branch agents/g268-a655-2b-land (= bb535bd4d) reused: worktree ~/slates-work/g268-a655-2b-land. Piece commit
  cb57b107f (real message; tree == 5824536a8, r22-tested: svn 88/88, merge 1/1, server 51/51, log re-read on the
  Studio). Scratch merge 9d81d528b with origin/main 3612d1ef9 (one conflict, the amendment log's end: main's five
  entries first, A-655 2b last; code auto-merged; static checks: EntryParts sites, StepRefusal uses, helpers, GAPS
  hunks). Pushed to agents/g268-a655-2b-land. Studio run kiln r90 (script ~/.cache/g268-a655-2b-run2.sh <branch>
  <sha>, log ~/.cache/g268-a655-2b-merged.log): fmt --check + svn `commit:: migration:: serve::` + merge t_a655 +
  server `repository:: vcs_store:: lfs::`. NEXT if green: fresh worktree from cb57b107f, `git merge --no-commit
  origin/main`, take 9d81d528b's SLATES_DESIGN.md, check `git diff --cached 9d81d528b` empty, commit the final merge
  message carrying r90, push HEAD:main; then delete branches agents/g268-a655-2b{,-land,-land2}, the Studio clone,
  target, logs and scripts.
- 2026-11-19T21:10Z (A-655 part 2b, in flight): built on agents/g268-a655-2b (worktree ~/slates-work/g268-a655-2b):
  wip 37f52be33 (code+tests), wip2 (svn test borrow fix, WORD_BYTES dropped, docs incl. the amendment entry whose
  Evidence bullet holds the marker EVIDENCE_TO_FILL -- replace before landing), merge 5a192ec6d with origin/main (one
  conflict, the amendment log's end, main's first). First Studio run (kiln r221, 37f52be33): slates-merge t_a655 1/1;
  slates-server repository:: vcs_store:: lfs:: 39/40, the one failing main's pre-existing
  each_verb_moves_the_names_log_and_each_members_version_its_own_log (A-662 recorded it failing on main 71ebf2a74);
  all 6 svn_commit tests green; slates-svn's test build failed (E0505 in record/tests.rs, fixed). Final run r22 on
  5a192ec6d started ~21:08Z (script ~/.cache/g268-a655-2b-run.sh on the Studio, target ~/.cache/g268-target, clone
  ~/.cache/g268-a655-2b-src, fmt patch ~/.cache/g268-a655-2b-fmt.patch). NEXT: apply the fmt patch locally (base64
  over kiln), fill the evidence, land as one commit on a fresh branch from origin/main (git diff origin/main HEAD |
  git apply --index), push HEAD:main; then delete the Studio clone/target/logs and the branch.
- 2026-11-19T19:40Z (A-655 part 2b, UI goal 93; relaunched fresh ~19:15Z after the process exit): read the relaunch
  brief, restart preamble, origin/main CLAUDE.md section 8, lint-habits, this file. Worktree ~/slates-work/g268-a655-2b
  (branch agents/g268-a655-2b from origin/main effb30ac2; A-662 at acc2ac16f in it; A-658 NOT on main, no judge_computed).
  PLAN (one landing): (1) slates-svn commit::record: Record{Created{uuid,date}, Revision(Revision)}, magic SSVR v1 LE,
  golden + hostile + round trip; (2) slates-svn: Repository::set_room, a re-judge at a moved head (FSFS's merge again:
  verdict(index, head, rows, LockTable::verdict(CommitLocks, now)) then check), StepRefusal::Verdict{refusal,user}, the
  drive's commit words shared; (3) slates-merge: a public head read of the tree beneath a path (Green::beneath) for a
  directory's deletion; (4) server vcs_store: SvnStore holds Option<migration::Repository> + its charge, replay of svn
  records (Created then Revision: check+apply), lend/settle to the shard budget; (5) server repository: Subversion's
  CreateRepository entry carries the Created record (uuid from the repository id, svn:date from wall_ns), install_root
  replays it; (6) server repository/svn_commit.rs: the step (store, re-judge, check, increment from rows: Create+Insert,
  Mkdir, Delete+Insert, Unlink/Rmdir beneath (head read), Copy(+REPLACE), SetXattr/RemoveXattr slates.svn.props =
  list identity; NODE_EXCLUSIVE), A-635 append with the record in the entry's objects (EntryParts gains objects), then
  apply; refused typed: a directory deleted or replaced beneath a same-revision copy from an older version, a replace
  without copy that changes kind (engine TypeChanged, GAPS 8f). Owed after: engine tree read at a version, A-656 wiring.
- 2026-11-19T11:35Z: A-655 part 2a LANDED on main 9caa5055d (78a670256..9caa5055d; commits b0850e364 6b61231c4
  05439ac97 code+tests, becd7f218 fmt, 6e0c2e796 merge, 9caa5055d docs). Built on Windows x86_64 jupiter (main's order,
  the Studio offline since 07:27Z): red ALL 19/442 failed (10:43-10:51Z), then each guard alone, each failing only its
  own tests (M6 date 14, M7 revprops 1, M8 txn- 2, M9 start-commit 4, M10 pre-commit 4, M2 head 2, M3 locks 1, M4
  content 1, M5 room 1, M1 index plan 1; 10:52-11:09Z); green 442/442 in 15.04 s on 05439ac97 (11:10-11:16Z; main's
  Windows baseline 426); cargo fmt from jupiter, each file's sha256 matched; landing merge brought no slates-svn input.
  Main's answers folded in: (1) a Moved append is re-judged by position (A-658's judge_computed) in 2b, the records'
  OutOfDate being commit_body's guard; (2) Drive::pending exposes the waiting transaction read-only for the hook's view.
  Signature sent to main (for g234/A-656 and A-652 part 3). Run requests filed (11:30Z: macOS lib run owed, clippy,
  xtask, doc). amendments.tsv A-655 row updated (backup amendments.tsv.bak-g268-a655-2a). jupiter clones/targets deleted;
  branch agents/g268-a655-2 deleted from origin. Clone on jupiter: HTTP/1.1 + openssl backend (schannel resets).
  Found, not fixed: two svn_time_to_cstring writers (reads::format_time, dav::response::svn_time).
  LEFT (main starts it): part 2b over A-662's map once on main (Subversion arm holding a migration::Repository; check ->
  Increment from checked.changes()/fresh() -> RepositoryLogs::append with Head{green, revision.head} and the svn record
  in EntryParts' objects -> apply; Moved re-judged via A-658; the record's encoding with golden vectors; recovery
  replaying check+apply). Part 1's Studio leftovers deleted by main when the Studio returns. Worktree
  ~/slates-work/g268-a655-2 (at 9caa5055d, clean).
- 2026-11-19T08:05Z (A-655 part 2a, UI goal 93; relaunched fresh 07:26Z): readback of THE RULE sent to main. Main's
  answers: I own the one public commit step (client commits and imports); 2a = one `Revision` value in slates-svn's
  commit module (rows, revprops with svn:date, contents, lock entries) from a client's Work (date from the owner's clock)
  or one imported dump revision (date and rest from the dump); 2b (server, over A-662's per-repository map, g258) waits
  for main's word. Main's addition (A-661, g229 approved): Step::Hook(Due::StartCommit) at open-root after its revision
  check, Step::Hook(Due::PreCommit) at close-edit before the verdict, Drive::resume(Result<(), Vec<(u32, String)>>).
  BUILT on branch agents/g268-a655-2 (worktree ~/slates-work/g268-a655-2, from main 018d32ba2), pushed 6b61231c4, NOT
  YET RUN: the Mac Studio d1323898 has been OFFLINE since 07:27Z (main: ran out of memory). Code: commit/revision.rs
  (Revision, of_work, date/author, StepRefusal), migration/records.rs (Repository holds the LockTable; check -> Checked,
  apply, commit, apply_lock_entries, revision_of; impl drive::Green for Repository), index.rs (plan, commit_planned,
  Planned, IndexRefusal::OutOfDate E160028), lock.rs (check_entries, apply_all), drive.rs (Step::Hook, Due, resume,
  revprops validated at open-root via validate_prop split from validate_property, Work.head, svn:txn- dropped),
  import.rs through the step, From<StepRefusal> for MigrationRefusal. Tests: drive/tests.rs (Replay over one
  Repository; replay through the step + revprops/dates/locks checked; hooks pause/refuse; revprops at open-root; misuse;
  out-of-date at the step), revision/tests.rs (3), migration/tests/step.rs (6), index/tests.rs (1). Docs script:
  g268-scripts/a655-2a-docs.py (args: red words, green words, merged, passed, seconds, clock); red script:
  g268-scripts/a655-2a-red.py (M1..M10 or ALL). Sources fetched to ~/.cache/g268-svn-src-fetch (transaction.c,
  load-fs-vtable.c, fs-wrap.c, tree.c, svn_props.h at d434dc14). NEXT when the Studio answers: delete part 1's build
  dirs in ~/.cache there; red ALL (own target), then green (fresh clone, own target), cargo fmt, merge, land, send main
  the step's signature. 08:10Z: main stopped my Studio watcher (each wake burns the allowance); main watches the
  Studio and its message is the signal to run, carrying its answers to my two part-2b points (FSFS's re-merge loop;
  the pre-commit hook's read-only view of the drive's rows and revprops). Arm no watcher. Finding for main (2b): FSFS's commit_txn re-merges when the youngest moved (tree.c 2255-2310);
  with pre-commit before the verdict and the step in the verdict's turn nothing moves on one owner.
- 2026-11-19T07:25Z: A-655 part 1 LANDED on main 018d32ba2 (a1932c3ee..018d32ba2): crates/svn/src/commit/drive.rs
  gains the checks (validate_new_path E160005; verify_mergeinfo + validate_prop: E165002 non-regular, UTF-8/CR chains,
  svn:date via a port of svn_time_from_cstring's fast path + apr_time_exp_get; check_authz via A-606 PrincipalRules at
  commit.c's call sites, files' checked_write) and the entry point `commit(params, &Session, Start, out) ->
  Begun{Driving, Authenticate, Answered, Close}` (must_have_access; add_lock_tokens' write + username); Setup gains
  rules + revision_properties; Work gains revision_properties. Red 07:08Z (checks) and 07:15Z (stub entry point),
  green 07:12Z/07:16Z; merged f52466962 426/426 + fmt clean 07:18Z. Signature sent to main for A-656 (g234). Main's
  extra ask (07:1xZ): the commit step must be drivable by A-652 part 3's dump import, one revision at a time, author/
  date/log from the dump's revprops, never the clock; checks stay in the client's path. NEXT: part 2 in slates-server
  (RepositoryLogs::append, BranchChange::Update{green, work: Increment}, Moved -> out-of-date) + split epoch_seconds
  (17 `?` may trip cognitive_complexity). Docs script: g268-scripts/a655-1-docs.py.
- 2026-11-19T06:50Z: A-655 (UI goal 93) ASSIGNED by main (A-617 part 2 recorded 494d62425; design change approved:
  no Trial across commands, per A-645). Scope: (1) the commit's remaining checks in commit.c's order and words:
  svn_repos__validate_prop (fs-wrap.c 202-263; mergeinfo first via verify_mergeinfo 297-324, order 327-338),
  check_authz through A-606 (PrincipalRules::access / access_beneath; "Access denied" 220004/220001, commit.c 243-262
  and its call sites), svn_repos__validate_new_path (repos.c 2097 -> path.c svn_path_check_valid: E160005 "Invalid
  control character '0x%02x' in path '%s'", path escaped \%03o), authenticating for write before commit (serve.c
  must_have_access 807-850: trivial auth if lookup_access passes, else auth_request when anonymous and auth_access
  >= write and auth possible, else 170001 "Authorization failed"; authz with path NULL = access_anywhere);
  (2) the owner's commit step through A-635's RepositoryLogs::append (crates/server/src/repository/push.rs,
  pub(crate), BranchChange::Update{green, work: slates_merge Increment}); Moved answered as svnserve's out-of-date;
  (3) ONE entry point for g234's svn:// connection task (A-656) and its signature to main. PLAN: part 1 in slates-svn
  (checks + commit::exchange::begin entry point), failing tests first; part 2 in slates-server (the commit step).
  Codes: BAD_PROPERTY_VALUE 125005 ("Wrong or unexpected property value"), _EOL 125017, BAD_DATE 125003 ("Bogus
  date"), MERGEINFO_PARSE_ERROR 200020, REPOS_BAD_ARGS 165002, AUTHZ_UNREADABLE 220001, UNWRITABLE 220004,
  RA_NOT_AUTHORIZED 170001 ("Authorization failed"), FS_PATH_SYNTAX 160005. Worktree ~/slates-work/g268-a617 (branch
  agents/g268-a617, at main 494d62425); Subversion sources in ~/.cache/g268-svn-src (d434dc14).
- 2026-11-19T06:40Z: A-617 (UI goal 93) part 2b LANDED on main 494d62425 (e50a8bea1..494d62425): the commit's
  drive, crates/svn/src/commit/drive.rs (Drive::command per item, begin, write_commit_info, Green seam, Setup,
  DriveRefusal with svnserve's numbers and words) + drive/tests.rs. Every check in commit.c's order and words, FSFS's
  lock check at each call (A-602 verdict over one path), svndiff windows applied as they arrive with MD5/SHA-1
  (aws-lc-rs), close-edit runs A-592's verdict, early-error rule (drop until abort-edit/success). DESIGN CHANGE: no
  A-592 Trial held across commands (it borrows the index while the owner must append other commits -- the recorded
  race); the drive's folded rows are read over the index at its base each command, and the verdict builds the trial
  once. Golden: the 17 recorded commits replayed in order (bob's lock via A-602's guard with the recorded token, the
  race inside alice's drive), every answer byte for byte; hostile table (18 cases); room, MD5, lock-at-call and
  beneath-copy tests. Red (06:17-06:19Z): 4 guards removed, each caught. Studio: 321/321 (99674c480), merged
  5d5a94bde 338/338, c715685db 365/365, landing tree 679cddaa5 371/371 in 8.05 s + fmt clean (06:37Z); the last merge
  brought only non-svn files. Docs script: g268-scripts/a617-2b-docs.py (args: merged, passed, seconds, clock). A
  reused merge message on 679cddaa5 says two conflicts where there was one (cosmetic, pushed). OWED (A-617's next
  part, main to assign): svn_repos__validate_prop, check_authz (A-606), svn_repos__validate_new_path, authenticating
  for write before commit, the owner's commit step (date, durable record) + the connection task running the drive.
- 2026-11-19T05:50Z: A-617 (UI goal 93) part 2a LANDED on main 1a13158ed (1f56d99ae..1a13158ed): the commit's
  request and its editor commands read as svnserve reads them (crates/svn/src/commit/edit.rs: EditCommand,
  CommitRequest, EditRefusal E210004/E210001, LockTokensRefusal; vparse_tuple's rules, stricter on a NUL in a C string
  and a revision past svn_revnum_t), golden: crates/svn/fixtures/commit (make-fixtures.py, 26 relayed connections of
  stock svn/svnserve 1.15.0 on the Studio, 17 commits, 4 refused; SHA256SUMS). Main's two clippy findings in
  commit/tests.rs fixed (Stopped::NotADirectory; is_multiple_of). Studio: red 9 tests (05:17Z), green eee90bd82
  245/245, merged 0234aaa50 315/315 in 8.82 s + fmt clean (05:42Z); later merge only main's A-642 design text.
  23 line citations corrected against d434dc14. Commits: a12441822 64ee0bfea e917310d8 (script), a55d5c814
  eee90bd82 (code), b48c1c358 (fmt), 8ef924a7b (citations), 2d06122af (docs), merges f96248bf0 0234aaa50 1a13158ed.
  Docs script for busy merges: g268-scripts/a617-2a-docs.py (args: merged commit, passed, seconds, clock, census).
  Run request filed (clippy/xtask/doc for the edit module). NEXT: part 2b, the drive (commit/drive.rs).
- 2026-11-19T04:52Z: A-617 (UI goal 93) part 1 LANDED on main 347c7cd25 (4c79dabb7..347c7cd25): A-602's
  LockTable::verdict answers the first failing lock in svn_path_compare_paths order (met/in_path_order/path_order in
  crates/svn/src/lock.rs); red first 05d28fd0c (A-592's scratch case committed in commit/tests.rs; A-602's oracle model
  now returns at the first failing check, census ordered_commits), fix ecf6307a7, fmt+docs fc2f37de4, GAPS repair
  7c4c9133b (a merge was committed with GAPS conflicted, fixed before any push), merges 0ae4eb8a3, b79787c23,
  347c7cd25. Studio: red 2/155 (04:39Z), green 155/155; landing tree 7c4c9133b green 236/236 in 2.27 s, fmt clean
  (04:48Z); later merge changed only the workspace manifest's slates-http line. Docs rebuild script for busy merges:
  ~/.local/state/slates-fanout/g268-scripts/a617-docs.py. NEXT: part 2, piece 7's commit editor drive.
- 2026-11-19T04:30Z: LANDED A-592 (UI goal 92, §4.17.4 piece 5) on main 99c72e534 (4a9d2bb12..99c72e534). ENDED.
  What: crates/svn/src/commit.rs (new): `check_out_of_date` (commit.c's drive test, E160028/E160006) and
  `verdict<Accepted>(index, base, work, locks: FnOnce(&[ChangedPath]) -> Result<Accepted, LockRefusal>)`: FSFS
  tree.c merge() stated on the index (only the rows of (B, head] and the nodes they reach; the work's identities by
  the index's own builder on B, `Index::trial`), then the caller's lock check -- the merge task passes g250's
  `LockTable::verdict` (A-602, landed 9e260b414 while I worked); ConflictKind x8; CommitRefusal {OutOfDate,
  RevisionPastBase, Conflict, Locked(LockRefusal), Index}. index.rs: `Index::trial`, `Trial`, `TrialNode`,
  `Builder::new_on`, copy source any committed revision (additive; the one way to get FSFS's txn identities without a
  second copy of get_copy_inheritance). index/tests.rs: `begin_on`, `into_tree`, oracle types pub(crate). lib.rs: mod
  line + doc sentence. Docs: §4.17.4 status A-592, rule 2's lazy copy, piece 5 built, amendment A-592; GAPS §4.17.4
  (title incl. A-605's mergeinfo, owed list fixed, A-602 order finding); research notes (5 sources + §6 notes).
  Evidence (Mac Studio d1323898, kiln): red 04:01Z (rule stubbed: 13 of 16 commit tests failed); 03:57Z oracle stack
  overflow (my oracle listed / as its own entry; fixed); red 04:18Z (lock check asked before the merge: 2 failed);
  green on the landing tree 04:25Z c20fa087e: fmt clean, `cargo test --locked -p slates-svn --lib` 126/126 in 1.52 s,
  census 4,096 cases: commits 1,702 (1,539 on head); PropertiesChanged 765, StructureChanged 427, DeletedTwice 59,
  DeletedAgainstChange 190, KindChanged 98, FileChanged 55, NotADirectModification 159, AddedTwice 267; E160034 110,
  E160037 72, E160039 192; lock beneath 85; skipped 392. Later merges touched no slates-svn input (checked by diff).
  Predecessor's own runs 03:20Z (logs on the Studio, deleted): its first oracle 54/54 green, a double delete met once.
  FOUND (owed to A-602/g250, in GAPS; confirmed by a scratch test on the Studio 04:25Z, never committed):
  LockTable::verdict verifies met locks in byte order of lock paths; FSFS checks changed paths in
  svn_path_compare_paths order -> locks at /t/a/b and /t/a-b: A-602 answers /t/a-b, FSFS /t/a/b.
  Sibling: lib.rs's crate doc does not mention A-605's `mergeinfo` (its owner's line).
  Cleanup: scratch branches agents/g268-a592-wip..wip10 deleted; Studio target (457 MB) + src (168 MB) + logs deleted.
  Worktrees: ~/slates-work/g268-a592 (branch agents/g268-a592 = main 99c72e534); the old clone
  ~/slates-work/g268-slates-delivers-everything keeps superseded A-592 wip commits on its old branch (nothing
  uncommitted); -p2 and -seam hold staged copies of pieces landed long ago (A-302/A-303, A-310: their files are on
  main), nothing unsaved.
  LEFT (main starts them): piece 7's drive calling check_out_of_date and the lock check per command over a
  transaction built a command at a time (an incremental Trial); A-602's lock order fix (g250's).
  A-555 (goal 91) stays PARKED as recorded at 04:00Z below: nothing uncommitted, no branch to park.
- 2026-11-19T04:20Z: A-592 (UI 92, §4.17.4 piece 5) begun. Read: §4.17.4 items 3-4; FSFS tree.c at d434dc14
  (fetched to ~/.cache/g268-svn-src/tree.c, sha256 5f4e47d132ef9e02): merge() 1790-2135 is merge(target=TXN,
  source=HEAD, ancestor=BASE) (svn_fs_fs__commit_txn -> merge_changes 2186/2284); compare_dir_structure 1723 compares
  names + node ids + copy ids; prop equality is content (prop_rep_equal). PLAN: (1) crates/svn/src/commit.rs: pub fn
  verdict(index, base, work: &RevisionRecord) -> Result<(), CommitRefusal{path, kind}> mirroring merge()'s order
  (root down; per dir: head==base node -> ok; head props changed -> DirectoryPropertiesChanged; work changed props &&
  head's structure (names/node/copy ids) changed -> StructureChanged; ancestor entries head changed, sorted: both
  changed -> null => DeletedTwice/DeletedAgainstChange, any file => FileChanged, node/copy id differs =>
  NotADirectModification (txn copy ids by FSFS get_copy_inheritance), else recurse; then head's additions the work
  also has -> AddedTwice); E160024. (2) oracle in index/tests.rs's naive FSFS (NaiveFs, Txn; add begin_at(B) and a
  txn-tree getter; histories()/rows_from for the work): transcribed merge() over the three trees; proptest via
  test-seeds; rule == oracle (path + kind). (3) failing first: verdict stub accepting everything -> red.
- 2026-11-19T04:00Z: A-555 PARKED on main's order (Ada: goal 92 first). Landed: part 1 63c83464e (parking, NFS,
  BaseHost), part 2 b822f00f3 (fetch driver), part 3 2a63efc15 (FUSE parking, Linux, unverified: run-requests 'A-555
  third piece'). Nothing uncommitted; no branch to park. LEFT (A-555's create, not started), planned as three landings:
  (3a) catalog BaseRecord::S3 {endpoint, region, bucket, prefix, style} appended (db wire_order publish) and its 35 match
  sites in 13 files (recovery refuses an S3 volume typed until 3c); admission with an S3 base: S3Host (chunk from the
  store, cache from the shard's admittable room, derived), the overlay over its root, the slot's BaseHost::S3,
  base_wait::spawn_driver over a Store; a lib test on the audit daemon with a SimS3 store held open (the first end to
  end of parts 1+2: Parked, ShardCell, land). (3b) the wire: RequestBody::CreateOverS3 {name, size, names,
  require_locked, base, credential} appended (ipc wire_order publish), the daemon resolving the endpoint (crate::dns)
  and building the network Client (S3Base::endpoint; TLS authorities: A-471's trust-roots question), slates-api
  Instance::create_over_s3 + contract bump, the client's method. (3c) the credential's rotation verb and the anchor's
  hold across a restart (anchor layout), recovery re-spawning the driver. Studio target + src deleted (6.1 GB).
- 2026-11-19T04:00Z: NEXT (main's order): A-592, §4.17.4 Subversion piece 5, the commit rule as a pure function over
  the Subversion index's rows (A-553) with an FSFS merge() transcription as its oracle; don't touch the merge engine.
- 2026-11-19T03:50Z: LANDED A-555 third piece (FUSE parking, Linux-only) on main 2a63efc15 (change 3c3efe4f0, merge
  2a63efc15): a FUSE request answered EAGAIN after its mount's lens noted a pending fetch is kept (fuse.rs keep): a task
  waits on base_wait::wait (failover delay from the first wait, the bound) and hands it back (settle_kept); the next
  request turn dispatches it before the device (channel::dispatch_kept via the shared dispatch_admitted). macOS half
  green r389 (server base_wait 4, bridge-fuse 1, fmt); the Linux code + its unit test are NOT compiled by me -> filed
  (run-requests 'A-555 third piece'). Not marked met in the UI until main's Linux build passes. NEXT: piece 3 of main's
  list, the wire's create + credential hand-off.
- 2026-11-19T03:20Z: LANDED A-555 second piece (the fetch driver) on main b822f00f3 (change f5d7c2b8a; merges
  6b4115402, b822f00f3): slates_base::s3::driver::drive runs a host's fetches one at a time against a Store (network
  Client or SimS3) and hands each answer to its HostCell (server: base_wait::ShardCell -> base_wait::land); an idle
  driver waits on S3Host::next_fetch_or_wait (a miss's want() wakes it); the host's Drop wakes it and it ends (detached,
  lives as long as its host); spawn_driver waits for the create (allow(dead_code) on it, ShardCell, land until then).
  Evidence (Studio): r356 red (miss woke 0, drop woke 0) then green base s3 58/58, server base_wait 4/4, fmt; r364 green
  on the merged tree. Wip branches deleted. NEXT: FUSE parking (Linux-only), then the create + credential.
- 2026-11-19 ~02:50Z: main's next piece for g268 = A-555's rest, in order: (1) the fetch driver calling
  base_wait::land (no parked request wakes in production without it); (2) FUSE parking (Linux-only, filed for main's
  build); (3) the wire's create + credential hand-off. Land after each if it grows; failing --lib tests first.
- 2026-11-19T02:45Z: LANDED A-555 part 1 on main 63c83464e (change 1e0532bb4 + 17c19aa91 + fmt 0174c605c; merges
  de0a34a01, b98600c2e, 63c83464e): a request that meets a pending S3 base names its fetch and waits for that fetch
  alone. HostError::Pending(FetchId) / VfsError::BasePending(FetchId); S3Host names one fetch per listing read and per
  chunk run, landed() reports the settled fetch, hold_for_waiters/release_held keep a landed fetch's items unshed until
  its waiters ran (GAPS A-464's owed hold, closed), unkept counted; slates_base::BaseHost {Disk, S3} is the slot's host
  (landing takes the disk host only, take_disk_host); server base_wait.rs (BaseWaits within the client credit,
  base.wait.{parked,landed,full,overdue}, Lens at the export, Parked future, land() for the driver, #[expect(dead_code)]
  until the driver calls it); NFS serve_local_fetched for local and forwarded calls; FUSE EAGAIN / NFS JUKEBOX for a
  request that cannot wait. Main's clippy findings on A-524 folded in (listed_bytes, settle/read_whole, test splits,
  Tree alias, SimS3::page, time::unix_of/ns_of_iso8601). Evidence (Studio d1323898): r128 pre-merge red (FUSE EIO, NFS
  SERVERFAULT, host hold) + green; r241 merged red (no wake (0,0,0), no bound Parked(1), no release 2 unkept, held shed
  0 unkept) + green; r294 green on b98600c2e (server 6, base s3 55, vfs base:: 7, land 2, fuse 1, nfs 1, fmt); r300
  green on 63c83464e (server 6, fuse 1, fmt). kiln was disconnected ~02:05-02:27Z (held the landing until it returned).
  UI: piece ce3e002e marked met; part 01a1f153 (A-555) stays active for the rest. Run requests filed (clippy x3
  platforms, Linux/Windows compile of the slot host type, landing/NFS suites, the end-to-end parking run once a volume
  over S3 can be made). Wip branches agents/g268-a555-wip..wip7 deleted; Studio target (7.6 GB) + src deleted. ENDED.
  LEFT (A-555 part 2/3, main starts them): FUSE parking (Linux: held off the device, dispatched again when its fetch
  lands); the wire's create naming an S3 base + the credential's hand-off (owner shard + anchor) + the fetch driver
  calling base_wait::land; wake a destroyed volume's waits; WinFsp/Shards/virtio-fs/typed-channel waits.
- 2026-11-19 (eighth g268 agent, restarted after a usage limit): read the preamble, origin/main CLAUDE.md (326 lines, §8),
  progress, inbox. Predecessor landed nothing of A-555 (main a613562eb). Piece: A-555 (UI goal 90): the server parks a
  FUSE/NFS request on BasePending until its fetch lands (one fetch, many waiters, the shard's waiter budget, typed
  refusal at the bound); then, if it fits, the wire's create + credential hand-off. Clone ~/slates-work/g268-slates-
  delivers-everything at a613562eb.
  PLAN (part 1, this landing): HostError::Pending(FetchId) / VfsError::BasePending(FetchId); S3Host names one FetchId per
  listing read / chunk run, landed() reports the settled fetch, hold()/release() keep a landed fetch's items unshed
  until its parked requests ran (GAPS A-464's owed hold); slates_base::BaseHost {Disk, S3} as the slot's host; server
  base_wait.rs (BaseWaits bounded by the client credit, typed refusal + counts, a recording Lens at the lend point,
  Parked future, land() for the driver); NFS serve_local_held + serve_remote park a JUKEBOX call that met a pending
  base; bridges map BasePending -> FUSE EAGAIN / NFS JUKEBOX. NOT in part 1: FUSE parking (Linux-only re-dispatch,
  cannot compile here) and the wire create + credential hand-off + fetch driver (catalog BaseRecord::S3, request
  variant, anchor hold): both filed as next pieces.
- 2026-11-19 (A-555 in progress): main's clippy findings on A-524 folded in (host.rs listed_bytes saturating; test
  helpers settle/read_whole panic- and unwrap-free; two long host tests split; Tree alias; SimS3::page, time::unix_of,
  time::ns_of_iso8601 split into steps). Studio r128 (scratch 76e6b5f): RED as intended (FUSE EIO, NFS SERVERFAULT,
  host hold test with held items shed); GREEN base s3 / vfs base:: 3 / land engine 2 / fuse 1 / nfs 1 / fmt; server lib
  test build failed on one missed alias site (error.rs `H::Pending`), fixed. Scratch c8e61e47e on
  agents/g268-a555-wip2 waits for the server's red+green run: kiln disconnected ~now ("not connected to the kiln
  server"); retrying. Scratch branches to delete after landing: agents/g268-a555-wip, agents/g268-a555-wip2.
- 2026-11-19T01:40Z: LANDED A-524 (item 7 part 3) on main c8731b725 (bd40ab1d0 + merges dac6f078e, c8731b725): the
  S3 base's host (crates/base/src/s3/host.rs: S3Host implements HostFs sans-io; a miss answers HostError::Pending and
  names its fetch, one per need; a directory is its delimited listing serving one learned round trip, relisted after
  it while it answers; a file bound to its object by If-Match + the version the store names; chunks + listings cached
  within the caller's budget, LRU shed), SimS3 (sim.rs) its oracle, S3Base (name.rs) the library's naming exported by
  slates-api (contract 46, after main's 43-45), fetch.rs, time.rs. Seam: HostError::Pending, VfsError::BasePending,
  mapped in vfs, server (Unpublished; target/landing answers errno 0) and land. Evidence (Mac Studio): base s3 49/49
  (oracle 256 cases), api 2/2, server error 3/3, vfs base:: 3, land engine 2; on the merged tree dac6f078e again 49/49 +
  2/2; failing first 4/4 on adas-mac-studio-3 (d1323898 offline then). Merges: main's A-527 Win32 HostError arms kept
  beside Pending. Filed: lint, Linux/Windows, the full lib suites, the live store run. Sibling (main's): server
  warnings deadlines.rs:16 / reap.rs:35, shards_route.rs not rustfmt-clean. Process slip: one `git reset -q` (index
  only, after a scratch write-tree; nothing lost) -- not to be repeated. Wip branches s3h-s3j deleted; Studio target
  (9.3 GB) + src deleted. ENDED.
- 2026-11-19T00:12Z: LANDED A-471 (item 7 part 2) on main 24c90fac3 (46cea08ca + merges 39fdf37b3, c0c8d3dd5,
  ff0c12ea7, 24c90fac3): the S3 base's network client in crates/base/src/s3 (client, http, tls, wait; sigv4's
  amz_timestamp). Caps: request head 17,585 B, response head 12,288 B, error body 74,436 B; waits RFC 9002 estimator +
  liveness window, doubled per timeout to the ceiling, retries full-jitter. Also fixed main's r413 findings in this
  goal's files: prop_oneof -> tuple strategies (archive stream.rs, vfs export.rs), api export/import tests split into
  judging helpers, list.rs literals named/marked and its test slice by get. Evidence (Mac Studio, debug): r447 s3 30/30,
  archive 7/7, vfs export 8/8, api 2/2; the same on merged trees 39fdf37b3 and c0c8d3dd5; red 4/4 (412 untyped,
  If-Match not sent, no back-off, length past cap). Before main's no-compiling order (2026-11-19 ~00:05Z): clippy of
  the touched crates clean, xtask none in A-471's files, xwin check x86_64 clean. Linux check NOT run (adacachy queue;
  stopped on the order): filed in run-requests.md with the cross-lint and the live store run. Sibling found, not fixed
  (not mine): crates/anchor/src/handles.rs:69 WHAT_SHARDS_DAX's Format: line is past xtask's 4-line reach (880f9130e).
  Wip branches s3c-s3f deleted; Studio target and src deleted. ENDED.
- 2026-11-18T23:59Z: LANDED A-464 (item 7 part 1) on main b3194636f (125d07953 + merge b3194636f): §4.15 "An S3 base"
  (sources: S3 API reference ListObjectsV2/GetObject/HeadObject/Object, object-key and consistency guides, the S3 SigV4
  page and IAM's SigV4 reference, all fetched 2026-11-18) + crates/base/src/s3 {sigv4, list, keys} (aws-lc-rs added to
  slates-base; proptest + test-seeds dev-deps). Evidence (Studio, debug): r389 14/14 `cargo test -p slates-base --lib --
  s3`; red once each (M1 `$` unreserved -> PUT golden vector red; M2 `<!` passed over -> hostile test red; M3 `..` a name
  -> escape test red). Run requests filed (23:59Z: base lib on Linux/Windows, cross-target lint of base with aws-lc-rs,
  clippy/doc gate). Mantle told. Wip branches + Studio target (920 MB) + src deleted. ENDED.
  NEXT (main starts them, each its own piece): A-464 part 2 (network client), part 3 (HostFs + SimS3), the import
  decoder's streaming read, the manifest's streamed encoding.
- 2026-11-18T23:30Z: LANDED A-459 (item 12) on main aedce71d0 (a fast-forward): the export walks the snapshot twice --
  the first walk measures each chunk and drops its bytes (SnapshotArchiver::advance_into into slates_archive::Measure),
  the archive is laid out (Layout) and its volume admitted exactly; the second walk's chunks, each checked against the
  first, go to a StreamWriter that stages records, manifest, seek table and trailer with the hash folded as it goes,
  emptied into the file at a copy slice as StreamWriter::next decides, within StreamWriter::bound (counted
  image.archive_stage_flushes / image.archive_stage_past_bound). Rejected (on record in the amendment): one walk with
  a patched header and a read-back hash (the admission would have to grow a bounded reservation and a locked credit).
  Evidence (Studio, debug): r349 + r355 -- archive lib 7/7 (oracle on generated archives, bound, a differing second
  walk refused), vfs export 8/8 (48 generated trees under measured codecs equal Archive::encode; an archive past twice
  the bound streams within it), api export/import/hostile 3/3; red once each (r355: header outside the hash -> both
  oracles red; stage written only at the end -> both bound tests red). Docs: §4.11 status, A-459 entry, GAPS (A-441's
  gap closed). Run requests re-filed (23:30Z: api suite, fleet takeover, archive/vfs whole, A-436's Linux runs,
  clippy/doc gate). Wip branches + Studio target (4.6 GB) + src deleted. ENDED.
  NEXT (main starts it): item 7, an overlay's base read from S3 objects (§4.15 design first).
- 2026-11-18T22:50Z: item 12 (A-459) begun: export's archive encoded as a stream.
- 2026-11-18T22:30Z: LANDED A-453 (item 6, import half) on main 2e41e8028 (c17716d94 + merge 2e41e8028):
  Instance::import_archive (contract 40): RequestBody::ArchiveImport / ReplyBody::ArchiveImported /
  Refusal::ArchiveRefused{fault: ArchiveFault} (appended, published in wire_order.txt); slates_archive::Decoding (the
  decode a piece at a time; Archive::decode = its pieces run to the end); server image.rs ImportJob (length admitted vs
  the archive volume's class cap and the shard's room before a byte is read, read a copy slice a unit, decoded,
  admitted under the archive's name policy, Planning/Restoring within verbs::restore_budget (shared with the takeover),
  laid in with verbs::Populating, record+publish+completion one atom, shed in slices); vfs
  export::name_equivalence_of_id; client archive_import; api Invalid::Archive. Evidence (Studio, debug): r310 units
  (archive 22, vfs export 6, wire_order 2, api lib 64) + the round trip, hostile and export tests; r318 with the
  symlink; r320 red once each (laying skipped; foreign magic classed corrupt). Mantle told. Wip branch + Studio target
  (4.6 GB) + src deleted. ENDED (one piece, then end).
- 2026-11-18T21:22Z: LANDED A-441 (item 6, export half) on main f5e9191c4 (a0b569ca7 + merge f5e9191c4):
  Instance::export_archive (contract 38): RequestBody::ArchiveExport / ReplyBody::ArchiveExported (appended, published
  in wire_order.txt), ARCHIVE_FILE; server image.rs ArchiveJob (pin, SnapshotArchiver in slices, encode, admit a volume
  sized to it, write a chunk a unit, record+publish as one atom, unpin); client archive_export; api re-exports.
  Evidence (Studio 21:18Z): fmt, wire_order 2/2, contract_history, the round trip (308,524-byte archive decoded and
  restored: every file's bytes/len/mode, dirs, no post-snapshot write, same manifest twice, NotFound/AlreadyExists) in
  0.80 s; red once with the file's writing removed. Mantle told. Wip branch + Studio target (3.5 GB) deleted. ENDED.
- 2026-11-18T21:05Z: LANDED A-436 on main 05f257e2d (076d6b5e1 + merges e7a44b5e2, 05f257e2d): a full write log's forced
  publication settles the write by its own volume (write_log::keep_record + log_acknowledged_write, shared by FUSE and
  the stream; FORGETs likewise): captured -> kept; left out -> appended again into A-422's freed room; only a write
  nothing keeps marks the overflow. Evidence (Studio): 16/16 focused lib tests (21:00Z); red once with the old rule
  (both new tests failed at the overflow mark). fuse.rs is Linux-only: its compile + kill tests filed in run-requests.
  Wip branch deleted; Studio target (3.0 GB) deleted. ENDED.
- 2026-11-18T20:56Z: LANDED A-430 on main bd2c385d0 (2936f0a98 + merge bd2c385d0): a checkpoint that cannot image a
  volume carries its committed record (checkpoint_log::committed_volume, one volume at a time, oracle-tested against
  Journal::recover); a publication that keeps a left-out record keeps every deferred block (Buddy/ChunkArena::
  capture_deferred), relief withheld meanwhile (ShardState::deferred_kept). FOUND while fixing: the delta path's kept
  record named blocks the first commit released (silent corruption or a refused shard at recovery) -- fixed by the
  same keep. Evidence (Studio): mem 4/4, vfs oracle 1/1, server 18/18 on the merged tree (20:54Z); red once per half
  (no carry; 524,288 deferred bytes released). Owed (GAPS): the precise kept set (only the kept records' blocks).
  Wip branches deleted; Studio target (5.0 GB) deleted. ENDED.
- 2026-11-18T20:33Z: LANDED A-429 on main 645211744 (13bc7dfe8 + merge 645211744): verbs::replay_one writes an
  overlay's logged write through Volume::with_host and the slot's host (copy-up first, as the bridge did); before, a base
  file the recovered image held uncopied had its window's bytes before the write read as zeros. Evidence (Mac Studio):
  13 focused lib tests green (20:29Z), the new test red once with the fix removed (byte 0 differed). Bug record
  docs/bugs/2026-11-18-a-replayed-write-into-an-overlays-base-file-zeroed-the-bytes-before-it.md; GAPS (A-422's
  sibling 2 marked fixed); design §4.6 status + A-429 entry. Wip branch deleted; Studio target (2.9 GB) deleted. ENDED.
- 2026-11-18T20:26Z: LANDED A-422 on main da3a13add (b57c3b142 the fix + docs; ab8933180 + 29617e378 the fleet
  test's Windows gates; merges dcfc65553, da3a13add). A publication that leaves a volume out keeps exactly its records
  under the new stamp (WriteLog::retain) and owes that restamp from before its commit (RESTAMP_OWED bit); Windows dead
  code gated cfg(unix) (write_log FORGET_UNLOGGED + log_forgets, state writes_lost) and the fleet test's MAP_STORE +
  library_shard_count gated linux/macos. Evidence (Mac Studio): 14 focused lib tests green on da3a13add (20:23Z); red
  once with the fix removed (20:16Z, `left: []`); `cargo xwin check -p slates-server --all-targets` (x86_64 msvc)
  0 warnings (20:22Z). Wip branches deleted; Studio target (6.5 GB) + src deleted. ENDED.
- 2026-11-18T19:55Z: LANDED A-410 on main 3ab8b9528 (f756f7a62 + merges bfff518ea, 3ab8b9528). ENDED on the
  coordinator's order (Ada, final: implement, unit-test, merge, END; main starts the next item).
  FOUND, not fixed then (reported to main): (a) main's e418f5cd2 put `chain_has_room` between `gather`'s doc comment and
  `gather` in crates/bridge-virtiofs/src/device.rs; (b) the write log after a publication that skips a volume (fixed
  since as A-422); (c) candidate: the device's FLUSH could skip its barrier while the log holds every write (A-399 for
  streams, `flush_logged` for FUSE): a perf change, A/B owed.
- 2026-11-18T19:50Z (seventh g268 agent, relaunched after the accidental stop): read the preamble, the brief, CLAUDE.md
  (origin/main, 329 lines, §8 THE RULE), progress, inbox (g234 holds item 4). Found the predecessor's A-410 staged in the
  main clone; red-checked once on the Studio (M1 the device's bridge given no write log -> RED, `left: []`).
