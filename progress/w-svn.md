# w-svn -- progress (UI goal 93, "The Subversion server built"; supervisor slates-7a)

## Piece 1: A-695 svn:// lookup -- IN PROGRESS (started 2026-11-19 ~20:05 PST)
- Took over g234's scratch: worktree ~/slates-work/g234-svn-lookup, branch agents/g234-svn-lookup (pushed).
  Predecessor: red c0d6b161f (r477), lookup cef1f7077 + locate bfa5eb06e, docs 1efae5e9f (r508 green), merge bcd221301.
- Merged origin/main b63f8a05d (A-681's lender) as 321ba10b9 (design log end: main first, A-695 last).
- b02491a61: vcs_svn::held reads the opened repository's records through vcs_store::svn_view::lend_in (the landed
  lender); docs (module doc, GAPS, design status + entry) name it; per-call sources stay owed until A-692 (no session
  authenticates before CRAM-MD5 keys exist).
- Studio run r1 (clone ~/.cache/wsvn-a695, target ~/.cache/wsvn-target): fmt, svn find:: report::tests::, server
  vcs_svn:: daemon_edge:: vcs_http::svn:: svn_view, cli vcs_svn svn_listeners.
- Next: green -> landing branch from origin/main (one commit: cherry-pick -n of the piece), merge, diff stat, push
  HEAD:main; delete Studio scratch; report; then piece 2 (g229 brief).
- A-695 LANDED main a3705be03.

