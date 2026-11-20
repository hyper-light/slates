# g229 -- progress (relaunched fresh 2026-11-19 ~22:40Z after an interrupt; report to main only on a landing or a block)

## QUEUE from main (2026-11-20 ~00:10Z, after A-688): each its own piece, one commit + merge, failing test first;
## after each landing a 3-line report to main and go on; END when context runs short (progress file first)
1. Commits over ra_svn: the connection's accepted commit work (svn:// AND the svn+ssh tunnel share A-656's
   connection) handed to A-655 part 2b's `repository::svn_commit::commit` on the repository's owner shard instead of
   refused 200007; test against the recorded svnserve commit sessions (crates/svn/fixtures/commit/).
2. Then the drop's exit difference (E210002/1 vs svnserve's 0): reproduce with stock svn on a commit really refused
   (a lock or an out-of-date base); fix only if it still differs.
NOT mine: the production SvnStores lender (g297's, shared by DAV, svn:// and the tunnel); moving the tunnel to its
repository's shard after the greeting (g234's svn:// lookup, made callable from the tunnel).
Disk care: `git diff --stat` for unexpected deletions before any commit (the MacBook disk is ~100% full).

## Now (2026-11-20 ~00:45Z): queue item 1 = A-698 (claimed), ra_svn commits through the owner's step -- IN PROGRESS
- Worktree ~/slates-work/g229-svn-commit, branch agents/g229-svn-commit, WIP ba264d42c (pushed) over main 8c7965bf4+:
  slates-svn: Work.locks (CommitLocks; drive sets it), serve::Uncommitted::Refused{code,message}; server:
  repository::svn_commit::commit_work(state,(principal,repo),work,now) + Uncommitted::answered(); tunnel:
  SvnStores::commit(id,principal,work) -> Option<Result<Committed,Uncommitted>>, Received::Accepted{work,..},
  SvnCommand::land (committed()/not_committed()), hook-configured words now "does not run this repository's hooks yet".
  Tests: tunnel a_stock_commit_lands_through_the_owners_step_as_svnserve_t_answered_it (mkdir replay, stock bytes
  but the author; lender lands on records' check_work+apply, NOW = the recorded date), a_work_the_owners_step_refuses_
  is_answered_as_close_edits_failure (lender full -> 160000 no room); step t_a698_a_clients_accepted_work_lands_through
  _the_step (audit_on_shard, CreateRepository, commit_work).
- Studio kiln r509 running: fmt; RED (perl stubs: commit_work -> NotHeld, land -> 200007); GREEN `--lib -- vcs_ssh::
  repository::svn_commit::`; slates-svn `commit:: serve:: migration::`; logs ~/.cache/g229-a698-logs.
- ~00:20Z r509: fmt clean; RED 52/55 (the 3 new failing); GREEN 55/55; slates-svn 115/115. Docs filled.
- ~00:40Z main moved 44 (A-652 part 3 dump import via the step, A-684 daemon SSH edge). Landing worktree
  ~/slates-work/g229-svn-commit-land, branch agents/g229-svn-commit-land: squash + resolution (svn_commit.rs doc;
  GAPS: main's A-684 then A-698; design log end main first) committed as 8baf30e2f (real message), pushed to that
  branch (NOT main). Studio kiln r533 testing that exact commit (fmt, vcs_ssh:: repository::svn_commit::, slates-svn).
  NEXT: if green: fetch; merge origin/main if moved (judge overlap); diff --stat (deletions!); push HEAD:main; report.
- (was) NEXT: docs (design status after A-688's, amendment A-698 at the log's end, GAPS line), one commit from a fresh
  branch or squash, merge, diff --stat, push; 3-line report; then queue item 2.

## Now (2026-11-20 ~00:05Z): svn+ssh, A-688 -- LANDED be892650b..8c7965bf4 -- END
- One commit de9909e40 (crates/server/src/vcs_ssh/svn.rs new: SvnServices/SvnCommand/SvnStores/TunnelBounds/Opening;
  tests/svn.rs 9 tests + tests/serve.rs e2e an_svnserve_tunnel_serves_a_stock_session_over_the_channel; site.rs doc;
  vcs_ssh.rs mod + owed; slates-svn handshake::Unserved::failure; design status under the svn+ssh bullet + amendment
  A-688 at the log's end; GAPS line after A-651 part 4 + A-656's owed) + merges aed39ae08 (A-651 part 4: tunnel_site
  now `impl SshSite<Keys = K>` for S::Keys: Send), 8e0a1609f (A-679 hg over SSH: one `site::not_served`, main's),
  8c7965bf4 (A-694, A-629 part 2c: nothing of vcs_ssh/svn/ssh; r458 stands). Diff vs main before push: 8 files, mine.
- Oracle: stock svnserve -t 1.15.0 (Homebrew, Studio): recorded mkdir commit (crates/svn/fixtures/commit) + scripted
  sessions recorded 2026-11-19T22:27:07Z and 22:33:56Z (first six pinned by SHA-256 in the tests). Scripts kept in
  ~/slates-work/g229-scratch/svnrec.sh, svnrec2.sh; notes svnrec-2026-11-19T22-27Z.txt.
- Evidence (macOS arm64, Mac Studio): red r311 `--lib vcs_ssh::` 26/35 (stub: tunnel sent to other services), green
  35/35; landing trees r377 38/38 (main 693bc8feb), r448 40/40 (f1fbd2c44), r458 47/47 (fc5f4ea98); slates-svn
  `handshake:: serve::` 60/60 each; fmt clean each. Run request filed 2026-11-20T00:00Z (Linux/Windows, lints,
  red-checks, live svn+ssh once A-684 + a lender exist). amendments.tsv A-688 = LANDED (backup .bak-g229-a688).
- Cleanup: Studio ~/.cache/g229-target (9.4G), clone, logs, recordings deleted (kept g229-lw-logs: lease_wait's owed
  evidence). Worktrees g229-svn-ssh and g229-svn-ssh-land removed; branches kept (agents/g229-svn-ssh pushed at
  4829efe91, scratch WIP; agents/g229-svn-ssh-land = 8c7965bf4 on main).
- Findings for main (not chased): (1) by use: a client leaving during a refused edit's drop ends E210002/1 here, 0 in
  svnserve (every stock svn+ssh commit today); (2) two SvnStores seams now (vcs_http::svn for DAV, vcs_ssh::svn for
  ra_svn): one production lender over A-662's records should serve both; (3) next pieces: that lender, and the
  tunnel's accepted work handed to A-655 part 2b's repository::svn_commit::commit; the tunnel's move to its
  repository's owner shard (A-651 part 4 moves a connection only for an exec naming a repository).

## lease_wait unit tests -- STOPPED at main's redirect (owed; branch kept)
- Branch agents/g229-lease-wait-tests = 759a8c11a (pushed; WIP, unbuilt) over the predecessor's 4084f4b62 on main
  4dca63df7; worktree ~/slates-work/g229-lease-wait.
- Found: 4084f4b62 never compiled: Studio r57's log (~/.cache/g229-lw-logs/lib.log there) shows E0599
  ReplyBody::from_bytes without slates_wire::Wire in scope (lease_wait.rs:467); fmt was clean. No test of it ever ran.
- 759a8c11a rewrites the module's tests: they drive verbs::run_forwarded (made pub(crate): the real completion check,
  join chain and run_recorded, not a hand copy); the lease refuses via OwnerLease::supersede(version + 1) and holds via
  ::installed; (1) confirm: parked, resolve x2, retry with the lease refusing again reads the recorded Status, served 1;
  (2) lapse, real time on a laptop audit_daemon(): a laptop spawns no coordinator (daemon.rs), so the module's deadline
  timer is the only resolver (fleet_progress() == 0 as the non-vacuity count); looks every HEARTBEAT/10 until resolved,
  AtItsDeadline vs Early vs NotWithin(LIVENESS/2 + scheduler_overrun, A-660's discipline); retry with the lease holding
  reads LeaseUnconfirmed, expired 1; (3) within its deadline it stays parked and a retry joins it (still 1 parked).
  The fleet test's assert message in assert_isolated_owner_lease ("on this connection") fixed too (3rd stale instance).
- OWED to resume: Studio fmt + `cargo test --locked -p slates-server --lib lease_wait:: -- --test-threads=4`; then a
  fresh branch from origin/main, cherry-pick, real message, merge, land.
- Sibling finding for main (not changed): deadline_timer's comment "A refused sleep (the timer wheel full) still resolves
  now" is stale since AUD-29-39 (2026-09-30): rt's Sleep waits for a free timer and is refused only off a shard.

# g229 -- progress (relaunched fresh 2026-11-19 ~19:15Z after the Claude Code exit; report to main only on a landing or a block)

## Now (2026-11-19 ~21:15Z): lease_wait unit tests (main's piece after A-601 continued (2)) -- IN PROGRESS
- Worktree ~/slates-work/g229-lease-wait, branch agents/g229-lease-wait-tests: work 4084f4b62 on main 4dca63df7
  (pushed). crates/server/src/lease_wait.rs gains `mod tests`: a_parked_read_runs_once_its_lease_confirms_and_only_once,
  a_parked_read_whose_lease_never_confirms_is_refused_lease_unconfirmed_at_its_deadline,
  a_read_still_within_its_deadline_stays_parked_and_its_retry_joins_it (audit shard; lease made to refuse by a
  superseded OwnerLease, confirmed by the default; deadline put behind the clock in the module's own tests; observed by
  parked(), join(), the served/expired counts and the completion record). Fleet isolated-owner test's doc fixed (each
  observation connects a client anew; no status read). Studio r57: fmt + --lib lease_wait::.
- Slip (no harm, verified): a first worktree command ran `worktree remove --force` and `branch -D` on a path/branch that
  did not exist yet; every g229 branch and worktree checked intact. Never again.
- NEXT: if green, land as one commit + merge; if a test fails, it is a bug: fix, claim in bug-claims.


## Now (2026-11-19 ~21:05Z): A-601, continued (2) -- the lease half -- LANDED cb9d7f5a5..4dca63df7 -- END
- main's piece (after a Claude Code restart at the usage limit): does a test prove a deposed owner past its lease never
  answers a read older than an acknowledged commit? Yes, by four tested rules read together: a holder defers its
  promise while its answers may feed the departed owner's lease (daemon.rs
  a_holder_defers_a_promotion_while_its_answers_may_feed_the_departed_owners_lease; lease.rs promotion-gate tests),
  every promotion quorum meets the lease's confirmations (lease.rs exhaustive oracle), the lease's bound sits inside
  the horizon and lapses one ns past it (lease.rs), and end to end the isolated owner's head reads are refused
  Refusal::LeaseUnconfirmed while its successor commits v4 (fleet.rs; a parked status served once the lease confirms).
- Landed: 3c05b3aca (docs only: amendment "A-601, continued (2)" + one sentence in §4.10's A-601 status) + merge
  4dca63df7. Evidence: macOS arm64, Mac Studio, r10 on main bb535bd4d: lib lease:: + the deferral 13/13; fleet 3/3.
  No markers tree-wide. Run request filed. amendments.tsv A-601 extended (backup .bak-g229-lease). Studio scratch
  deleted; worktree g229-lease-reads removed (branch kept).
- Observed, not changed (told main): the isolated-owner test's doc says its reads go "on that connection", but
  observe_taken_over_green opens a new client per observation; lease_wait.rs has no unit tests of its own (its
  park-then-refuse rule is covered only by fleet tests).


## Now (2026-11-19 ~20:50Z): A-601, continued (1) LANDED 34f655916..1fdebac50 -- END
- main's piece: whether a live-shipped volume can be taken over with no holders recorded, where live_ship's successor
  falls back to the configuration's candidates. Answer, by reading: it CAN be taken over so (owner lost before its first
  shipper's holders were placed: a volume just made live, a new live clone, whose record publish_clone_as clears), but
  then no log ever shipped (holders_placed), the takeover runs no live phase one (fleet::materialize gates on the
  catalog's live_holders) and serves the placed seal, and the successor's log begins new (planned: Adopted only from a
  live takeover's adopt_head), so the fallback names only a new log's holders. A successor of an adopted log keeps the
  recorded set (verbs::finish_making). Refusing such a takeover would strand A-577's seal-served volumes: main's step 3.
- Landed: cd5d278fc (live_ship.rs module doc "Whose holders a log counts" + the published() comment, which also named
  the test-only verbs::materialize_taken_over where verbs::finish_making keeps the record; GAPS answer; amendment
  "A-601, continued (1)") + merges bfb1b106a, 1fdebac50 (main moved twice; none touched the described code).
- Evidence: macOS arm64, Mac Studio, r215 on 5a92d4ec7 (main 22855c29e + the change): fmt clean; slates-server lib
  live_ship:: takeover:: + a_live_clone_is_answered_only_once_its_first_whole_entry_is_durable 38/38; fleet's 17
  live-shipping tests 17/17. Run request filed (Linux/Windows; cargo doc). bug-claims: ANSWERED line. amendments.tsv
  A-601 row extended (backup .bak-g229-lh). Studio scratch (5.5G target) deleted; worktrees removed, branches kept.
- QUEUED by main, not started (main confirms after this END): the lease side of A-601's goal: does lease.rs's test
  suite prove an owner whose lease lapsed refuses reads, typed, before any successor can commit? If a test proves it,
  cite it in A-601's amendment; if none, write it failing first, fix if needed, focused tests + fmt, land.

## Now (2026-11-19 ~20:40Z): A-601 LANDED 3274e7bff..c3c2c045f -- END
- One commit f59d1f3bc (crates/db/tests/live.rs + design §4.10 status + A-601 amendment + GAPS) on main 2dc39bd60,
  then merges 1c0212777, e1e2b67f6, c3c2c045f as main moved three times during the landing (each conflict the
  amendment log's end: main's entries first, A-601 last). Diff vs main before the push: 3 files, no markers.
- The check: Run::read_safety judges, after every step and every settling delivery while lost < f, every head phase one
  could adopt (f + 1 counted holders report no greater head); each such head's state must hold every durable write.
  The first violation is reported at the end of settle, after the other checks (mutation tests keep their messages).
  Counters: reads_judged, reads_past_a_stale_holder. Two red checks (a read adopting a later owner's chain without
  the write; a holder with no copy passed over).
- Red first: r150 on b3439756e (the check stubbed): 2 of 2 failed. Its green half then failed the newcomer test: 12 of
  400 histories (7 at f=1, 5 at f=2), every read counting a fresh member. Cause, in the MODEL: without `following` the
  successor's shipper named every known host, fresh members included, though the test's doc says no shipper does and
  the daemon's successor ships to the record's live_holders. Fixed in the model (successor = holders the superseded
  shipper committed; same list without newcomers, histories unchanged: r150 and r176 counters identical) + judge only
  within tolerance (lost < f, the nemesis's own takeover guard).
- Green: r176 on 05ebce515 (main 2dc39bd60 + the change): fmt clean; main nemesis test passed (judged 7,198 / 6,329,
  past a stale holder 3,721 / 3,035); 12 of 12 other focused tests. Main's later commits touch nothing slates-db builds
  from but slates-anchor's LAYOUT_VERSION, unused by live/register (said in the merge messages).
- Question for main (GAPS, amendment, run requests): live_ship.rs's successor falls back to the configuration's
  candidates when live_holders is absent; if a live-shipped volume can be taken over with none recorded, the daemon
  could name a fresh member as the model did.
- Run request filed (Linux x86_64/arm64, Windows x86_64; main's gates). amendments.tsv A-601 = LANDED (backup
  amendments.tsv.bak-g229-a601). Studio scratch (target 882M, logs, clones) deleted. Worktrees g229-a601 and
  g229-a601-one removed; branches kept (agents/g229-a601-read-safety pushed at 05ebce515; agents/g229-a601-one local).

## Now (2026-11-19 ~19:50Z): A-660 LANDED 1b480bd70..787f5948b -- END
- One commit 82542220c (deadlines.rs's tests + bug record + A-660 amendment with its Tests line) on main 2e4433040,
  plus the merge 787f5948b with main 1b480bd70 (one conflict, the amendment log's end: main's entries first, A-660
  last). Diff vs main before the push: 3 files; no conflict markers.
- Evidence (macOS arm64, Mac Studio, kiln r66, on a8c2fc09b = main 2e4433040 + the change, the landing's code):
  fmt clean; `cargo test --locked -p slates-server --lib deadlines:: -- --test-threads=4` 10/10, the status capture
  among them; the 19 integration tests that call Daemon::inject_failover, filtered by name, 19/19 (attachment_term 4,
  landing_fairness 1, fleet 12, slates-client reap 1 and client 1). Same counts on 3ed9ad19f (main b1926d201, r53).
  Main's commits 2e4433040..1b480bd70 touch none of the code these tests drive (the merge message says why).
- Earlier runs this session: r9 (main 0c7a77bd3 + the fix's deadlines.rs laid over) integration 19/19, but the lib
  did not compile: main's own break, PackRefused::Stalled (bf9d39139 18:40Z .. 93bd6e807), told main 19:25Z; r34
  (d0bf8417e) lib 10/10. Main then asked for the landing's own tree: r53, r66.
- Main's corrections this session (Ada's rule): run the FOCUSED unit AND integration tests (filtered), not --lib
  alone; evidence comes from the landing's own tree; no scratch patches.
- Run request filed (Linux x86_64/arm64 and Windows x86_64: Command 1 the lib filter, Commands 2 the 19 integration
  tests; main's gates). amendments.tsv A-660 = LANDED (backup amendments.tsv.bak-g229-a660).
- Cleanup: on the Studio ~/.cache/g229-target (11G), g229-target-base, g229-a660 (the predecessor's 3.3G), logs and
  clones deleted. Worktrees g229-a660-one, -final, -land removed; branches kept (agents/g229-a660-final pushed at
  a8c2fc09b, agents/g229-a660-land at 23696216c, agents/g229-a660-one = 787f5948b, local only). Kept:
  ~/slates-work/g229-base (main 4644dcec3) and the predecessor's notes in ~/slates-work/g229-scratch.
- Lesson: A-661's landing note said main touched nothing under crates/server, db, rt, ipc or wire since 4644dcec3,
  but slates-git (188314131) had changed beneath slates-server and broke its lib tests' build: check every crate the
  tested crate builds from (`git diff --stat <base> origin/main -- crates Cargo.lock`), never a chosen few.


## Now (2026-11-19 ~18:52Z): A-661 LANDED; A-660 BLOCKED on jupiter's disk -- END (main: no room = report and END)
- Main's word mid-task (CLAUDE.md §8, overriding the brief): only `cargo test -p <crate> --lib <filter>` for what I
  changed + fmt; A-660's failing test first stays; no extra red checks on adacachy; no reruns; a box with no room =
  report and END; land each alone on a green run.
- **A-661 part 1 LANDED bf9d39139..92ffda926** (one commit, from the fresh branch agents/g229-a661-land; the old branch's
  9e30cfa2f said "WIP, unbuilt"). Code byte-identical to the tested 2dc5e209e; main touched nothing under crates/server,
  db, rt, ipc or wire since 4644dcec3. Evidence (Linux x86_64 adacachy, capped units): r40 (predecessor's; logs kept in
  ~/.cache/g229-linux there) main's tree 624 passed / 2 failed (A-660's; the hangup flake), A-661 630 passed / 1 failed
  (A-660's), fmt clean; r118 `svn_hooks::` green 5/5, red four ways (each choice broken failed only its tests). Design
  §4.17.4 status, amendment Tests line and GAPS filled. Run request filed (macOS + main's gates). Hangup flake filed in
  bug-claims (18:50Z), a hypothesis only.
- **A-660 NOT landed.** Branch agents/g229-a660-land = 23696216c (d0bf8417e, the fix rebuilt clean on e3234e618 since the
  old branch had "WIP" commits, + merge with main 92ffda926: one conflict, the log's end, main's first, A-660 last;
  main touched nothing in deadlines.rs, daemon.rs, state.rs, fleet.rs or rt since). Worktree ~/slates-work/g229-a660-land.
  jupiter r102 (17:17-18:40Z): fmt clean; the build type-checked slates-server (3 warnings, main's items) and died
  writing its test object: "rustc-LLVM ERROR: IO failure on output stream: no space on device"; C: 100% (1.5 GB free);
  the red swap's fetch then failed "No space left on device". My clone, target (5.9G) and logs there deleted.
  NEXT (when main restarts it): on a box with room, clone agents/g229-a660-land, `cargo fmt -p slates-server -- --check`
  and `cargo test --locked -p slates-server --lib deadlines:: -- --test-threads=4` (expect every deadlines test
  passing, the status capture among them); fill "- **Tests** @@TESTS@@" (A-660's amendment, the log's last entry) and
  the bug record's Evidence with it; fetch, merge origin/main, diff --stat (3 files), markers, push HEAD:main; file the
  macOS run as owed. Red is already on record: Studio 10/10 (main's file), Linux r40 at 4644dcec3 (deadlines.rs:830),
  Windows main's baseline r1485 at 6e42e7a2c.
- Also stopped on jupiter at ~17:12Z: the predecessor's two orphaned runs (a 15:55Z synced run and r33), both g229's,
  by root PID; they held ~/.cache/g229-target-green's lock and the older would have rm -rf'd it under r33.
- amendments.tsv rows A-660 and A-661 updated (backup amendments.tsv.bak-g229).

# g229 -- progress (relaunched 2026-11-19 ~16:20Z after the usage limit; history below unchanged)

## Now (2026-11-19 ~16:25Z): A-660 then A-661, each landing alone -- RUNS GOING
- Readback of THE RULE sent to main. Worktrees checked: ~/slates-work/g229-a660 and g229-a661 are linked worktrees of
  the standalone partial clone ~/slates-work/g229-base (never ~/Projects/slates); both clean, pushed, merged with main
  4644dcec3 (= origin/main at relaunch). No conflict markers.
- A-660: e57715af6 (doc rewrap only: one doc line ran 187 columns) on 4f4bf3767, pushed. jupiter run **r33** (kiln
  restarted, ids from r1): fmt + `cargo test --locked -p slates-server --lib -- --test-threads=4`, failures compared
  with main's 23 (windows-server-baseline.txt), clone + target deleted at the end. Reused the predecessor's partial
  target name ~/.cache/g229-target-green (r1501 never reached slates-server). jupiter C: is 99% full (15 GB free).
  NEXT: fill "- **Tests** @@TESTS@@" (design A-660 entry) with Studio red/green + r33; bug record's evidence; commit;
  fetch + merge origin/main; diff --stat; markers; push HEAD:main; file macOS run as owed.
- A-661: 2dc5e209e (tests bound by within(10 liveness windows), as hook_driver's) on 1982342fe, pushed. adacachy run
  **r40** (`on` adacachy: kiln's `needs: linux/amd64` placement refused 3 times "has no room here now: it isn't
  started"; `on` works): one clone of the branch; first its compiled tree set to main 4644dcec3 (main's lib.rs, no
  svn_hooks) = Linux baseline, then my files back (only slates-server recompiles); both `--test-threads=4`; in a
  systemd-run --user unit MemoryMax=24G CPUWeight=1 Nice=19 IO caps nvme1n1 150M/400M RuntimeMaxSec=4h, TMPDIR on
  disk under ~/.cache/g229-linux. NEXT: fill 3 @@TESTS@@ (design status, amendment, GAPS); land after A-660.


## Now (2026-11-19 ~07:40Z): A-660 then A-661 -- IN PROGRESS, the Studio offline (main's rule: no builds elsewhere)
- A-660 (deadlines tests): branch agents/g229-a660-deadlines-failover at c807195c2, worktree ~/slates-work/g229-a660.
  Fix: both tests inject the delay via daemon.inject_failover; look_until widened by the shard's scheduler overrun and
  a miss reports it (Result<Duration, String>); bug record docs/bugs/2026-11-19-the-deadlines-tests-wrote-the-failover-
  delay-the-beat-overwrites.md; amendment A-660 with "- **Tests** @@TESTS@@" to fill. Evidence so far: red (main's file)
  status capture 0/10, green 10/10 each alone, module 15/15 under two suites; 1 miss of 11 full runs (attachment+lease,
  load 14.7) not reproduced in 12 since. OWED: the final run on c807195c2 (r1209 was stopped when the Studio went OOM):
  CARGO_BUILD_JOBS=4, one cargo at a time, no double suites; then fill @@TESTS@@, merge main, land ALONE first.
  LESSON: never select a test binary by mtime after a red rebuild; one target per checkout.
- A-661 (svn hook calls): branch agents/g229-a661-svn-hook-calls (9e30cfa2f code, docs after), worktree
  ~/slates-work/g229-a661: crates/server/src/svn_hooks.rs (+ tests.rs: 3 pure + 2 end-to-end via hook_driver roles),
  lib.rs mod (cfg unix). Unbuilt. Docs drafted with @@TESTS@@. Call points APPROVED by main: g268 adds Step::Hook,
  Due::{StartCommit, PreCommit}, Drive::resume; g234 runs hooks via svn_hooks + post-commit after append; DAV LOCK/UNLOCK
  (g298's DAV code, not on origin yet) are mine to wire next. Land after A-660.

## Now: UI goal 93, A-648 = the hook points' gaps A-623 found (main's assignment) -- LANDED, END
- LANDED cf9fe4c25..a2f20ce13 from agents/g229-a648-svn-hook-gaps (135d358c0 code + tests, docs commits, merges
  de9dc5cbf and a2f20ce13 with main's A-642/A-644/A-625/A-652).
- Record: slates-db's and the protocol's HookPointName gain Subversion's six points (tags 8..13, both wire_order.txt
  published); RepositoryPolicy::hooks_env + SetRepositoryPolicy::hooks_env appended (HooksEnvSection/Variable);
  guard refuses a section no hook env can carry or any section outside Subversion (DbError and DatabaseRefusal
  PolicyHooksEnvMalformed, tag 11: A-642's Moved took 10 first, resolved at the merge); sections charged.
- Driver: Subversion hooks run in "/" (HookPoint::directory); pre-lock's stdout is a second pipe handed back
  (hook_driver::Ended::standard_output) and read by svn_lock_token (C string to first NUL; empty = server's token;
  PastCap); SvnHooksEnv::of(policy) builds the env from the record; HookPoint::of maps record points.
- Studio: db lib 142/142, ipc lib 74/74, both wire_order 2/2, server lib 621/622 on the merged tree; red 11 ways.
- Found (not mine): server lib test deadlines::tests::a_status_capture_is_held_the_failover_delay_from_its_capture_
  not_its_request fails 5/5 here and 3/3 on a clean clone of main 3239c88 (deadlines.rs:830): reported to main.
- Lesson: two checkouts sharing one CARGO_TARGET_DIR read each other's artifacts (cargo hashes path packages relative
  to the workspace root): the red run against the main clone's target compiled slates-server against main's ipc. One
  target per checkout, always.
- svnlook view decided by main (read-only FSFS-layout view from A-645's derived data, after A-647; queued).
- Owed (design §4.17.4 status, GAPS): the FSFS view; the server's calls (commit, lock, unlock, revprop) incl. refusing
  a PastCap token; a dangling link told apart; no core dumps; the Windows driver.
- Studio scratch deleted (~/.cache/g229-a648*); worktree removed (branch kept); run request filed.

## Now: UI goal 93, A-623 = Subversion's hook points (Ada's scope cut 2026-11-19: the server only, no svnlook) -- LANDED, END
- LANDED 9e514ef36..d449a10c7 from agents/g229-a623-svn-hooks (f1a9cd0be the points' inputs, 0d194ab74 the recorder,
  25f1cde86 svnserve's words + fixtures, c463c6f16 docs; merges 06aa369e6, 5faf7827d, 20da28565, d449a10c7 with main's
  A-622 part 3, A-643, A-613, A-626).
- `crates/server/src/vcs_hooks.rs`: Subversion's nine points (start-commit, pre/post-commit, pre/post-revprop-change,
  pre/post-lock, pre/post-unlock); every argument a C string ending at its first NUL; LOCK-TOKENS lines with the
  client's own (relative) path escaped by svn_path_uri_autoescape's ten bytes; SvnHooksEnv (own section, else default,
  else nothing; repeated sections merge, an empty one is none); `svn_told`/`svn_told_unconfigured`: svnserve's words
  per outcome (E165001 check_hook_result texts, signal "did not exit cleanly", exec failure = exit 255 no output,
  E165002 for no room, E165008/E165009 wrappers, post-commit-err, E165006); Outcome::TimedOut keeps its reaped status.
- `crates/server/src/hook_driver.rs` (g255's A-622 part 3, landed while I worked): env_clear() when
  HookInput::clean_environment, with a re-invoked-binary test; its test helper names the field.
- Evidence: `crates/svn/fixtures/hooks/make-fixtures.py` (stock svnserve 1.15.0, Studio; 33 cases, 64 hook runs, every
  byte sent; 325 files, SHA-256). Studio: `cargo test --locked -p slates-server --lib -- vcs_hooks hook_driver` 37 of
  37, fmt clean; red 11 ways (each guard broken in a copy failing only its tests).
- Found: A-597's pre-commit wrote LOCK-TOKENS + blank line with no token and its test asserted it (svnserve: empty);
  token lines carry the client's relative path; the driver applied env over the server's environment.
- svnlook rebuild stopped at main's word; its recordings + script on the UNMERGED branch agents/g229-a623-svnlook
  (a4d07e4b5).
- Owed (design §4.17.4 status, GAPS): HookPointName's six new points (A-622 wire tags, g255); a hooks-env the owner can
  set; driver: cwd `/` vs its view root and what REPOS names (stock `svnlook -t TXN REPOS` needs an FSFS repository
  there: Ada's decision), pre-lock's stdout as the token (sent to null now) and a bad token refused (E160037,
  recorded), a dangling link told apart (recorded), no core dumps; the server's calls; Windows svnserve's words.
- Run request filed (clippy, xtask, cross-target lints over vcs_hooks + hook_driver). Studio scratch deleted
  (~/.cache/g229-a623{,-rec,-target}); local scratch deleted; worktrees removed (branches kept).

## Now: UI goal 92 (51cef9f7), A-606 = Subversion piece 10, authorization -- LANDED, END
- LANDED on main 0d21b12f6..bceebaea8: f18a86ffc (tests first, module stubbed: red 50 of 55, the 5 passing only
  no-panic/parse-only asserts), 6f141033b (the module), 589902437 (rich-set oracle test, header-bracket test, design
  §4.17.4 item 10 status + pieces, GAPS, research §7), 332b3928a (A-606 amendment), merges a48913e11, b13226f2d,
  bceebaea8 (conflicts only lib.rs's mod list/doc, GAPS §4.17.4, design pieces list + amendment log end; main's first).
- Module: crates/svn/src/authz.rs (API: Authz::parse(rules, groups, Bounds) -> Result<_, AuthzRefused{file,line,kind}>,
  global_rights, rules(repository, Principal) -> PrincipalRules{access, access_beneath, access_anywhere, uniform,
  uniform_answers}; RepositoryPath), authz/ini.rs (svn_config__parse_stream), glob.rs (apr_fnmatch/_test port,
  parse_rule_path classes + ** normalization), model.rs (constructor callbacks, post-pass; groups never expanded),
  tree.rs (create_user_authz incl. issue-4762 choice, trim_tree, finalize, lookup walk, global rights).
- Evidence (Mac Studio d1323898, kiln): `cargo test -p slates-svn --lib authz` 57 of 57 (2.1 s) on 589902437;
  `cargo test -p slates-svn --lib` 235 of 235 on the merged b13226f2d (main's later commits changed nothing in
  crates/svn); cargo fmt --check clean every run; no compiler warnings. Golden vectors: authz-test.c (+ authz.rules CRLF
  byte for byte, authz.groups), repos-test.c's authz tests (incl. commit_editor_authz as item 10's questions, issue
  4060), APR testfnmatch.c at flags 0, svnadmin's conf/authz, the book's examples, 1.10/1.14 notes. Naive oracle:
  256 generated sets x 4 repos x 6 principals x 156 paths, + a rich hand-built set proving non-vacuity.
- Departures from 1.14.x's code (documentation followed; each a named test; trunk fd4e82b same): (1) "/" walked as
  one empty segment so [:glob:/*] decides the root; (2) recursive "/" weighs only empty-segment rules -> grants
  recursive read above a denied /secret (looks like an authz bypass in svn itself via a copy of "/"); (3) suffix
  match reverses the segment in place for the level's other nodes; (4) [//x] taken for the root. Decided one way:
  group cycles anywhere refused; NUL and empty option names refused; first refusal in file order.
- Run request filed (this session's timestamp): clippy -p slates-svn --all-targets + xtask; the four departures vs
  stock svn (svnauthz built against brew's libsvn, or svnserve probes), cases (a)-(d) with predicted answers.
- Owed (main starts each): the server's questions (piece 7), the authz file read from the repository per file
  identity, force-username-case, mod_authz_svn's questions (piece 13).
- Studio scratch ~/.cache/g229-a606 (757 MB) deleted; local scratch deleted. Worktree ~/slates-work/g229-a606-svn-authz
  (branch agents/g229-a606-svn-authz = main bceebaea8).

## A-606 follow-up (main's word, 2026-11-19): the svn finding is Ada's -- out of the tree, nobody verifies or reports it
- LANDED 22e1612fb..cd4ea4d66 (e9954ea10 + merges 08f4d0fc0 and a clean one): the recursive-root finding removed from
  the module doc, tests (test renamed a_recursive_question_at_the_root_weighs_every_rule, /private not /secret),
  design status + A-606 amendment, GAPS and research §7; departures now three, cross-references renumbered.
  Evidence: slates-svn lib 236 of 236 + fmt clean on 08f4d0fc0 (Studio); main's later commits touched no crates/svn.
- Still in git history (not rewritten, banned): 589902437, 332b3928a, f18a86ffc's test doc, merges a48913e11,
  b13226f2d, bceebaea8. Run request case (b) withdrawn. The suffix-reversal departure (still in the tree, stated
  neutrally) can also change what svn grants; told main, Ada's call.

## A-601 (goal 81 dbe13f39, ReadSafety in the live-log nemesis): PARKED, not started
- No worktree, branch or commit exists for it (checked 2026-11-19: ~/slates-work/*601*, every g229 worktree clean,
  no agents/g229-*601* branch); amendments.tsv still says "assigned". Next step when resumed: ReadSafety as a history
  check in `slates_db::live`'s nemesis (crates/db/tests/live.rs checks Agreement, NoLoss, StaleNeverCommits,
  Convergence only), as a lib test, following register_nemesis.rs's FencedLedger per-position check.


## Now: UI goal 85 (kiln 17ad60b6), bug 4 = A-489 (main's assignment)
`a_location_round_asks_a_peer_whose_session_was_out_once_it_returns` (crates/server/tests/fleet.rs) fails intermittently
with `Refused { OwnerUnreachable { region: 0 } }` (quiet Studio at main 3022c6894 and 2a5bf7264; predates A-437; passes on
rerun). Failing run's location counters at the foreign node: round 58, retried 56, refused_by_peer 116, not_owner 58,
unavailable 58, session_out 1, budget_spent 1, gave_up 1; the successor counted owned_unmaterialized 1.
Plan: read owner_location.rs + A-437's round budget; find why every round met not_owner/unavailable until the budget ran
out; reproduce deterministically as a server `--lib` test (failing first, red once); fix; design/GAPS lines; --lib tests,
fmt, clippy -p slates-server --all-targets -D warnings, xtask check (Studio d1323898); merge; push; END.
- [x] started 23:50 UTC; told main.
- CAUSE (from main's r277 logs on the quiet Studio 2768d0d5, ~/.cache/slates-gate/fleet-q1.log and fleet-base.log): the
  successor counted `fleet.owner_location.root_view_differs` 58 (68 in the base run): it refused every round because
  the foreign node's query is bound to a newer root version than it held. The foreign node is region 1's only member,
  so a root voter on the newest root; region 0's representative (lowest host id, also a voter) answered "not mine"
  under the same version; the successor and one other learner were behind. A root learner fetches only when its
  regions or known voters look wrong (`fleet::root_diverges`), and a seat change (A-406, in both failing commits)
  moves the root version with neither, so a learner left behind at formation never fetched. The owner (highest host
  id) is not the representative, so its death changed no voter.
- LANDED 7267db674..7e07579d8 (A-489 = 76fdca5d0, merge 7e07579d8; one conflict, the design log's end, main's A-472,
  A-463, A-485, A-488 first). `owner_location::serve` takes the asker; a query bound to a newer root records
  (version, asker) in `ShardState::root_newer_seen` (counted fleet.owner_location.root_behind); `fleet::root_learner_fetch`
  (extracted from drive_root_group's learner branch) fetches when it names a newer version, and `with_shown_peer` asks
  that asker beside the known voters. Docs: §4.8 owner-location status + learners, A-489, GAPS, bug record
  docs/bugs/2026-11-18-a-root-learner-behind-its-asker-refused-every-location-round.md.
- Evidence (Studio d1323898, 76fdca5d0): owner_location::tests 7/7, root_learner_tests 1/1, daemon placement test 1/1,
  fmt clean, `cargo clippy -p slates-server --all-targets -D warnings` clean, `cargo xtask check` nothing in crates/server
  (45 literals elsewhere, routed). RED once: newer-root record removed -> periods [None, None, None] vs [None, Some(R),
  None]. After the merge: main's changes in my files were a visibility change, a doc fix, a VolumeSlot field; no rerun
  owed. Studio scratch deleted.
- Run request filed 23:48Z: the fleet test x20 on the quiet Studio, then Linux, + the other location flows.
- The converse: main assigned it as A-504 (below). DONE.

## A-504 (UI 85, bug 4's converse): LANDED 3d174a1cb..3ab09886c (A-504 = 5113f0a44; three merges with main, each
conflict only the design log's end, main's A-497, A-490, A-484 first)
- A refusal names the root the refusing peer holds (`owner_location::Refused`, 8 bytes, sent by `owner_location::answer`
  for every refusal); an asker reading one naming a newer root counts itself behind (fleet.owner_location.asker_behind)
  and records it as A-489's peer side does, so its root learner fetches from that peer; each look of a lookup asks
  under the root held at that look (`Query::under_held_root` in `verbs::locate_within_horizon`). No wire_order-guarded
  enum changed (they cover ipc's protocol and db's records), so nothing published; the located reply's bytes unchanged.
- Evidence (Studio d1323898, 5113f0a44): owner_location::tests 9/9 (a_behind_askers_round_is_answered_after_one_fetch,
  a_refusal_names_the_peers_root_and_no_reply_reads_as_one new), root_learner_tests 1/1, daemon placement 1/1, fmt and
  `cargo clippy -p slates-server --all-targets -D warnings` clean, xtask structural ok and no literal in crates/server
  (6 elsewhere). RED once with the refusal sent empty: left None, right Some(Refused { root_version: 1 }). Main's merged
  changes touched none of my code files: no rerun owed. Studio scratch deleted.
- Run request filed: the location flows + the location test x20 on the quiet Studio and Linux.
- Worktree ~/slates-work/g229-a504 (branch agents/g229-a504-refusal-root = main 3ab09886c).
- DONE; stopped (END).

## Done earlier on goal 78 (kiln 01a1af515aa6705aac9a15614e0205ca, chaos across real processes)
- A-401 part 1 LANDED (07813ec55, merged fd1fa07cb): a live log's candidate reserves the restore's claim at the first
  entry (NoRoom typed), the held log is charged against it, the restore takes it over (A-397 carried it into the turns).
- A-401 part 2 LANDED 590e1a300..5c376e543 (c92cca49e + predecessor WIP 0b6e80bf7, 115e6a119, 4071b693c): `live-ship`
  asks every candidate to admit before recording the flag, refused BudgetExceeded{least room}; asks answered by the
  partition, awaited in shard time (A-408), late answers dropped. Units on e5fca080d: live_ship::tests 17/17, claim 5/5.
- Bounded kill-at-ack chaos flow landed with it. Run requests filed 2026-11-18T20:03Z (lost in the wipe? re-file if main
  has not confirmed): the fleet test a_live_ship_a_candidate_cannot_admit_is_refused_and_ships_once_it_can + red-check on
  590e1a300; live_chaos a_bounded_claim_is_served_by_its_successor_or_refused_before_it_ships on macOS x3 then Linux.
- Owed on goal 78 (main starts each): (1) the control shard's claims view can name a claim its partition released
  (claims_kept keeps only copies holding entries) -- recorded in A-401 + GAPS, failing test first; (2) a candidate that
  joins later and cannot admit; (3) placed seals' holders reserve the claim; (4) a re-enabled live volume's recorded
  holders are not asked; (5) A-345's register half; A-334's yield across a warm restart.

## Clones
- ~/slates-work/g229-chaos-across-real (branch agents/g229-chaos-across-real-chaos, = main 5c376e543 + nothing).
- ~/slates-work/g229-a489: removed after landing (A-489 on main 7e07579d8).
- ~/slates-work/g229-a504 (worktree, branch agents/g229-a504-refusal-root = main 3ab09886c; landed).

## A-510 (UI 85: a daemon must stop promptly on SIGTERM): LANDED d50fac5b9..d63e602a7 (guard test + bug record; no
product change, since Slates' stop did not reproduce the delay)
- Report: kiln's K5 real-instance test (g267, r407) sent `kill -TERM <anchor>` and returned; the instance was seen serving
  ~2 min later; waiting for the stop (r443/r457) it ended in 204/176 ms. No log kept (the anchor's log was in the test's
  TempDir).
- Read: log lines never panic on EPIPE and kiln logged to a file; the anchor leads its own group; the anchor's
  stop_step kills an overdue daemon within its own bounds and its poll wakes on the signal; the daemon's stop asks no
  client anything (drain, give_back_replicas up to OBSERVE_BUDGET, runtime shutdown, log writer).
- Reproduction (`crates/api/src/role/stop_tests.rs`, the liveness tests' roles): a sender starts the anchor in its own
  group, connects (then also: creates a volume, attaches for write, writes, holds the attachment), sends SIGTERM and
  exits at once (process::exit, no destructors); the test watches the pids alone. Studio: 283 ms (bare client), 191 ms
  (attachment held), 251 ms on the merged tree. Not reproduced.
- Left (in the bug record docs/bugs/2026-11-18-a-stop-asked-by-a-sender-that-exited.md and GAPS, open): what kiln's run
  had beyond it (kiln's executable as role host, launcher stdin, log file, K5's captured clones, the TempDir removed
  right after the signal with the clones' base, kiln runner's leftover-process handling). Evidence run filed: kiln's K5
  fire-and-exit with the log kept + ps/sample; plus the stop test on Linux and the lints I may no longer run.
- Studio scratch deleted. Worktree ~/slates-work/g229-a510 (= main d63e602a7). DONE; stopped (END).

## Goal 81 (dbe13f39), part 01a1af51 "Chaos across real processes", the network half (g250 has the failure half)
- LANDED 8a10459cb..57d25dd88 (798c647f8 + merge): `crates/db/src/register_nemesis.rs`, lib tests. The real `Acceptor`
  and owner rules over a simulated network: latency/reorder, loss, duplication, one-way/two-way cuts, holders offline and
  back from persisted state, owners paused/resumed, takeovers (new generation) and owner restarts (same generation).
  History checks after every accept/commit/adoption: Agreement (TotalOrder), Continuity (NoLoss), StaleNeverCommits,
  ReadSafety (FencedLedger per-position). 48 seeds x f=1,2 x 1,500 steps: none broken, every fault/stale/late case
  counted. Mutations kept as tests: promise without fence (39/48, 44/48), restart forgetting fence (16/192, 9/192),
  promotion on a minority (23/48, 7/48); ReadSafety's direct red check. Found: the fence is load-bearing only across an
  owner's restart under one generation (with takeovers alone neither fence mutation broke a seed).
- Docs: SLATES_DESIGN §4.8 "Promotion and takeover" status; GAPS new entry.
- Next missing (owed, main starts): ReadSafety in the live log's nemesis (`slates_db::live`; its tests/live.rs nemesis
  checks Agreement, NoLoss, StaleNeverCommits, Convergence only) as a lib test; real-process live_chaos runs (filed).
- Studio scratch deleted; worktree ~/slates-work/g229-nemesis (= main 57d25dd88). DONE; stopped (END).
