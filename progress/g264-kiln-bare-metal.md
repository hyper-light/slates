# g264-kiln-bare-metal — progress (REWRITTEN 2026-11-18 ~22:35 UTC after the ~/.cache wipe; owner #5)

Goal (kiln 01a1ec34599974448aed2672bafaf9a2, UI goal 88's bare-metal part): kiln's bare-metal engine runs every
harness and agent on a slates volume through the slates library on macOS, Linux and Windows (WinFsp), with review,
drift and landing by permission mode, proven by tests that start agents. Role: implementer subagent of the slates
supervisor. THE RULE: one piece, focused --lib unit tests on the Mac Studio (d1323898), merge, push, END.
Windows leg: g085 (seam agreed in its inbox 19:55Z/20:55Z: its #[cfg(windows)] arms of workspace/{place,slates}.rs,
handoff's Claude project dir for K:\, Windows runs; mine the engine + macOS/Linux).

## ITEMS
- [x] I1 default workspace = a volume for every start naming none — kiln 17a2c94a7
- [x] I2 a child in its parent's folder shares its parent's volume — 17a2c94a7
- [x] I3 through the slates library alone (no CLI), kiln's own exe as anchor/daemon — g263 swap + dc64e4db1
- [x] I4 Windows WinFsp drive letters (place.rs) — 17a2c94a7 (runs: g085)
- [x] I5 review + drift — 17a2c94a7
- [x] I6 landing by permission mode, no self-raise — 17a2c94a7, 33fdfc63f, a2cc1e429
- [x] I7 can't-mount machine refuses typed — a2cc1e429
- [x] I8 macOS unmount at session end — a2cc1e429; mount named by real path + table-judged removal — 68eda5b14
- [x] I9 anchor wait while it lives (slates A-366) — dc64e4db1
- [x] I10 bare-metal test can_write everywhere — 006ff1773
- [x] I11 shell agents on a volume (cwd, PWD, environment re-homed) — 9a8d8742d (cb0b17555 + ce46bf661)
- [x] I12 resumed conversations on a volume, harness record re-homed (Claude transcript + session dir, Gemini chat +
      subagents dir; Codex/ACP by id + cwd; branch = copy) — kiln main 835ceeb5d (1a110feeb red, 48c35ea6e fix,
      835ceeb5d merge of main aba723dc5; ledger conflict in row W resolved: main's fleet-move text + my Owed (W2))
- [x] I13a step35 gaps.json — 40161397b; [x] I13b w26_r18 (mail pump + fake Claude consumption) — 40161397b
- [x] I13c w26_ac10 — kiln main b74da5318 (dd195dbd0 fix + docs): a DIAG run on main (r348) showed keep() was never
      called: follow::deliver judged a message by Sent::went, which a W20-published message never is, so every message
      read as lost (no hand-off kept; the conflict ladder's "couldn't reach" too). deliver now uses
      Sent::kept_for_person; keep() returns typed NotKept, kept() records refusals in the audit trail (ship.hand_off,
      refused). Studio r366: workspace::follow + ci::ladder 10/10 (ac10 green; new w26_r18 keep test).
- [x] I14 bare-metal half — kiln main b043c2833 (84490e54a red, b043c2833 fix): K5 (g267, kiln 1ec78034d) already makes a
      fleet runner's every workspace (bare metal too, via make/provision) a captured clone that moves; the bare-metal gap
      was the arrival: follow_to re-homes the I11 environment from the mount it left to the new mount (cwd already
      followed). Captured clones stay fleet-only by principle (a move needs the fleet; a capture elsewhere costs RAM for
      nothing), stated in W3. Studio r25: RED w7 test, GREEN 46/46. Filed the fleet move e2e (run-requests 00:10Z).
- Drift landing fix (conflicted outcome) — e30bdc96a. Linux bare_metal:: 2/2 at e30bdc96a (main's r194).

## Owed / ledger (kiln docs/agentic.md row W, in I12's commit)
- a shell inside another agent's mount: laid over the mount (Slates refuses, A-227) instead of sharing its volume.
- [done ae9ae2509] a conversation record left in the gone mount's project folder: now sent back to the person's folder at the agent's end.
- Claude branch copy (rehome_branch) can leave a stale copy that transcript::find may pick first (read_dir order).

## Runs (main's)
- macOS bare_metal:: + bare_metal_tests on the mount fix 68eda5b14: main's r284 in flight.
- re-filed 22:45Z (run-requests.md): the resume e2e (I12), the shell-agent e2e (I11), the fake-Claude suites.

## Unit-test evidence (Studio d1323898, logs ~/.cache/g264-logs/* THERE)
- I11: RED kinds PWD test + ops end_to_end w2 (shell); GREEN 37/37 + w2 1/1; UI prettier/typecheck/spawn-setup ok.
- I12 (r330): RED i12 claude + gemini + w2 (resume: "machine old is not connected"); GREEN 42/42.
- I12 merged tree (r344): 42/42. Studio build output removed at END.
- NEXT: I14 bare-metal half after g267's K5 (A-463) lands the shared capture+clone path (g267 leaves a note).
- 23:20 UTC LANDED kiln main ae9ae2509 (b74da5318..ae9ae2509): 754033554 failing tests (homes w2 claude/gemini back,
  refused leaves both), ae9ae2509 fix: Host::end sends the ending agent's conversation (state.harness_session) home
  before its mount goes (workspace::send_conversation_home -> homes::return_home; typed homes::NotMoved
  InBoth/Refused/Unnamed, logged; both records kept on refusal); I12's rehome/rehome_branch share move_claude/
  move_gemini. Studio r384: RED 3, GREEN 94/94. Docs W2, agents.md, ledger row W (owed item removed). Filed the
  resume-after-end e2e (run-requests.md 23:20Z).
- NEXT (main's ask, inbox): macOS r284 still red — the Ask volume destroyed with its mount in place, no removal log
  lines in the e2e output. Then: the shell-inside-another-agent's-mount item; I14 after g267's K5.
- 23:45 UTC LANDED kiln main f54571bdf (1ec78034d..f54571bdf): 43e601c20 failing test (take_down over TakeDown: never
  destroy while the table holds the point), 6e1d95f32 fix (MountStays typed, volume kept; unmount_patiently believes an
  unmount only when the table agrees; /sbin/mount; e2e runner PATH gets /usr/sbin + /sbin; e2e prints the runner's
  removal lines), 3240a6563 + 09f618e07 test fixes, 6ff8598a4 merge (K5), f54571bdf (K5 base release named from the
  volume). Root cause (code reading): the drive's runner PATH held only /usr/bin and /bin links, so `mount`, `umount`,
  `umount -f` (in /sbin) weren't found and remove() destroyed the volume anyway. Studio: RED "refuses true, lies false:
  destroyed under a mount the table holds"; GREEN slates+place 31/31, merged tree 36/36, e2e checks 17/17. Filed the
  macOS rerun (23:45Z). Owed by Slates (ledger): slates_api::mount::unmount runs `umount` by name.
- NEXT: I14 after g267's K5 note; the shell-in-another-agent's-mount item moved to g262.
- NEXT (its own piece, per the coordinator): g267's sibling: slates::excludes(m) for an unmounted (isolated) workspace
  runs `git -C ""` (place::top("") is empty), so git reads the runner's own cwd and a landing's excludes are whatever the
  runner's folder ignores. Failing test first.
- 00:55 UTC LANDED kiln main 907d448bf (7e5ccf240..907d448bf): ccc6d0e8d failing test (ignores_from), e215dc83e fix: an
  unmounted workspace's ignores are read from its volume (review::ignored_paths: shallowest ignored dir else the file),
  writable takes the plan's excludes, slates_served expects /.git /secret.env /target left out and writable Ok, docs W8;
  merge 907d448bf (review.rs conflict with main's chmod work resolved by hand; not rebuilt per Ada's order — filed).
  Studio r77: RED w8 (Tree("")), GREEN 47/47, slates_served 1/1.
- RULE NOW (Ada via main, 2026-11-19): the only compiling I do is the --lib unit tests of my piece; no clippy, cross
  checks, doc builds or extra builds; merge as soon as the piece's --lib tests pass, then END.

## 2026-11-19 (restart after the usage limit) — UI goal 88 piece: kiln's unmount waits on the agent's exit, never a timed retry
- Branch agents/g264-kiln-bare-metal-unmount-event (worktree ~/slates-work/g264-kiln-bare-metal-kiln-ue).
- Predecessor (before the limit): f5e028697 red (Studio: 32 passed, 2 failed — take-down kinds; "over while its harness
  still ran"), 92f10873c fix (Studio: 34/34): Host::end awaits processes_ended (harness + group, each exit by
  os::watch::exited: pidfd/kqueue/handle) after close_wait; take_down asks the unmount ONCE; unmount_patiently (its
  crate::rt::sleep, red in kiln's clippy) and the forced let-go are gone; MountStays refused by kind (Held). Then merged
  main 449a9288e (7f2c6d1e1), never built.
- Me: the merge brought contract 42 (70e3dcaed), whose typed MountFault::Busy / StillMounted take_down ignored (kind by
  lsof alone). 4be490453 RED (seams typed: unmount_typed keeps Slates' refusal as Ok(Err(refused)); Held::Busy carries
  lsof's whole answer): Studio r39 32 passed, 1 failed — Busy+nothing lsof sees -> Refused, Busy+lsof can't run ->
  Refused, StillMounted -> Busy. ab85247cd FIX: StillMounted -> StillListed, Busy -> Busy naming lsof's holders, lsof
  decides only where the OS said no more than "failed" (Linux fusermount3); docs: Held, held_by, take_down, CONTRACT 42
  note (let_go gone), agentic.md W3. fe88031ba merges main ef2812dce (docs-only W9). Green run: Studio r56.
- LANDED kiln main fe88031ba (ef2812dce..fe88031ba): f5e028697 + 92f10873c (predecessor), 7f2c6d1e1 merge, 4be490453 red,
  ab85247cd fix, fe88031ba merge (docs-only main). Studio r56 GREEN: 33 passed (workspace::slates::tests +
  harness::host::tests::w3_). Studio cleaned: g264-kiln-ue-src, g264-kiln-ue-target (26G), slates-work-target-g264-kiln
  (25G) removed; logs kept at ~/.cache/g264-logs/ue42-{red,green}.txt there. Run requests filed (clippy x3, Windows/Linux
  lib tests, bare-metal e2e).
- Sibling findings, not fixed (outside the piece): (1) os::watch::exited pends forever on a pid it cannot watch
  (pidfd_open other than ESRCH: ENOSYS before Linux 5.3, EMFILE; kqueue refusal other than ESRCH; OpenProcess refused),
  so processes_ended would hold that agent's workspace release (its volume in the anchor's RAM) until the runner
  restarts; the "awaited" log line names the pids. (2) An agent process stuck uninterruptibly on the workspace's hard
  NFS mount (its daemon gone) also holds the release forever; the forced let-go used to free it, now nothing is cut off
  by design.
- DONE with this piece. Next (main assigns): I14 remainder / anything main sends.

## QUEUE (main, 2026-11-19), in order
1. [x] LANDED kiln main b7720ebca (fe88031ba..b7720ebca) UI goal 88 part 01a1f153aa82776dabb687f9c7c42b4d: kiln slates_e2e `bare_metal` is #[cfg(unix)] (slates_e2e.rs:1453), so
   W5's Windows step ran 0 of its tests: run it on Windows (WinFsp drives, kiln.exe), each platform difference in its doc;
   a test that can't hold on Windows gated alone and skipping LOUDLY with its reason. Compile what I can as kiln --lib; the
   Windows e2e run is main's (W5's workflow on kiln's CI). Worktree ~/slates-work/g264-kiln-bare-metal-kiln-w5win,
   branch agents/g264-kiln-bare-metal-w5-windows.
2. [x] LANDED kiln main 5ee60ecac (852f228e9..5ee60ecac; worktree ~/slates-work/g264-kiln-bare-metal-kiln-exited) os::watch::exited pends forever on a pid it can't watch: refuse by kind, so the workspace release isn't held.
3. [x] LANDED kiln main 9f1a41e55 (fd6f5fce8..9f1a41e55) An agent stuck on its mount after Slates' daemon has gone blocks cleanup forever: on the daemon's/anchor's own exit
   event (which kiln watches) alone, release the mount with Slates' unmount_busy (MNT_FORCE on macOS); never a timer.
- 1 done: bare_metal runs on cfg(any(unix, windows)); module doc states each difference (WinFsp drive judged by its file
  system name via kiln::workspace::place::is_mount — mount_point/slates_mounted_at; drive gone at the end; instance
  ends with its tree; stand-in via .cmd shims + vm_e2e::stub_python (now pub); loud SKIP without Python 3; drift on the
  watcherless Windows host by re-check). Stand-in now writes with newline="" (Python text mode wrote \r\n on Windows,
  which would fail every byte check). No test gated (none known not to hold). Built nothing (no --lib code; THE RULE):
  run request filed 2026-11-19 (W5 workflow at b7720ebca + Unix CI compile). NEXT: queue 2 on main's word.
- 2 state (02:15Z): b4a2f52bd RED pushed (exited -> Result<(), Vec<Unwatched>>, Unwatchable NotAPid/Unsupported/Refused;
  Ends seam in harness/host.rs with main's pend-forever kept; aisessions one_exited maps Err to never; tests:
  os::watch g9_a_wait_on_what_names_no_process_is_refused_at_once_by_kind, harness::host
  w3_an_ended_agents_process_the_os_cant_watch_holds_no_release, both by now_or_never, no clock). 16de3ef24 FIX committed
  LOCALLY, not pushed (pid validation before backends; macOS/Linux/Windows/other backends refuse by kind; processes_ended
  counts a refused pid as ended and logs; docs W3). Red run r147 lost: kiln server connection dropped ~02:05Z (MCP and
  CLI both "not connected to the kiln server"; kiln status answers). Studio leftovers to delete at END:
  ~/.cache/g264-kiln-exited-src, ~/.cache/g264-kiln-exited-target.
- 2 done: red b4a2f52bd (Studio r255: 29 passed, 2 failed), fix 16de3ef24, merges e5a740700 (green r315: 31 passed,
  1 ignored) and 5ee60ecac (main 852f228e9, none of my files); pushed 852f228e9..5ee60ecac. Studio build dirs deleted
  (17G + 618M); logs ~/.cache/g264-logs/exited-{red,green}.txt there. Run request filed (Linux/Windows lib tests + clippy).
  Found: macOS watched pid 0 (kernel_task) for ever (red r255). Sibling: unused import slates_rt::TaskId in vendored
  slates server/src/deadlines.rs:16. NEXT: queue 3 (daemon-death release via unmount_busy) on main's word.

## 2026-11-19 item 3 (UI goal 89): parked at its next commit per main (goal 92 takes priority)
- Branch agents/g264-kiln-bare-metal-daemon-gone (worktree ~/slates-work/g264-kiln-bare-metal-kiln-gone).
- 79aad6f9d RED (Gone seam; let_go_when_gone lets go of nothing at a daemon's end): Studio r383 31 passed, 1 failed
  (w3_a_gone_instances_mounts_are_let_go_of_on_its_daemons_end_alone: left Some(LetGo([]))).
- a53629f2e FIX + 9f1a41e55 merge of main fd6f5fce8 (pushed): ensure_up -> follow() (Unix, once per process): daemon pid
  from Slates' daemon_status; os::watch::exited on it; at its end the table's workspace mounts are listed, then Slates
  asked: restarts past the ended one's -> follow the next daemon (anchor keeps NFS listener + daemon fds, §4.6);
  unavailable or a new instance -> unmount_busy each listed mount; same daemon answering / untold end / unread table /
  other refusal -> nothing let go, said. Green run r422 in flight; land if green (main's rule).
- Design note for main: acted on the anchor's death as Slates tells it, not on a bare daemon exit, since a daemon its
  anchor restarts serves the same mounts again; forcing them then would cut off agents whose mounts were recovering.

## NEXT PIECE (main, 2026-11-19): A-593 — §4.17.4 Subversion piece 6 part 1: the dump-format codec in slates-svn
- 3 LANDED 9f1a41e55: green r422 32/32 (workspace::slates::tests); Studio dirs deleted; run request filed.

## 2026-11-19 A-593 (UI goal 92, §4.17.4 piece 6 part 1): the dump format codec in slates-svn — LANDED main 9b91fbcf1
- Worktree ~/slates-work/g264-a593-svn-dump, branch agents/g264-a593-svn-dump (pushed). Landed de0176391..9b91fbcf1.
- Commits: 5e32ff98a + 9585dc524 fixture script; 4024c73fe RED (API, stubs refusing everything, 25 tests: Studio 0 of
  25); cc2f72443 + ec3decbe7 reader/writer (+ the oracle's own fix: revision properties are no node's); f07b9e6ca green +
  docs (fmt applied from the Studio's diff); 3ddea7ef1 .gitattributes `-text` for the dumps; five merges of main
  (ebb02a83e, 475952115, 8ffeabb6f, 2966836d6, 9b91fbcf1), each a docs conflict at a document's end or GAPS' §4.17.4
  paragraph; evidence commits.
- Code: crates/svn/src/dump.rs (types, DumpError + Subversion numbers, shared checks), dump/{header,read,write,digest}.rs,
  tests in dump/tests.rs + dump/tests/{support,golden,hostile,writer,generated}.rs. MD5 and SHA-1 through aws-lc-rs (the
  vendored copy exposes MD5, so no md-5 crate). Reader: resumable over the caller's buffer, Admission{buffer_bytes} checked
  when each length is read; a deletion then an addition of its path folds to one Replace (FSFS's row). Writer: dump.c's
  header order and blank lines, delete-half for a replace by copy, every check before appending.
- Fixtures: crates/svn/fixtures/dump (full, deltas, part1, part2 dumps; changed.txt; svn-version.txt "svnadmin, version
  1.15.0 (r1937767)"; SHA256SUMS) made by make-fixtures.sh on the Studio, run twice to the same bytes; copied back via a
  kiln code_run (base64 of a bzip2 tar through env), SHA-256 checked both ends. 1.15.0's dump.c/hash.c = 1.14.x d434dc14.
- Evidence (Studio, --locked): red 0/25; green 78/78; merged trees 203, 260, 261/261 (2966836d6, 9.18 s); fmt --check
  clean. Logs ~/.cache/g264-logs/a593-* on the Studio; build dirs deleted.
- Docs: §4.17.4 status + piece 6, A-593 amendment, GAPS §4.17.4 (sentence + headline), research notes §12 (main's §11
  is mergeinfo).
- Run request filed (clippy x3, xtask check, cargo doc, cross-target check). Owed (main starts it): piece 6's verbs over
  the codec and the dump round trip.

## 2026-11-19 A-628 (UI goal 93, §4.17.4 piece 6's rest, rescoped by Ada): the server's import and export — LANDED main 9ba8b4580
- Scope (Ada via main): no svnadmin CLI, no svnadmin text; server verbs import(dump) and export(range) over the index and
  records, typed refusals; gate import(export(R)) = R and export(import(D)) = D for the golden dumps. The svnadmin-shaped
  work is PARKED unmerged on agents/g264-a628-svnadmin (812951aff; worktree ~/slates-work/g264-a628-svnadmin): stock
  1.15 verb outputs, redumps.txt (stock load+dump of every golden dump = full.dump).
- Worktree ~/slates-work/g264-a628-import, branch agents/g264-a628-import-export (pushed). Landed ef3fd18a0..9ba8b4580.
- Commits: dfc714085 RED (verbs stubbed to refuse; Studio 27 of 27 migration tests failed); 7156ebc0e GREEN; 53da91209
  A-593 lint-habit slips (as_chunks, byte string, no prop_oneof, no string slicing, is_multiple_of, no panic/unwrap in
  Result test fns); f4dc10b69 usize indexes in the generated test; ffc9fc60e rustfmt (Studio diff 4fbfb9b8, applied);
  c892ba001 docs; merges 1fbcc1e79, 1e663a9a5, 4bfb205e5, b83497e7f, 9ba8b4580 (docs conflicts at the amendment log's
  end, GAPS headline, research section numbers: mine is §15 after A-617's §14); f5d0e3951 + the b83497e7f-run docs.
- Code: crates/svn/src/migration.rs (MigrationRefusal + Subversion numbers), migration/{paths,properties,records,import,
  export}.rs, tests in migration/tests.rs + tests/{support,golden,rules,refusals,generated}.rs. blake3 dep (+ lock line).
  Records: Repository{uuid, Index, revprops, contents by BLAKE3, txn counter, Room{entries,bytes}, held}. Import mirrors
  load-fs-vtable.c + FSFS (fold_change; prop-mod iff a call; text-mod iff a text; mergeinfo flag semantic: value
  changed); keeps revision numbers (OutOfSequence), refuses a foreign UUID on a non-empty repo, keeps properties byte for
  byte (svnadmin load validates svn: props and rewrites svn:mergeinfo by default). Export = svn_repos_dump_fs4 replay
  (path driver, dump_node comparisons by content identity = FSFS rep sharing), format 2 only (DeltasOwed for 3).
- Evidence (Studio, --locked, debug): red 0/27 migration (dfc714085); green 313/313 (7156ebc0e, f4dc10b69); merged
  1fbcc1e79 357/358 — the 1 failure main's A-613 reads proptest
  `hostile_commands_are_answered_or_end_the_connection` (list `5:trunk ( ) true` streams < 4 items), reproduced ALONE on
  origin/main a5dffc913 from seed cc 8c15aaa60f4700e2c4cf9ad02498c39f24f70500057be3b37f4181c424a4d919 (reported to main);
  b83497e7f 359/359; fmt --check clean. Trap hit: RED and GREEN trees sharing one CARGO_TARGET_DIR reused the red
  binary (cargo's path-independent metadata + mtimes); touched sources and reran (memory: own target dir per arm).
- Found for A-553's owner: the index refuses Replace-by-copy of the node-revision it replaces (CopyOntoItself); FSFS
  records it when the deletion comes first (tree.c copy_helper 2602: the same-node no-op only while the destination
  holds it). The generated test avoids it.
- Owed (A-628 part 2): format-3 export = port text_delta.c's windowing + xdelta.c (sources kept in
  ~/slates-work/.g264-a628-src.adscBK) so deltas.dump/part2.dump re-export byte for byte; the wiring (records on the
  shard, texts in the object store, verbs on the control channel, sliced under the shard budget).
- Docs: §4.17.4 status + piece 6 restated, A-628 amendment (log end), GAPS §4.17.4 (headline + sentence), research
  §15. Studio scratch deleted (run dirs, target, A-628 sources clone, svnadmin-era files).

## 2026-11-19 A-652 (UI goal 93): index replace-by-self-copy, format-3 export + the deltifier, the verbs' wiring
- Worktree ~/slates-work/g264-a652, branch agents/g264-a652 (pushed). Disk: the MacBook hit 100% (557 MiB free) and the
  first worktree checkout failed; freed my own: the landed g264-a593-svn-dump and g264-a628-import worktrees and 17
  clean, merged kiln worktrees (g264-kiln-bare-metal-kiln-*; kept -swap, unmerged, and the kiln clone itself).
- Part 1 LANDED main 8b3297fe0..e2e7e7051: the index takes a Replace whose copy source is the node it replaces (FSFS
  copy_helper copies once the delete came first); IndexRefusal::CopyOntoItself removed. Red 12b582e68 + 676e0c1ee
  (own target dir): 5 of 367 failed, all CopyOntoItself (unit test, index oracle, A-592's verdict oracle via the shared
  generator, migration rule + generated); green 6423b148d 367/367; merged trees 398/398 (7f22bc7d8), 404/404 (5bc3710fe).
- Part 2 LANDED main 9200bb014..1570459e9: svndiff::deltas(source, target, window_room: NonZeroUsize) ->
  Result<Deltas<'_>, SvndiffError>, Deltas: Iterator<Item = Window> (text_delta.c windows + xdelta.c choice), the crate's
  one delta generator (signature sent to main for g234's A-616); format-3 export (Prop-delta incremental, text deltas
  svndiff0, base digests). Red a402eae77 (choice stubbed to new data): 4 of 413 failed; green 401f62382/909016345
  413/413; deltas.dump and part2.dump exported byte for byte; every recorded window remade; merged ed0b6bf0d 418/418.
- Docs landed 145a12542: A-652 entry (parts 1-2 + "Owed of A-652"), GAPS, research §15 deltifier sources
  (text_delta.c fc69ad86, xdelta.c 54b4ca6d, string.c 62b5ad96, cached_data.c 24e324df).
- Part 3 NOT STARTED, by design: an imported revision is a commit (version + record one durable step, item 3.5), so it
  must go through A-655's owner commit step (g268; its branch agents/g268-a655-wip1 is at the commit checks) and read the
  shard's Subversion state A-655/A-656 keep. Asked main to sequence it after A-655 or reassign the commit step.
- Merge helper: ~/slates-work/.g264-merge-main.py WT TAG MSG (fetch, merge, resolve a log-end-only conflict main-first,
  commit, diff stat); a continuation inside an entry needs ours-first by hand.
