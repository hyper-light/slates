# g234 -- progress (rewritten 2026-11-18 ~22:10Z from what the agent holds; the 14:04 PST ~/.cache wipe took the old file)

Role: Claude implementer subagent of the slates supervisor. Clone ~/slates-work/g234-windows-wake-estimate, branch
agents/g234-windows-stream-resync. Builds on the Mac Studio via kiln (runner d1323898b255a3ed435eb83fa0f7b0cf, sync false).
Rule (Ada's order via main, 2026-11-19, supersedes the 11-18 clippy/xtask addition): one piece; my only compiling is
its --lib unit tests plus cargo fmt; once they pass, merge at once (diff stat, then push HEAD:main alone) and END. No
clippy, xwin, Linux checks, xtask or doc gates: file them for main in run-requests.md. Stay ended until main sends
the next piece.

POLICY_READ (Shards, 2026-11-18 19:55Z): origin/main 4ff00b72a83ed398efb2747c803c2e606948c3be; docs/development-rules.md
sha256 df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3; AGENTS.md sha256
f66d304c033ad4001d3d7347b67faa3bb3137d637860793e8e74f1eb6266db41; docs/development-evidence.md sha256
61613a244106d95b2759b38c8b6660ff29a616983e7afd6e9faabdc38368fb63; role implementer, READ-ONLY on Shards.

## A-695: the svn:// repository lookup (main's next piece, ~23:30Z) -- IN PROGRESS
- Brief: the rest of find_repos; a repository another shard owns reached by moving the connection to its owner
  shard (vcs_ssh's keep_here/arrivals, as the HTTP edge and A-651 part 4); sources read per call. Failing test first:
  a two-shard daemon's svn:// session on a repository the other shard owns, as svnserve's recorded session. After it
  lands: un-ignore g307's svn:// leg in crates/cli/tests/svn_interop.rs if its other waits landed (A-685 has;
  CRAM-MD5 is A-692, g307's). Then the learned waits as their own piece. Report to main after each landing, go on.
- Don't chase the A-510 stop-test flake (main noted it).
- MAIN'S RULING: g297 owns the production SvnStores lender (store as reads::Repository + Contents, with the
  directory listing); don't build one. Do find_repos + the owner-shard move now; read each call's sources through
  g297's SvnStores once on main (test lender until then).
- PLAN: slice A = slates-svn find.rs (find_repos port: skip_scheme_part BadUrl, authority skipped, relpath
  canonical, uri decode, repos_path_valid BadPath, first segment = repository name (DAV's SVNParentPath rule), fs path,
  root URL = URL less fs path's components) + server vcs_svn.rs: locate the name's owner shard, move the connection
  there (vcs_http migrate/arrived/seat_arrival pattern; ListenerState on every shard via keep_here/HERE), open the
  Subversion repository with its mechanisms (no anonymous principal in 4.13; CRAM-MD5 keys = A-692) -> today svnserve's
  'No access allowed' (fixture handshake/no-access). Failing test: 2-shard in-process daemon, repository on the other
  shard, recorded no-access session. Slice B (later): per-call sources via g297's lender + A-692.
- DISK: the MacBook filled (403 MiB) and a merge in g234-vcs-svn failed half-written; removed all my old worktrees
  (now ~35 GiB free). Worktree now ~/slates-work/g234-svn-lookup, branch agents/g234-svn-lookup (origin/main
  574185afa). Main's clone ~/slates-work/g234-windows-wake-estimate stays (it holds .git). CHECK git diff --stat for
  unexpected deletions before every commit (main's rule after a design doc was truncated to 0 bytes).
- SLICE A written: c0d6b161f (test only, red; pushed also as agents/g234-svn-lookup-red) + cef1f7077 (svn find.rs +
  find/tests.rs, report.rs uri_decode_bytes, lib.rs; server vcs_svn.rs lookup/move/open, daemon.rs start(shards)).
  Studio run r477: red (server vcs_svn::) then ff to green: fmt, svn find:: report::tests::, server vcs_svn::
  daemon_edge::, cli vcs_svn svn_listeners. Docs (design status + entry A-695, GAPS, cli.md) after the run.

## A-689: the svn:// listener, `--vcs-svn ADDRESS`, held by the anchor -- LANDED ~23:25Z, ENDED
- [LANDED] main f7684fc20 (push f1fbd2c44..f7684fc20, fast-forward): ONE commit 9abe7e9a2 (squash of scratch
  agents/g234-vcs-svn on base 693bc8feb; tree == r344's tested tree, docs' evidence lines aside) + merges 8c9819826
  (2bfec1547: A-685 args.rs auto-merged, cli.rs rebuilt as main's + my block) and f7684fc20 (f1fbd2c44: A-680 part 2a
  daemon_edge auto-merged). Each landing tree == the scratch tree that ran (r373, r394).
- What: role::vcs_address (one reader, replaces vcs_http_address), ProcessOptions/StartSpec/DaemonConfig vcs_svn,
  CLI grammar, contract 54; server vcs_svn.rs (the predecessor's listener) + vcs_svn/tests.rs; daemon.rs
  held_or_bound_listener + listens_at (moved from daemon_edge, shared) + bind_vcs_svn_listener; anchor ENV_VCS_SVN +
  Supervisor::hold_vcs_svn_listener; api role/anchor.rs hold_vcs_svn_listener; cli.rs svn start + restart tests
  (edge_port_before_serving takes the port line; start test uses main's anchor_with_edges/port_on).
- The predecessor's failure: his CLI test read svnserve's 207-byte greeting; the daemon writes 198 (no svndiff1, by
  design). Not a server bug.
- Tests (macOS arm64, Mac Studio): r394 over f1fbd2c44 server 20/20, cli 5/5, fmt; r373 grammar 14/14, anchor 1/1,
  api piece tests green + role::stop_tests A-510 flake (reported); r344 api 39+4 ign. Run request filed 23:25Z.
- Studio scratch deleted (g234-svnlistener 242M, g234-target 21G). Worktrees: ~/slates-work/g234-vcs-svn (scratch,
  pushed), ~/slates-work/g234-vcs-svn-land (landed).
- Owed (GAPS A-656): the repositories' lookup (rest of find_repos, owner shard, move), sources per call, CRAM-MD5
  keys, learned waits (one mechanism, 3 edges), SSH tunnel entry point; other platforms (main's).

## RELAUNCH 2026-11-19 ~22:15Z (fresh agent after an interrupt): the --vcs-svn piece, A-689 (claimed in the ledger)
- Branch agents/g234-vcs-svn pushed at c6f0ec569 (== local): red 825715a27 (cli test), green scratch 5292f9918 +
  c6f0ec569. No g234 process on the Studio. Its logs ~/.cache/g234-svnlistener (Studio), target ~/.cache/g234-target.
- Predecessor's green run at c6f0ec569 (macOS arm64, Mac Studio, 22:07-22:12Z): api contract 1/1, api role 5/5,
  cli grammar 12/12, server deploy 14/14, svn serve:: 27/27, fmt clean; BUT the CLI test
  an_anchors_vcs_svn_flag_starts_its_daemons_svn_listener FAILED: cli.rs:13615 read unwrap Err WouldBlock (errno
  35, the read timeout) after the daemon logged 'the svn:// listener serves on 127.0.0.1:49155'. Red (825715a27):
  'the anchor ended before its daemon served' (flag unknown). To do: find why no greeting; merge origin/main
  (225d7eead's held_listener / ENV_VCS_HTTP / StartSpec::vcs_http) and mirror it as vcs_svn; watch g299's A-684
  (--vcs-ssh, StartSpec::vcs_ssh) which touches the same files.
- ~22:40Z: merged origin/main b94ae6e90 into the scratch branch (6d06dcf87; contract now 54; cli.rs taken from main);
  added: anchor ENV_VCS_SVN + Supervisor::hold_vcs_svn_listener; api role/anchor.rs hold_vcs_svn_listener; StartSpec::
  vcs_svn + start test; server daemon.rs held_or_bound_listener + listens_at (moved from vcs_http/daemon_edge.rs, shared)
  + bind_vcs_svn_listener; vcs_svn.rs adopts ENV_VCS_SVN; vcs_svn/tests.rs (in-process greeting + off-loopback);
  cli.rs svn start + restart tests (edge_port_before_serving takes the port line). Scratch 62f99c2b6 has G234-DEBUG
  log lines in vcs_svn.rs (REMOVE before landing). Studio run r291 (fmt, server vcs_svn::, cli vcs_svn).
- r291 FOUND THE BUG: the daemon's greeting is svnserve's WITHOUT svndiff1 (198 bytes; by design, §4.17.4 '5. The
  wire', handshake::SERVER_CAPABILITIES); the predecessor's CLI test read_exact'ed svnserve's 207 bytes and timed out.
  Tests now take 'svndiff1 ' out of the recording (as slates_svn handshake tests do). Debug lines removed.
- Green: r310 (39478a4a8 over main b94ae6e90), r344 (f53c7f369 over 693bc8feb): server 19/19, api 39+4 ign, grammar
  12/12, cli 4/4, anchor 1/1, fmt clean. r373 (scratch 0fcd3a255 over 2bfec1547, A-685's args.rs): server 19, grammar
  14, cli 5, anchor 1, fmt clean; api 38 passed + role::stop_tests A-510 test FAILED (no A510-SENT within 20s, load
  11-13; outside the piece, passed r310/r344 with the same api code) -> report to main, no rerun.
- LANDING worktree ~/slates-work/g234-vcs-svn-land, branch agents/g234-vcs-svn-land: 9abe7e9a2 THE commit (squash of
  scratch, base 693bc8feb, tree == r344's), 8c9819826 merge of 2bfec1547 (== scratch 0fcd3a255), merge of f1fbd2c44
  in progress (resolved, == scratch 2e7df134a); r394 runs server + cli + fmt over 2e7df134a. Then: commit the merge
  citing r394, diff stat, markers, push HEAD:main; run-requests; report to main; delete Studio scratch.

## NEXT (main's redirect ~21:42Z, goal 93): `slates anchor|daemon --vcs-svn ADDRESS` -- IN PROGRESS
- A listener running A-656's svn:// connection task per connection, in g299's `--vcs-http` pattern (fb6b66296: one
  address reader in slates_api::role, the anchor passes the address to its daemon, the port logged before serving,
  loopback only without --fleet). Failing cli integration test first: an anchor started with `--vcs-svn 127.0.0.1:0`
  answers an svn:// greeting. Focused tests (api role, deploy, cli grammar + the new test, svn connection) + fmt on the
  Studio; one commit + merge; END.
- Worktree ~/slates-work/g234-vcs-svn, branch agents/g234-vcs-svn (from origin/main 2410011da).
- PLAN (decided ~21:55Z): (1) api role: ONE reader `vcs_address(text, fleet_node)` replacing vcs_http_address (both
  flags; loopback alone without --fleet); ProcessOptions.vcs_svn; Given/takes_value/Role::args; daemon role sets
  DaemonConfig::vcs_svn and logs "slates daemon: the svn:// listener serves on IP:PORT" before the serving line;
  start.rs + liveness_tests `vcs_svn: None`; CONTRACT 53 + history line. (2) cli args.rs: grammar + usage + test like
  the_grammar_parses_the_daemons_vcs_http_edge. (3) server: config.vcs_svn: Option<SocketAddrV4>; daemon.rs
  vcs_svn::start beside vcs_http::start + Daemon::vcs_svn_port; new crates/server/src/vcs_svn.rs: listener on the
  control shard, seats = vcs_http's Bounds::of_shard seats, each connection a spawn_child (detached), Seat guard,
  overdue at the connection's first RTO (vcs_http::waits::first_rto_ns over TcpStream::round_trip) and the most
  overdue reaped only when a seat is needed; the task: Connection::new greeting, read the client's answer, refuse it as
  svnserve's find_repos does (skip_scheme_part -> BadUrl, backpath -> BadPath, else NoRepository: the repository
  lookup is the NEXT piece), write, close. (4) cli test: anchor --vcs-svn 127.0.0.1:0, read the port line, greeting ==
  fixtures/handshake/no-repository.server.bin's greeting, send its client answer with the port, expect
  `( failure ( ( 210005 N:No repository found in 'svn://127.0.0.1:PORT/missing' 0: 0 ) ) ) ` then EOF.
  (5) docs: design §4.17.4 A-656 status + amendment "A-656 part 2, first slice", GAPS, docs/cli.md, README.
- Owed after it: the repository lookup (name on its partition's shard, connection moved there as vcs_http moves one),
  Sources lent on the owner shard, CRAM-MD5 keys over §4.13's principals, learned waits (one generic mechanism).

## A-656 continued (3), its instant tested: an expiring lock (relaunch 2026-11-19 ~21:20Z) -- LANDED ~21:41Z
- [LANDED] main 2410011da (push 0577ab0bd..2410011da, fast-forward): ONE commit ba0ddb2ee + merge 2410011da of
  origin/main 0577ab0bd. Predecessor's work (scratch agents/g234-lock-expiry c73ed41ae, staged landing in
  ~/slates-work/g234-lock-expiry-land) finished: tests strengthened (update: at the expiry byte for byte a
  never-expiring lock's update, past it an empty table's, the held update reaching close-edit; commit: svnserve's words
  + Last::Refused at the expiry, (None, Taken) past it).
- Tests: serve::tests::an_update_keeps_a_lock_until_it_expires_and_takes_its_token_away_after,
  commit::drive::tests::a_commit_naming_a_lock_meets_it_until_it_expires_and_not_after. Both passed at once, no drive
  change, no bug claim. Run (macOS arm64, Mac Studio, 21:37-21:38Z, scratch agents/g234-lock-expiry-run ef721cd35 over
  main 3612d1ef9): `cargo test --locked -p slates-svn --lib -- commit:: serve:: lock:: report::` 132 passed, 359
  filtered out; fmt clean. slates-svn's inputs identical to the landed tree. GAPS line (A-656 block).
- Finding for main (read, not run): the commit drive judges every call's lock and close-edit's verdict at ONE instant
  fixed when the commit began (CommitStart::now -> Setup::now; commit/drive.rs judge 2071, check_lock 2288), while
  FSFS judges each check at its own apr_time_now() (Subversion 1.15.0 libsvn_fs_fs/lock.c 457 lock_expired, sha256
  45d641ae...; copy in ~/slates-work/g234-fsfs-src-2026-11-19) and the update drive now judges per call
  (Sources::now): a lock lapsing mid-commit still refuses the commit's later calls and its close-edit.
- Studio scratch deleted (g234-lockexp, g234-target, predecessor's g234-expiry*). Worktrees: g234-lock-expiry
  (scratch), g234-lock-expiry-run (scratch), g234-lock-expiry-land (landed).

## A-656 continued (3): one record of locks (main's piece from my finding, ~20:38Z; resumed after a restart) -- LANDED, ENDED ~21:00Z
- [LANDED] main 8727ff426 (push bb535bd4d..8727ff426, fast-forward): ONE commit. Built on
  agents/g234-one-lock-source-land from origin/main bb535bd4d by cherry-pick -n of the scratch commits 3c2845b8a
  (red test), 528b9711d (fix), 8ffad9617 (docs) on agents/g234-one-lock-source; main's a5ceb9c02 (g297: a denied
  read authenticates mid-command; list folds non-ASCII) had changed serve.rs and serve/tests.rs: one conflict
  (resume's where clause, main's R: reads::Repository kept) and main's new guards uses converted (read_again's G; its
  tests' FixtureGuards args). Scratch merge 949486e34 (never on main) had the landing tree (3a1c079ab) for the run.
- What: report::drive::Guards removed; drive::Sources { locks: &LockTable, now: AprTime }; held_token() reads
  LockTable::get_lock(path, now) (expired = none); serve::Sources loses guards, gains now; the generic G gone from
  serve.rs, exchange.rs, drive.rs; green.rs FixtureGuards removed, locks_of(name, report) -> LockTable (the
  update-with-lock session's lock), NOW = AprTime(0).
- NOTE: main's brief said "the commit drive reads Guards::lock_token"; in fact the update's report drive did
  (drive.rs 1085, 1308); the commit drive already read the table (check_lock). Fixed the real reader.
- Test: serve::tests::an_update_keeps_a_lock_the_lock_table_holds_and_takes_away_one_it_does_not. RED (macOS arm64,
  Mac Studio, 20:42Z, 3c2845b8a over main 901be4f52): with alice's lock on /trunk/a.txt in the table and none in
  the guards, the update sent ( open-file ( 5:a.txt 2:d0 2:c1 ( 6 ) ) ) and ( change-file-prop ( 2:c1
  20:svn:entry:lock-token ( ) ) ). GREEN: 528b9711d over 901be4f52 `cargo test --locked -p slates-svn --lib --
  commit:: serve:: lock:: report::` 126/126 (20:48Z); the landing tree 3a1c079ab over bb535bd4d 130/130 (20:55Z),
  fmt clean; the landed commit's crates/ == the tested tree, docs updated to 130 after the run.
- Docs: design 4.17.4 status (A-616 part 2 second slice: lock table at the call's instant), amendment "A-656,
  continued (3)", GAPS Continued (3) line, bug record
  docs/bugs/2026-11-19-an-update-judged-locks-by-the-callers-guards-not-the-lock-table.md.
- Owed (main's next piece per its message): the commit drive's second compile of Policy::authz -> take the session's
  set. Windows run + red-checks filed.
- Studio scratch deleted (g234-locks, g234-target, logs). Worktrees: ~/slates-work/g234-one-lock-source (scratch),
  ~/slates-work/g234-one-lock-source-land (landed).

## A-656 continued (2): one read rule for a session (main's piece from g297's step-6 finding, ~20:20Z) -- LANDED, ENDED ~20:40Z
- [LANDED] main 901be4f52 (push 22855c29e..901be4f52): ONE commit f0f110cf3 + one merge of origin/main (main moved
  once; the only conflict the amendment log's end, main's first). Built on agents/g234-one-read-rule-land from
  origin/main by cherry-pick -n of the scratch commits 54d9eb302 (red test), 49040a4d1 (fix), 262f6f508 (docs) on
  agents/g234-one-read-rule (never on main).
- What: authz::may_read(rules, path) is the one read rule (moved from reads' readable); serve.rs
  ReadRules::for_principal + Established::compiled_read_rules compile the principal's rules once (again after an
  authentication) and hand them to reads_session, update's two target checks and the drive's Sources::rules;
  report::drive::Guards keeps lock_token alone; Drive::next takes &Sources; green.rs FixtureGuards holds locks alone,
  AUTHZ + rules_of(user) compile the recorder's rules; serve tests take AUTHZ from green.
- Test: serve::tests::an_update_and_the_reads_judge_one_path_by_the_one_rule_set. RED (macOS arm64, Mac Studio,
  20:25Z, 54d9eb302 over main 733e6de6b): check-path secret refused 170001, update secret answered
  "( success ( ( ) 0: ) ) " and awaited its report. GREEN (20:30Z, 49040a4d1): `cargo test --locked -p slates-svn --lib
  -- report:: reads:: serve:: authz::` 158 passed, 319 filtered out; fmt clean. Main changed no crates/svn or
  Cargo.lock through landing; the landed slates-svn == tested.
- Docs: design 4.17.4 status (A-616 part 2's second slice: guards = locks, one compiled read rule), amendment
  "A-656, continued (2)" (no A-number assigned by main), GAPS (A-656 block, Continued (2) line), bug record
  docs/bugs/2026-11-19-an-update-read-by-the-callers-guards-where-the-reads-used-the-sessions-rules.md.
- Findings for main (not in the piece): locks have two sources too (report::drive::Guards::lock_token for the drive,
  A-602's LockTable in serve::Sources::locks); the commit drive compiles the same Policy::authz rules a second time
  (commit/drive.rs:460, 950; same answers).
- Studio scratch deleted (g234-rule, g234-target, logs). Worktrees: ~/slates-work/g234-one-read-rule (scratch),
  ~/slates-work/g234-one-read-rule-land (landed), both clean.

## A-616 continued (7): one svndiff 0 chunk writer (main's piece, 2026-11-19 ~19:45Z) -- LANDED, ENDED ~20:20Z
- [LANDED] main 733e6de6b (push a6e688321..733e6de6b): the piece is ONE commit, bdc73e368, plus three merges of
  origin/main (44d503232, 97e66db6e, 733e6de6b; main moved twice during the push, each merge's only conflict the
  amendment log's end, main's first). Built on agents/g234-a616-7 from origin/main by cherry-pick -n of the scratch
  commits b993db65a + 01de0c6b1 (agents/g234-svndiff0-writer, never on main) + the docs.
- What: report::editor::DeltaWrites (+ Writes, WINDOW_WRITES) is the one writer of an svndiff stream's writes;
  EditorWriter (textdelta-chunk each, delta committed after every chunk) and reads::file_revs::delta (a string each)
  use it; simple_insertion/window_sections private again, window_sections refuses with SvndiffError; file_revs'
  write_sections gone. g297's get-file-revs-svndiff0 transcripts (~/slates-work/g297-fixtures-in/authz-r88, kiln r88)
  added to crates/svn/fixtures/reads with their sums (checked) + sessions.txt line; g297's re-recorded get-dir,
  get-file, get-file-revs, get-locations transcripts NOT taken (hash-order-only differences, main keeps its own).
  The report recorder's two never-recorded svndiff0 sessions (cd84e3811) taken out: the owed recording is settled.
- Test: reads::tests::both_callers_of_the_one_delta_writer_send_svnserves_svndiff_0_writes (replay 5 commands;
  shapes (14, 4, 9, 1); every delta's windows through EditorWriter == svnserve's strings) + the sums test's name.
- Run (macOS arm64, Mac Studio, 2026-11-19 20:12Z, at 01de0c6b1 over main 545181b30; main changed no crates/svn or
  Cargo.lock through landing): `cargo test --locked -p slates-svn --lib -- report::editor:: report::exchange::
  reads:: serve::` 69 passed, 407 filtered out; `cargo fmt -p slates-svn -- --check` clean. First build (b993db65a)
  failed: a closure returning its argument's borrow (lifetime); made a function.
- Docs: design 4.17.4 status (A-613's reads "Owed: the svndiff 0 session's recording" settled; part 2 first slice's
  editor line names DeltaWrites), amendment "A-616, continued (7)" (no new A-number: main assigned none; ledger's next
  free was A-677), GAPS (A-613 svndiff 0 line, A-616 (6) settled, new A-616 (7) line), bug record follow-up.
- Finding for main (not in the piece): two svndiff integer readers -- svndiff.rs's private read_integer (Result<
  Option>) and report/editor.rs's pub(super) read_integer (Option, used by window_sections and report/recorded.rs).
- Studio scratch deleted (g234-writer, g234-target, logs; also the predecessor's g234-a656 950M, g234-a387, g234-ab).
- Worktrees: ~/slates-work/g234-a616-7 (landed, clean), ~/slates-work/g234-svndiff0-writer (scratch, clean).

## RELAUNCH 2026-11-19 ~17:00Z (fresh agent; main e3234e618 at start) -- ENDED ~19:00Z, both pieces landed
- [LANDED] A-656 (part 1 + continued (1) + the drop-at-success test): main 9a7339e9e (push e3234e618..9a7339e9e,
  fast-forward, ~17:25Z). The pushed branch agents/g234-a656-svn-connection (b7fb9fb77, merged with main e3234e618 at
  36e116f0b) held seven "WIP" commits and a default-message merge, so it was rebuilt on agents/g234-a656-land from
  origin/main: the seven unbuilt WIP commits folded into 551b5deab (part 1's first built commit, its message kept) as
  58c81a9b4, the nine later commits cherry-picked with their own messages, the four merges dropped; one conflict (the
  amendment log's end, main's first). Landing tree == tested tree (tree 99deee5bb). MAIN'S CORRECTION (CLAUDE.md
  section 8, binding): one piece lands as ONE commit plus the merge of origin/main; A-656 went in as ten. Do that from
  now on.
- Green A-656 (Windows x86_64, jupiter, 17:08-17:13Z, at 36e116f0b): `cargo fmt -p slates-svn -- --check` clean;
  `cargo test --locked -p slates-svn --lib` 466 passed, 0 failed (main's baseline 442). Red seven ways earlier by the
  predecessor (in the commit messages).
- [LANDED] the svndiff0 fix (A-616 continued (6)): main cd84e3811, ONE commit (push 92ffda926..cd84e3811, ~18:55Z),
  built on agents/g234-svndiff0-one from origin/main: cherry-pick -n of 28a3592ae (agents/g234-svndiff0-land =
  ba18879f2 rebased on 9a7339e9e, with my changes) + rustfmt's one hunk + the run's evidence in the docs. My changes
  to the predecessor's unbuilt fix: the test gains a window copying no source in two instructions (window_handler's
  shape), without which a "first instruction is new data" mutant survived; the insertion branch's
  `if !new_data.is_empty()` dropped (always true: Progress::take refuses a zero-length instruction and Progress::finish
  wants all new data used), with a comment; a 101-column module-doc line and a 152-column design line rewrapped.
- Runs (Windows x86_64, jupiter): green at 28a3592ae `cargo test --locked -p slates-svn --lib` 467 passed, 0 failed
  (18:45Z, fresh clone + fresh target: the A-656 clone and target had been deleted by a cleanup); fmt asked for one
  assertion split over four lines (applied; `rustfmt --check` 1.9.0 of the pinned 1.98.1 passes the landing tree, and a
  control on the unformatted file fails with that hunk). Reds in ONE build with G234_RED switches, filter
  report::editor (18:49-18:52Z): none 8/8; 1 (no insertion path = main's writer) the new test alone failed; 2 (any
  version) it + every_recorded_drive_is_written_again_as_svnserve_wrote_it; 3 (first instruction New) it alone; 4
  (header before every insertion window) it + a_client_that_does_not_accept_svndiff2_is_sent_version_0.
- Owed (GAPS, design, bug record): the two svndiff0 sessions recorded against stock svnserve 1.15.0 on the Mac Studio
  (`make-fixtures.py OUT svndiff0-checkout,svndiff0-update`), added to report/recorded.rs SESSIONS, and
  every_recorded_drive_is_written_again_as_svnserve_wrote_it writing each drive with its own client's capabilities;
  the macOS runs of both pieces (run-requests.md). A-656's own owed list is unchanged (part 2 listener, hooks' pauses,
  lock verbs, reads' auth-for-one-command).
- Worktrees left: ~/slates-work/g234-a656-land (clean, landed), ~/slates-work/g234-svndiff0-one (clean, landed),
  ~/slates-work/g234-svndiff0-land (holds the rustfmt hunk uncommitted; superseded by cd84e3811), and the older
  g234-a656 / g234-svndiff0. jupiter scratch all deleted (~18:55Z).

## A-656: the svn:// connection (assigned 2026-11-19 ~07:00Z), and the A-616 svndiff0 sibling bug (main ~13:00Z)
- Worktree ~/slates-work/g234-a656, branch agents/g234-a656-svn-connection (pushed). Studio scratch ~/.cache/g234-a656
  (shallow clone + target) -- delete after reading each run. Load rule: CARGO_BUILD_JOBS=4, one cargo at a time.
- [GREEN, NOT LANDED] Part 1, 551b5deab, merged with main at 7a958b0f9: crates/svn/src/serve.rs (sans-io Connection:
  handshake, reads + streams, reparent, report exchange, unserved 210001), A-616's drive changed texts through A-652's
  deltifier. 427/427 on the Studio before the merge; the merged tree's gate (run r1213) never reported: the Studio went
  OOM and OFFLINE (~07:40Z) and has stayed offline (checked ~5-min cadence; runner adas-mac-studio.local-2).
- [WRITTEN, UNBUILT] Continued (1), 70127c768: commit through the connection (Turn::CommitAsked, Connection::commit
  -> A-655's commit::drive::commit, Committing feeds Drive::command, Turn::Accepted, committed() / not_committed();
  Uncommitted::NotServed = 200007 until A-655 part 2; drop until abort-edit/success); svnserve's authentication for
  one command (handshake::Server::for_command; serve asks for commit (Write) and update's unreadable target (Read);
  Turn::Established then Resuming re-runs the command, Context::authenticated_now skips the trivial auth);
  serve::Sources {index, repository: reads::Repository + commit Green, guards, locks, policy}; part 1's update test
  was WRONG (svnserve asks for auth first) -- corrected. Tests: serve/tests/commit.rs replays all 16 recorded commit
  connections + bob's race byte for byte; handshake test vs svnserve's own lock-many auth (lock-a session) byte for
  byte; 8 more serve tests. Design status + amendment "A-656, continued (1)" + GAPS written.
- [WRITTEN, UNBUILT] svndiff0 bug, worktree ~/slates-work/g234-svndiff0, branch agents/g234-svndiff0-header (pushed),
  ba18879f2 on top of 70127c768: report/editor.rs writes send_simple_insertion_window's shape (svndiff.c 89-151,
  259-261); hand-made test from svndiff.c; make-fixtures.py gains svndiff0-checkout and svndiff0-update (client
  without svndiff1/accepts-svndiff2); docs/bugs/2026-11-19-a-version-0-delta-sent-its-header-as-a-chunk-of-its-own.md;
  amendment "A-616, continued (6)"; GAPS. Sweep: only report/editor.rs writes textdelta-chunks; dump/dav/export write
  continuous streams; commit drive decodes whatever the chunks; g297's unlanded get-file-revs already has the shape.
- WHEN THE STUDIO RETURNS, in order: (1) gate 70127c768 (fmt + `cargo test --locked -p slates-svn --lib`, jobs 4),
  fix compile errors (written blind), red-check: handshake for_command info-gating, serve resume (skip re-run),
  not_committed drop rule; land A-656 (fetch, merge, diff stat, push HEAD:main). (2) svndiff0: record the two sessions
  (`make-fixtures.py OUT svndiff0-checkout,svndiff0-update`), add them to report/recorded.rs SESSIONS (+REPLAYED in
  serve tests, RECORDED_DRIVES 30->32), make every_recorded_drive_is_written_again_as_svnserve_wrote_it write each
  drive with its client's capabilities; run red on a scratch copy without the editor.rs change, then green; land.
- Owed of A-656 after that: part 2 (crates/server/src/vcs_svn.rs listener in vcs_http's pattern; SSH entry point for
  g294, tell main its signature; run request for main's live stock `svn checkout`); hooks' pauses (A-661) once
  g268's Step::Hook is on main; the lock verbs; reads' auth-for-one-command with g297's A-606 authz.
- MAIN'S ORDER (2026-11-19 ~08:25Z): no polling or background watch -- main messages when the Studio answers. Then:
  (1) land part 1's merged gate (7a958b0f9), (2) land 70127c768, (3) land the svndiff0 fix with its two recorded
  sessions. Part 2's waits: ONE generic mechanism for all three edges -- after g299's (agents/g299-a625, touches
  vcs_http) and g294's (agents/g294-a651-edge, vcs_ssh/waits.rs, a separate copy) land, make PhaseWaits and Tracker
  generic over an edge's phases in one module, move vcs_http, vcs_ssh and the svn edge onto it, delete g294's copy;
  prove all three with the existing waits tests UNCHANGED, add the svn edge's.
- PAUSED 2026-11-19 ~08:25Z waiting for main's message that the Studio answers. Studio scratch ~/.cache/g234-a656
  still to delete once it is back (could not reach it).
- Not for the repository (main's order): the svnserve finding stays with Ada; nobody verifies or reports it.

## UI GOAL 93: A-616, Subversion piece 7's third part -- ra_svn's reporter (assigned 2026-11-19 ~04:40Z)
- Worktree ~/slates-work/g234-a616, branch agents/g234-a616-svn-reporter. Builds/tests on the Mac Studio d1323898 only
  (sync false; shallow GitHub clone ~/.cache/g234-a616/slates updated by `git fetch --depth=4` + `merge --ff-only`;
  CARGO_TARGET_DIR ~/.cache/g234-a616/target; delete both at the end). Nothing built on the MacBook.
- [LANDED] Part 1, main 0392c81a6 (2026-11-19T05:33Z): the reporter's commands and report. crates/svn/src/report.rs
  (Request::read for update/switch/status/diff; Reporter::begin/receive: set-path, delete-path, link-path,
  finish-report, abort-report, first refusal kept, top-level checks, room -> 210009; Report value for part 2),
  report/tuple.rs (marshal.c vparse_tuple port), report/tests.rs, fixtures/report (history.txt, make-fixtures.py, 80
  sessions, SHA256SUMS; recorded on the Studio at d3a4696be). handshake.rs hands on `Session` (principal, client
  capabilities, uuid, root URL). Main's five clippy findings in handshake/tests.rs fixed; handshake docs rewrapped to
  100 columns. Lib: 48/48 report+handshake, 306/306 crate on 109ac36, fmt clean. Run request filed 05:35Z.
- [LANDED] Part 2 slice 1, main 7d861fc7e: report/drive.rs Call+Facts (the set g298's dav::Drive consumes),
  report/editor.rs EditorWriter (editorp.c/marshal.c bytes; 3 chunks a window; absent gated; svndiff 2/0), tests
  replay all 28 recorded drives byte for byte but LZ4 blocks; fixtures shared in report/recorded.rs. Lib 326/326.
- [LANDED] Part 2 slice 2, main 786f132c7: report::drive::Drive (reporter.c over index + reads::Repository + Guards,
  explicit stack, step budget, Next::{Call,Paused,Ended}, DriveFailure), drive/history.rs (fs_closest_copy,
  svn_repos_deleted_rev), drive/green.rs (history.txt -> index+green, checked vs changed.txt), drive/tests.rs (27
  recorded drives matched, copyfrom, pause, generated hostile reports in the writer's order). Red 6 ways. 365/365.
- [LANDED] 3239c8881: drive/history.rs pub(crate) as the crate's one copy-root walk (main's call; g297's reads.rs to
  use it); history check CRLF-tolerant.
- [LANDED] 602e1bd99: report::exchange::Exchange (Reporter + Drive + EditorWriter as accept_report/editorp.c join
  them; early answer before every call; close-edit/abort-edit answers; client error chains echoed as
  handle_failure_status reads them; Budget room/steps). 34 recorded exchanges replayed vs svnserve's bytes; red 6 ways.
- [LANDED] 733da9dd5: make-fixtures.py record-only mode; two copy-source sessions recorded against svnserve
  (update across a move, checkout from nothing): copy named '/trunk/new.txt' 3, no delta for an equal text. 404/404.
- Main's calls (2026-11-19 ~06:35Z): xdelta is g264's (A-652) in svndiff -- the drive keeps sending full texts as new
  data until it lands, then switches; the copy-root walk is mine, g297 calls it.
- Owed of A-616: switch the drive's windows to A-652's generator when it lands; the connection task is piece 7's.
- Studio scratch ~/.cache/g234-a616 deleted (2.1 GB) at the END, 2026-11-19 ~06:58Z. Run request filed 06:58Z.
- ENDED after A-616 part 2; waiting for main's next piece.

## UI GOAL 92 (51cef9f7): A-599, Subversion piece 7's first part -- ra_svn's greeting and authentication (relaunched 2026-11-19 ~03:20Z after a usage-limit stop)
- Rule readback sent to main first (SendMessage). Worktree ~/slates-work/g234-a599, branch agents/g234-a599-svn-handshake
  (clean). Builds and tests on the Mac Studio d1323898 only (sync false, GitHub clone, CARGO_TARGET_DIR
  ~/.cache/g234-a599, deleted after: 623 MB freed). Nothing built on the MacBook.
- [LANDED] A-599: main ac693838c (merge; the push moved main 99c72e534..ac693838c). Commits: 33dd139d6 the fixture
  recorder (WIP); a290ff424 tests + fixtures + the earlier draft given the tested API (red; two test lines did not
  compile); 0fff38cc0 tests compile + the lock by hand (hmac, md-5, digest's subtle alone; cargo's own resolution also
  moved five packages to windows-sys 0.61.2, left out); cf391a932 the three fixes (green); c2f33cc9f fmt + docs;
  feaa0973a, 575188f7f, ac693838c merges of main (A-602 lock, A-605 mergeinfo, A-592 commit modules beside mine);
  5c004a43f the complexity split; 25742a663 and 55367dc67 fmt + recorded numbers.
- Module: crates/svn/src/handshake.rs (Server: receive/open->Access/refuse/challenge(&CStr)/challenge_failed/key; CramKey
  over RustCrypto hmac+md-5; Refused, AuthFailure, Unserved incl. BadPath, Progress, Verdict), tests in
  handshake/tests.rs (28), fixtures crates/svn/fixtures/handshake (make-fixtures.py, SHA256SUMS, 17 sessions: stock svn
  1.15.0 r1937767 vs svnserve via a relay on 127.0.0.1:3690 and a tunnel, plus hand-made hostile clients; recorded on
  the Studio 2026-11-19T03:46Z; files moved here machine to machine through kiln code_run, sums checked both ends).
- Tests (Studio, `cargo test --locked -p slates-svn --lib`): red on 0fff38cc0 17 of 25 (8 failures = the draft's three
  decisions: ra-client without its list, NUL-holding CRAM response, item count bytes/2); green cf391a932 78/78;
  575188f7f 139/139; landing tree ac693838c 154/154 (28 handshake), `cargo fmt -p slates-svn -- --check` clean.
- Facts found: aws-lc-rs as vendored has MD5 (local change) but HMAC over SHA only (hmac.rs 124, 143-155); Subversion
  1.15.0's cram.c, internal_auth.c, protocol == 1.14.x d434dc14 byte for byte, serve.c differs by a Windows include and a
  comment; Homebrew's svn is built with Cyrus SASL (ANONYMOUS sends base64 "anonymous@host\n"; a failed CRAM-MD5 client
  sends ( CRAM-MD5 ( ) ) again before learning it has no more credentials); size_of::<Value>() is 24 on
  aarch64-apple-darwin; A-591's lesson: clippy's cognitive complexity counts each assert in tests too.
- Docs: design §4.17.4 status, item 10, piece 7 and the A-599 amendment entry; GAPS §4.17.4 entry; research notes §1
  (1.15.0 sources and RFC 2104/2202 with hashes) and §2 (Authentication).
- Owed (GAPS): the URL's canonical form, the repository it names and repos-info's root URL as find_repos derives them
  (piece 7's connection task); a learned failed-attempt limit (§4.13); CRAM-MD5 keyed contexts across a restart (§4.13);
  main's choice on HMAC-MD5 as a local change of the vendored aws-lc-rs; main's gates.
- Run request filed 2026-11-19T04:31Z (clippy/xtask/doc/cross-lint, Linux+Windows lib tests, an optional Linux
  Subversion 1.14 second witness, and a not-executed svnserve auth observation for Ada through main).
- Scratch kept: ~/slates-work/g234-svn-src (1.14.x sources; 1.15.0/ subfolder), ~/slates-work/g234-a599-cc.py (the
  complexity estimator), ~/slates-work/g234-a599-draft-handshake.rs (the earlier session's draft, moved out of the old
  worktree so nothing there commits it; superseded by main's handshake.rs).
- ENDED after A-599; waiting for main's next piece.

## UI GOAL 85 pieces (restarted 2026-11-19 ~01:10Z after a usage-limit stop; clone ~/slates-work/g234-windows-wake-estimate)
- [LANDED] A-549: main 0d6fe4d4e (change 920405c5b; red tests 62eb831ca by my predecessor, run red on 5d07377b1; tests
  split f9e15abc1; merges a7d81a2d4, 0d6fe4d4e). Snapshot::live_entries recorded at Volume::snapshot; clone_inner starts
  from it; rebuild_snapshot_tree returns a recovered snapshot's entries (named_entries of its own directory images +
  live_len of head-shared dirs); Volume::snapshot_inodes; admit_clone reads inherited inodes from it (stale snapshot
  refused before any slot). Red (Studio, main a613562eb + tests): bounded clone allowance u64::MAX vs 36 (share 28
  refused), dynamic hold 2 vs 13, entries 13 vs 9 (live) and 14 vs 10 (restart). Green: vfs --lib 82 passed 1 ignored
  on the merged tree; server --lib clone 8/8 (with main's A-544 tests), a544 5/5; fmt clean. Mutations of the recovered
  count (no shared arm: 7 of 9; zero: 0 of 9) both red. Bug record docs/bugs/2026-11-19-a-clones-admission-and-entry-
  count-read-its-origins-head.md; GAPS 8c; design entry A-549. Siblings left open (GAPS): A-561; clone admitted with no
  entry allowance; resize leaves the entry allowance; recovery counts whiteouts as live entries (reading only).
- [LANDED] A-561: main 49e0d61ad (change 5c97c983c; red test e2176302f, fix 8a5fd1074; merges 208b9b5db, 49e0d61ad).
  A restart refused (RecoveryIncomplete -> listed not_recovered ImageRefused, clones of its snapshot
  OriginNotRecovered) every clone holding its own snapshot while it shared a file with its origin. Fix (recovery side,
  chosen over capture-origin-first: capture is right by SharedSource::Head's definition; already-published images still
  say Head and rebuild only this way; no layout/IMAGE_VERSION change; neither encoder touched): shared_source's Head arm
  resolves through the head's rebuilt table, kind from the inode, as the Origin arm does. Test (vfs lib, volume.rs
  clone_count_tests): a_clone_holding_its_own_snapshot_is_rebuilt_at_a_restart_whatever_it_shares_with_its_origin, a
  6-bit matrix (48 distinct cases; the 32-case matrix + the clone rewriting an inherited file) comparing what the clone,
  the clone of the clone and a clone of the clone's snapshot read and count before and after the restart. Red with
  main's recover.rs 32 of 48 (exactly the clone-snapshot cases); green 48/48; vfs --lib 84 passed 1 ignored on the
  merged tree; fmt clean. Bug record docs/bugs/2026-11-19-a-restart-refused-a-clone-holding-its-own-snapshot.md; GAPS
  8c; design entry A-561. Run request 03:10Z (recovery oracle, server recovery suite + a test to write there, lints).
- [LANDED] A-572 (main's brief: the three GAPS 8c siblings, worst first, one commit each, failing --lib test first):
  - part 1 main bfb2c8208 (change 763bc0680; red a7f0970fc, fix 6b7aae06e): a restart counted whiteouts as live entries
    (rebuild_entries placed them through dir_insert, which counted them); dir_place counts / dir_insert admits only a
    placement that names a child. Test base.rs tests a_restart_counts_no_whiteout_as_a_live_entry: red 6 vs 3, green;
    vfs lib 85 then 88 passed 1 ignored on merged trees.
  - part 2 main 8b0a9a57e (change 9498bf881; red a7f0d10a7 + move 2e890253d, fix cc72699d1): admit_clone sets the
    clone's entry allowance = entry_allowance(size).max(the clone's count = its snapshot's). Test clone_admission_tests
    a_clone_is_admitted_with_an_entry_allowance_never_below_its_snapshots_entries: red u64::MAX vs 4,104, green; server
    lib clone 11 passed 1 ignored. (a7f0d10a7/cc72699d1 put the test in materialization_wait_tests -- main had added it
    after clone_admission_tests -- and don't compile alone; noted in the run request.)
  - part 3 main f24745e45 (change f682405de; red c23f4d738, fix 89a58c8d5): resize sets the entry allowance =
    entry_allowance(new size).max(entries held). New module resize_admission_tests (clone_admission_tests' helpers made
    pub(super)): red 4,096 vs 65,536 after a resize up, green.
  - Bug record docs/bugs/2026-11-19-a-restart-counted-whiteouts-as-live-entries.md (3 sections); GAPS 8c A-572 entry;
    design entry A-572 (parts 1-3 inside it; main's later entries follow it). Run request 03:50Z.
  - kiln refused every run for my session 02:10Z-02:27Z ("not connected to the kiln server"); told main; waited in a
    bounded foreground loop, no background jobs.
- Studio scratch ~/.cache/g234-a572 deleted after A-572.
- [LANDED] A-584 (UI goal 91, part "The Shards-owed items done with Shards' side" 01a1f121..a904eb1d): main 8a10459cb
  (change a7ba6ae92; red de7e50cff, fix b8ef6b4f6). POLICY_READ (Shards, 2026-11-19 ~04:20Z): origin/main
  aa819b0585649563d7185298db58bda8ac9dd533 (GitHub API, read-only); AGENTS.md 30 lines sha256 f66d304c...6db41;
  docs/development-rules.md 200 lines sha256 df2acf47...13d3; docs/development-evidence.md 47 lines sha256
  61613a24...fb63; all read in full; role implementer (slates side), READ-ONLY on Shards, owned Shards paths none.
  Triage: every g268 Shards-owed item's slates side is built (resync incl. condition 4c, A-303/516/532 save/restore;
  Shards' record architecture.md:4049: `shards restore` takes no --slates-fs-fd yet -> their build); restore-target
  candidate (which snapshot to clone; RESTORE_ELSEWHERE names none) recorded in GAPS, not built (main's decision).
  Built instead (main approved): A-346's Shards-stream sibling. shards::turn_with ends a stream whose volume
  lost_to_a_successor (first, before the ending's DAX write-back), shards::superseded ends a volume's streams at the
  learned fence / yield / inject (beside fuse::superseded, all platforms), counter shards.superseded_ended. Test
  shards::tests::a_stream_whose_volume_this_node_lost_to_a_successor_ends_unanswered (told + kept): red = told
  stream's SAVE held past 60 s; green 0.79 s; server lib shards:: live_ship:: readmission:: 31/31; fmt clean. Bug
  record docs/bugs/2026-11-19-a-shards-stream-on-a-volume-lost-to-a-successor-held-its-guest.md; GAPS A-346 entry +
  A-362 note + restore candidate; design §4.8 status + A-584 entry. Run request 05:10Z. Still owed (GAPS): guest
  device + WinFsp forms; relinquish waiting for a stream's end.
- Studio scratch ~/.cache/g234-a584 deleted. ENDED after A-584; waiting for main's next piece.

## GOAL 90 (kiln c2e4337b): the two Windows stream items from g268's list
- (a) [DONE by g268, verified 2026-11-18] the Windows streams' write log: A-402 (e0b59a1fe, main ca647c80b).
- (b) the Windows stream resync, slates' side:
  - [LANDED] A-418, the Windows Resync wire (40 bytes): main 0dad803d9 (change d119b92b6).
  - [LANDED] A-423, the anchor's Windows hold formats (WHAT_SHARDS_RESYNC, Record): main a1a463226 (change 2c729f42b).
  - [LANDED] A-427, the attach half (resync pipe held before serving, session Records): main 844ff8f4f (change 936c764a7).
  - [LANDED] A-433, the successor half + `slates shards` {resync}: main ee15687d1 (change 52bfa4f51); real-process test
    slates_shards_resyncs_its_programs_stream_after_its_daemon_is_killed (crates/cli/tests/shards.rs).
  - [LANDED] A-448, the anchor holds a Windows stream's DAX sections: main 0b0df27c5 (change 880f9130e). Sections
    re-offered at the device's asks, not in the Resync (main's decision 2026-11-18). Real-process test
    slates_shards_keeps_its_programs_dax_section_across_its_daemons_kill. Green: anchor handles 14/14, server shards::
    8/8, xwin clean (anchor, mem, vfs, server, api lib+tests; cli bins+tests).
  - [LANDED] A-460 (renumbered from A-456 after the wipe): main 0049aa171 (change 13a3e289a). The Windows handoff
    table (encode_env text, unchanged) rides an inherited read-only section sized to it; SLATES_ANCHOR_HANDLES names
    TABLE_HEX:LEN_HEX (<= 33 chars), checked against table_cap(bound) before anything is mapped; check_owners refuses a
    table naming its own section. Red on main b97d33e (15 build errors); green on the merged tree: anchor handles 17/17,
    rustfmt applied, xwin clean (anchor, server lib+tests). Windows tests + a real supervised restart past the limit
    (40 full-record mounts) + the full-caps restart filed (run-requests 22:10Z), with A-427/A-433/A-448 re-filed.
  - [OWED, Shards' side] Shards' device half takes the Resync through its spawner's grants (condition 4c).
  - [LANDED] A-516 (main's brief: the first slates-side item still owed to Shards, g268's list): main f1a7342b4
    (change 84978f117; red 564cedd32, green 9795afdf1). A-303's first "Not here": a SAVE's snapshot was published on the
    owner shard but not waited for at a live-shipped volume's candidates (sec. 4.10) as a stream's FSYNC is (A-200).
    Now answer_save holds the tables on live_ship::durable_target (DurableOwed gains Undurable::{Errno, Save});
    settle_durable sends them (shards.saved) or refuses SAVE_UNDURABLE (shards.save_refused); counter shards.save_held.
    Test (lib, shards.rs tests, unix): a_save_names_its_snapshot_only_once_the_volume_is_durable_where_the_save_put_it,
    scene by live_ship::ship_to_silent_holders (cfg(test)). Red on 6696fda78 (live SAVE answered tables at once);
    green: server lib shards:: live_ship:: 29/29 in 2.79 s (Studio); fmt clean for my files (main's shards_route.rs
    is not). g268 told (inbox 00:13Z claim, 00:26Z landed). Shards policy re-verified at shards main aa819b058 (hashes
    unchanged); role implementer, read-only. Run requests filed 00:26Z (Windows build, clippy/xtask, Linux lib test,
    server Shards suites, live fleet SAVE, Shards' save/restore). Still open of A-303: a clone's orphan record for a
    nameless file (next candidate for goal 90).
  - [LANDED] A-532 (main's brief, UI goal 90: A-303's second open note): main d73e1652c (change bc8315023; red
    274c10534, green 6dde9e521). A snapshot keeps the head's orphans (Snapshot::orphans), clone_inner gives each an
    orphan record (secured 0), SnapshotImage::orphans + IMAGE_VERSION 32, recovery checks them (snapshot_orphans).
    Tests (vfs lib, volume.rs clone_orphan_tests): the restored clone reclaims at let-go; a restart settles the carried
    orphan and a clone of the recovered snapshot carries it again. Red on 1d2723e51; green: vfs lib 77 passed, 1
    ignored (Studio). Sibling filed: a clone's live_inodes starts from its origin's head count, not its snapshot's.
    Run requests 00:42Z (vfs integration suites, server restore/recovery over a clone, Windows build, gates).
  - [LANDED] A-541 (main's brief, UI goal 85: the bug A-532 found): main 87d606e83 (change 10499c270; red 68040c733,
    green 75c19d762). Snapshot::live_inodes recorded at Volume::snapshot; clone_inner starts the clone from it;
    rebuild_snapshot derives a recovered snapshot's count from its image's entries (no layout change). Tests (vfs lib,
    volume.rs clone_count_tests): each count vs a restart's recount after every step; allowance of tree+1 admits one
    file then NoSpace; across a restart. Red on fba7cd888 (left 9, right 5); green: vfs lib 79 passed, 1 ignored.
    Bug record docs/bugs/2026-11-19-a-clones-inode-count-started-from-its-origins-head.md. Siblings reported, not
    fixed: server admit_clone's `inherited` (origin's head count; version reservation/hold and allowance floor) and
    clone_inner's live_entries. Run requests 00:55Z.
  - [LANDED] A-462, the Unix sibling: main 6696fda78 (change 7e06d565b, follow-ups 383b0f8ef 4fcd591a5 b207f2321
    8c591017e f6472765f). The three handoff parts ride one table (a line each + #crc32c) in a PeerObject (Linux memfd
    sealed FUTURE_WRITE|SHRINK|GROW|SEAL; macOS unlinked shm handed read-only); SLATES_ANCHOR_HELD = FD:LEN_HEX
    (<= 27 chars), checked against table_cap(TableBounds) before reading; a table that cannot be made refuses the spawn
    (AnchorError::Handoff). Limits cited: Linux MAX_ARG_STRLEN = PAGE_SIZE*32 (execve(2), binfmts.h); macOS ARG_MAX
    1,048,576 (getconf on the Studio, macOS 26.4); the restart test reads it at run time (getconf), never frozen.
    Red on b2909fc (49 build errors). Green on the tree merged with main a157b075c (Studio): anchor handles:: held::
    34/34; fmt clean; clippy -D warnings clean for anchor, server, cli (macOS) and anchor, server (xwin); xwin check
    cli tests clean; xtask structural/unsafe/version/toolchain/capabilities ok, literals only in main's
    crates/base/src/s3/list.rs. Linux: anchor lib clippy clean cross (x86_64, aarch64 gnu); held_copy_replay.rs
    type-checks with its gate widened in a scratch clone; the Linux host check never started (adacachy under strain,
    r371 and r27 stopped unstarted) -> filed. Also fixed: main's four handles.rs findings and siblings (CC splits,
    constant assert, string slice, WHAT_SHARDS_DAX Format line); the Windows serve loop's session wake now takes a
    turn (Turned::Session tells the anchor before any reply), recorded in the entry. held_turn.rs left to main's
    cf61d54e4. Run requests filed 2026-11-19T00:02Z (Linux check, Linux unit tests, supervised restarts past the old
    limit on Linux and macOS, server in-process tests, CLI garbling test, Windows suites, main's literal findings).

## GOAL 77 (kiln cc6bfdfd), waiting behind goals 84-90
1. [DONE, main 503662a8f] A-387's measured hold predicate (slates_rt::held_turn).
2. [DROPPED] the idle window F by the same predicate (A-218 measured and rejected the window-side comparison).
3. macOS R9 miss at cold start: waits on the profile run request.
4. Edit+merge loaded p99 < 1 ms, W1's per-edit hop: g083's (A-381/A-388).
5. Edit+merge loaded p99 < 1 ms, where a loaded verdict waits: waits on the scheduler-profile run request.
6. Windows job priority-class limit (A-361's open GAPS item): park on WaitOnAddress when no I/O is outstanding -- NEXT.
7. Quiet-Mac late wake (g223's owed run): measurement only, filed.

## Earlier goal (kiln 01a1af516c0577daac09ec09503f03ab): the Windows wake estimate (A-223 window rule; liveness plan)
Details of its notes were in the wiped file; its landed work is on main (git log: A-223, A-361, A-387).