## A-692 CRAM-MD5 -- LANDED main 05ae5fc4b (g307's branch agents/g307-a692-one + merge of a3705be03; Studio r3 green)
## Next: A-693 ra_svn lock verbs (g307 brief). Studio clone ~/.cache/w-svn-clone, target ~/.cache/w-svn-target.
## A-693 part 1 (get-lock, get-locks) -- LANDED main c8caa9ba3 (one commit; Studio r4 svn reads:: serve:: 71/71, fmt)
- Worktree ~/slates-work/w-svn-a693-land (from g066-fs-server .git), scratch branch agents/w-svn-a693 (pushed).
- Next: A-693 part 2 (lock/lock-many/unlock/unlock-many: LockTable::lock/unlock decision -> LockEntry through the
  owner's step; serve.rs Turn for the caller to apply), then piece 4 per queue.

## A-693 part 2 -- NOT STARTED (w-svn ended ~20:45 PST, context long); plan for the next agent
- svnserve 1.15.0 handlers: ~/slates-work/g234-svn-src/1.15.0/subversion_svnserve_serve.c lock 2954, lock_many 3034,
  unlock 3193, unlock_many 3225 (write_lock already ported in crates/svn/src/reads/locks.rs::lock_value; make it
  pub(crate)).
- Slice 2a (slates-svn, sans-io): serve.rs `elsewhere()` takes the four verbs: parse (lock `c(?c)b(?r)`, lock-many
  `(?c)bl` of `c(?r)`, unlock `c(?c)b`, unlock-many `bl` of `c(?c)`), must_have_access(write, needs user unless
  break) as commit asks Write (handshake::Server::for_command), then Turn::LocksAsked{consumed}; caller calls
  Connection::lock_verb(sources, draw) -> Decision entries (LockTable::lock/unlock, HeadView over the index as
  serve/tests/commit.rs's Head), applies them, then Connection::locks_applied(ok, out) writes svnserve's answers
  (lock: `( success ( lock ) )`; -many: per path `( success ( lock ) )`/failure in the client's order, `done`, then
  `( success ( ) )`; per-path authz refusal 170001). Test: serve/tests/commit.rs's lock-bob replay goes past
  lock-many (today Ending::Unserved there; take_lock then becomes the connection's own), locked-by-other too.
- Slice 2b (server): a lock-entries step on the owner (records.apply_lock_entries is the store's; no server path
  exists yet: grep apply_lock_entries crates/server finds none), the svn:// listener and the tunnel calling it.
- Pre-lock/post-lock hooks: owed (svn_repos_fs_lock_many runs them).
- Worktrees: ~/slates-work/w-svn-a693-land (landed, clean), ~/slates-work/g307-a692-one (landed, clean). Studio
  clone ~/.cache/w-svn-clone + target ~/.cache/w-svn-target kept warm for the next w-svn run.
- Unfinished from the queue: piece 2 (A-698 ra_svn commits): g229's landing branch agents/g229-svn-commit-land
  54603a1d4 (one commit 8baf30e2f + merges up to b63f8a05d), its run r533 result unknown -- merge main, one focused
  run (vcs_ssh:: repository::svn_commit:: + slates-svn commit:: serve::), push.

## A-692 continued (1): svn:// CRAM-MD5 wiring -- LANDED main eb1d50a33 (commit 2dae785bd + merges)
- Listener offers CRAM-MD5, keys from cram_md5_key, principal = token's; per-call sources via lend_in; write access
  where vcs_policy::reaches; most_open = item_bytes/2+1. Test a_stock_client_signs_in_with_an_enrolled_token_on_
  either_shard_and_reads. Local (own target ~/.cache/w-svn-local-target): server vcs_svn:: credentials:: 9/9.
- Note: main's crates/server/src/vcs_ssh/tests/receive.rs fails rustfmt --check (not mine).
- NEXT: A-698 ra_svn commits through repository::svn_commit::commit (g229's agents/g229-svn-commit-land 54603a1d4 has
  the tunnel half; the svn:// listener's CommitAsked/Hook/Accepted turns still end the connection), then un-ignore the
  svn:// interop leg in crates/cli/tests/svn_interop.rs if it passes.

## A-698 ra_svn commits -- LANDED main ddbd387a2 (one commit 7bedc3a88 + merges); svn:// listener lands via commit_work
- Test vcs_svn::tests::a_stock_commit_over_svn_lands_through_the_owners_step. Local own target 21/21.
- NEXT (main): w-perf's import bug: `slates repository import` of git-5000.dump refused TableAtCapacity green_chains
  after 1,685 revisions (RSS 3.9 GiB): derive the table bound per repository, bound the import's memory per slice;
  failing test first. Then re-check w-perf's svn commit repro (svn_scale.sh, branch agents/w-perf-svn).

## green_chains import bug -- LANDED main 9ea6e1311 (commit f37a96515)
- svn_commit::land: checkpoint_green after each revision; at the cap, Doubling::Now + append again. Test
  subversion_revisions_fold_their_greens_chain_as_they_land red (no checkpoint after 256) then green; local 12/12.
- Import memory: svn_import is already sliced per turn and charged as retention; the 3.9 GiB RSS was probably the
  unfolded chain (not verified: w-perf's svn_scale.sh repro is main's to run).
- NEXT in queue: A-693 part 2 (held by main), then DAV pieces (g297 brief).

## svn+ssh in the daemon's SSH site -- LANDED main a14635977 (commit fc79d36ae, verified ancestor of origin/main)
- svn_view::TunnelRecords (vcs_ssh::svn::SvnStores), daemon_edge::daemon_site. Test the_daemons_ssh_site_serves_
  svnserve_t_and_lands_a_stock_commit; local 16/16.
- NOW: green_chains import bug NOT fixed by f37a96515 (w-perf rerun on 9ea6e1311: still refused at revision 1,685,
  RSS 3.4 GiB). Suspect: the repository log entries (each revision's record + contents, EntryParts.objects) are
  charged to the partition's merge budget (green_chain_bytes) and never fold. Investigating.

## A-714 content charged per repository to the shard -- LANDED main b705a24c5 (commit 5ddb7e09b, verified on origin/main)
- db: objects no longer charged to the merge budget (Repositories::objects_bytes); server: vcs_store::settle_objects
  (after append, at make/recovery, credited at free), admit_objects for svn revisions only. Local: server 105/105 then
  61/61 after merge, db 29/29, fmt clean. A-716 (w-perf) takes svn texts held once.
- Ledger rule now: A-number is mine only after pushing its line on agents/fanout-state (next free A-720).
- NEXT: A-693 part 2 (svn:// lock verbs; interop E170003), then DAV pieces (g297 brief: the two HTTP rows, A-681).

## A-693 part 2 PARKED (main: the wedge bug first) -- branch agents/w-svn-a693p2-wip (pushed), worktree w-svn-a714
- Written, unbuilt: commit/record.rs Record::Locks (tag 4) + records.rs commit_record applies it; svn_commit record
  step handles Locks (check_entries); serve/locks.rs (lock_verb/locks_landed, Pending, words, read, answer);
  reads/locks.rs with_lock_value; may_write, full_path, lock_error_message made pub(crate).
- TODO: serve.rs: `mod locks;`, Phase::LockAsked/Locking (+Debug, waiting, session, receive out-of-turn, established
  arms), Turn::LockAsked, Answered::LockAsked in elsewhere/command; vcs_svn Next::LockAsked -> lock_verb + record +
  locks_landed; tunnel: answer LockAsked as unserved (210001); tests vs commit/lock-bob session.

## NOW: wedge bug (main): green advanced but svn record rolled back ("record 3560 not published ... profile exceeds
## region"), then every commit E160004 Diverged. Fix: one atom. Worktree ~/slates-work/w-svn-atom from origin/main.

## A-721 wedge fix -- LANDED main 0cb5120b1 (commit 3169466fe, verified on origin/main; ledger A-721 pushed bb4bb3a)
- verbs::reconcile_unpublished_effects -> remake_greens_ahead (lone engine / single-member lineage replayed from
  checkpoint+chain; wider lineages counted green_ahead_kept, owed). Test a_revision_refused_at_publication_...
  red then green; local 10/10 + 21/21, rechecked after the final merge.
- DAV pieces (A-681 rows) moved to w-interop by main. NEXT: A-693 part 2 from agents/w-svn-a693p2-wip (worktree
  ~/slates-work/w-svn-a714).

## A-693 part 2 lock verbs -- LANDED main 576a8a860 (commit 64b6f79c3, verified on origin/main)
- serve::locks (lock_verb / locks_landed; auth-for-one-command via Interrupted::Lock), Record::Locks (kind 4),
  svn:// listener + tunnel (SvnStores::land_locks, TunnelRecords). Tests: lock-bob and lock-a byte for byte; server
  landing+rebuild test; listener lock test. Local: svn 239/239 -> 164/164 merged, server 21/21, 32/32, 16/16 landed.
- Disk fell to 17 GiB: deleted my target (now 59 GiB free).
- NEXT (main): S4 with w-perf -- svn records name texts by identity, read from the green's content-identity index
  (held.rs hold_path); agree the seam with w-perf. Queue after: A-702 revprops / replay (g302 brief), A-652 export.

## A-702 part 1 -- LANDED main 15a3c5d4c (commit 3960207de, verified). serve::revprops; 165006 without a hook after
## svnserve's checks. Local svn 128/128 then 131/131 merged. Part 2 waits on ra_svn running hooks.
## NOW: S4 slice 1 (agreed with w-perf): slates-svn records keep text identity+size+checksums, no bytes; readers via
## lent `TextSource` (`fn text(&self, identity: &ContentIdentity) -> Option<Cow<'_, [u8]>>`); Room charges places.
## w-perf does slice 2 (server TextSource over Green::held, svn_commit drops text bytes, rebuild re-holds).

## S4 slice 1 -- LANDED main 21f03d433 (commit 7e1171e09) + follow-up 1d7c602ec (commit 5ff74820f), both verified
- TextSource / WithTexts / Cow text traits. My push of 21f03d433 met main's new dav/report_file_revs/tests.rs and broke
  slates-svn's lib-test build on main for one push; fixed by 5ff74820f (landed 1d7c602ec). Landed tree: svn 157/157
  (reads/dav/serve), server lib tests compile. Lesson: re-run after the last merge even when only tests changed.
- NEXT: w-perf's slice 2. My queue: A-702 part 2 waits on w-reaper's hooks piece; A-703 replay/replay-range next.

## A-703 replay/replay-range -- LANDED main 2963b7ad2 (commit 5fd097b10, verified)
- report::drive::replay (svn_repos_replay2 + path_driver3), serve::replays (streamed, budgeted), fixtures
  crates/svn/fixtures/replay (svnsync, svnrdump, 6 scripted; recorded on the Studio, moved via kiln code_run).
- Tests: 31 revisions call for call; every session through a connection (also byte+step a call). Local svn 296/296,
  server 35/35 on the landing tree (merged with A-730's lock hooks; serve.rs conflicts kept both sides).
- Run request: live svnsync/svnrdump over svn:// against a daemon vs svnsync.dump / svnrdump-dump.dump.
- NEXT: A-702 part 2 on A-730's hooks (vcs_svn::run_hook with HookPoint::SvnPreRevpropChange).

## A-702 part 2 revprop change through hooks -- LANDED main 0aaed9b3f (commit f5ab439dc)
- serve::revprops hands Turn::RevisionPropertyHook; revision_property_hooked (160006 / 160049, whole Record),
  _landed, _finished; listener vcs_svn::revision_property + tunnel SvnCommand::revision_property (A-736 ClientGone);
  SvnStores::land_locks -> land_record. Tests: svn 5/5, server 3/3 (hook args/stdin, svn:log read back, no hook 165006).
- Ledger A-702 row updated (fanout-state e19429d).
- Run request: stock `svn propset --revprop -r1 svn:log` over svn:// and svn+ssh:// with a pre-revprop-change hook.
- NEXT (main's bug, 2026-11-20): svnrdump replay-range over git/git spins shard 0 (3596 ms). Root cause read: the
  listener's go_on loop never yields on Turn::Wrote (Next::Again) or pipelined commands; fix = yield_now per turn.

## replay-range spin fix -- LANDED main a469a6b85 (commit ea468de36)
- vcs_svn follow(): Next::Again and Next::Consume yield_now before the next turn (SSH edge already yields a step).
  Listener tests 12/12 on the landed tree. No unit test sees a missed yield; run request: git/git svnrdump dump.
- NEXT: A-652 export half + lock-expiry by use (g268), then DeltaV v1, review piece 14.
