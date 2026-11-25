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
- Follow-up LANDED main 1fc450f1a (commit aa3a60af1): counter vcs_svn.turns_unyielded (polls_so_far unchanged
  between turns) + test pipelined_commands_are_answered_one_turn_a_shard_poll (64 pipelined, count 0 both shards).
  Red [1,68] on a469a6b85's parent, [1,2] on a469a6b85 (handshake turns), so the yield moved to go_on after any turn
  whose run did not move the shard's polls: red [1,67] without, green 13/13 with.

## no-hook revprop doubled request -- LANDED main 90d405247 (commit c83698017)
- vcs_svn::step cleared out after writing (trivial auth was written twice; stock svn read the 2nd as success).
  Windows run_svn_hook sibling answers 165006 (w-lint's jupiter lane compiles it). svn_interop no-hook test un-ignored.
## svn branch verbs refused by kind -- LANDED main 00a135af4 (commit 3efe27247); §4.17.4 amended.
## A-652 export half (taken over from g268): g268's staged work saved as origin/agents/g268-a652-3b-land2 (== wip2 code);
  carried onto main in w-svn-a703 as 272a543ca (pushed origin/agents/w-svn-a652-3b-wip). One red:
  t_a652_a_range_exports_into_a_new_volume_as_svnadmin_dump_wrote_it (image.units_past_budget=41, reply.undeliverable=2).
  NEXT: failing test with units_past_budget at 0, slice that path, then see full.dump / the 2 undeliverable replies.
  Disk: one worktree only; CARGO_INCREMENTAL=0.
## A-652 export half -- LANDED main a3f2602d7 (commit 4222a1049): id minted in publish (two exports collided).
## volume-id mint sweep -- LANDED main 3f65086f4 (commit 9cbdadac2): ShardState::volume_ids_minted reservation.
  Whole slates-server lib suite killed by watchdog (shard spinning ~3.3 s) on this loaded Mac with and without it.
- NEXT: migration::export resumable inside a revision: (1) a changed path a call, (2) a text a window at a time.
## exporter slicing part 1 LANDED 04dccfddb (f3683fb6d), part 2 LANDED bc180d633 (d82eb41e5), part 3 LANDED 9b82c4f31 (515bc3c22)
  Calls now: a changed path, or a text window (format 2 one walk; format 3 measured a window a call before its record).
- NEXT: g268's lock-expiry case (by reading: commit drive judges locks at start.now, drive.rs:2435; FSFS judges per call;
  only a DAV LOCK with a Timeout makes an expiring lock; svnserve's locks never expire).
## Release measurements (big Studio, adas-mac-studio.local-2) -- LANDED main c2f300d96 (BENCHMARKS + GAPS)
- Export calls vs 14 us slice: fmt2 median 167.8 us (digest of 100 KB piece), fmt3 p99 289.6 us (100 KB window).
- 500 MB svn commit: E210009 request-size refusal over svn:// and DAV; RSS 37->50 MB. Intake rules 1 and 2 fail.
- How to run on the Studio: kiln sync of a worktree times out (its .git points at the main repo); ship a standalone
  repo: git archive HEAD -> ~/slates-work/w-svn-ship (git init, one commit) and kiln run with cwd there, on
  adas-mac-studio.local-2 (has svn 1.15.0; no GitHub key).
- Asked main: is svn's byte store for the intake w-perf's S4 slice 2 (A-716)? Format 2 piece slicing needs a measured
  MD5+SHA-1 rate (like archive_slice_bytes' BLAKE3 rate): waiting on main's ordering. CLOSED (11-25): landed as item 2,
  63ab25b9f (svn_digest_bytes_per_second, svn_text_piece_bytes, Exporter::with_text_piece), used by svn_export.rs's
  exporter; the release measurement is main's round.

## INTAKE (main's order 1, Ada: perf gaps fixed upfront) -- in progress
- Agreed with w-perf (slates-work-89): stage/append/seal/discard in slates-merge (LANDED e9b70d6d4, commit 6487601b2);
  w-perf wires the server's TextSink over them; vcs_store charges each piece (lend/settle_objects, shard's budget);
  1b (set a path from a held identity) is mine, with the landing.
- Drive plan (slates-svn commit::drive): `pub trait TextSink` (&self: stage -> StagedText(u64), append, seal(identity),
  discard, release(identity), read sealed via text) passed as `texts: &dyn TextSink` to Drive::take/resume/edit/
  resume_edit (World gets it; Sources gets `texts` field for serve.rs). Delta streams: window applied into a scratch
  Vec, md5/sha1/blake3 incremental, appended to sink; textdelta_end seals; Work.texts -> identities (+facts);
  Revision::of_work reads bytes back via the sink until w-perf's c3 drops the log copy. Room = window in flight.
  Abort/refusal: discard open, release sealed. Server: interim sink queues appends per receive and applies under
  with_state after (or w-perf's wiring).
- Red (by use, Studio): a commit larger than a request's room lands; peak RSS minus stored text independent of size.
- Intake part 1 LANDED main 2d8ab9f3d (commit b5c4b8848): drive streams windows through TextSink; Sources::texts;
  listener Opened::texts, tunnel SvnCommand::texts (SvnStores::lend/commit take texts), DavCommit owns Box<dyn TextSink>,
  Merged::Accepted(work, texts); commit_work(.., texts); NoRoom E160000. Interim server sink = HeldTexts per connection.
  w-perf told (wires green sink; apply_holding should take the work's texts set). By-use 500 MB rerun: r30 on Studio.
## INTAKE PART 2 -- DECIDED (main, 2026-11-20): option (B)
- Sink writes each window into vcs_store's content plane (git's quarantine shape, A-662/A-719: GitPush::admit_all,
  push_sealed entries with a store move); the revision's log entry names text identities + a store move; green
  increment sets the path from the identity (A-716 1b, mine, merge crate); landing walks rows/identities a turn at a
  time. w-perf told by main: sink wires into the content plane, green's held table points at store identities.
- Conditions: (1) durability order as A-727 for git: content durable (content plane image barrier; f62687455 homes
  barrier) before the entry naming it commits; replication content before record. Red: kill between content write and
  entry -> after recovery commit whole or absent. (2) Measure 500 MB before/after in release on the big Studio over DAV
  and svn://: wall time, peak RSS vs text, landing's longest step, and stock svnserve on FSFS too.
- Records.rs ownership proposed to w-perf: w-perf owns text storage/c2/format; I own the landing walk entry points.
- Before (base 2d8ab9f3d): svn:// E210002 (daemon restarted, shard 0 spun 1.06 s in landing), DAV 8.0 s once / spun
  once; RSS 6.0-6.2 GB (12x). A-740 labels added to vcs_svn::land and vcs_http::svn::land (to prove the spin site).
- LANDED 134626b40 (A-740 labels on vcs_svn::land / vcs_http::svn::land; proof run r32 on Studio pending).
- LANDED main 4839b01f8 (commit e211d430c): slates-vcs ContentStore::begin_object/put_window/finish_object/
  abandon_object (streaming put_object twin; test an_object_put_a_window_at_a_time_is_the_object_put_whole).
- Proposed split to w-perf (awaiting reply): (1) streaming put = mine (done); (2) naming = store object root
  (records name_text too: w-perf); (3) server TextSink over ContentStore under vcs_store::Lent = w-perf; (4) landing:
  entry names identities + store move, durability barrier before entry, 1b, walk in turns = mine.
- Open question for main: 1b under (B) means the merge green's file blocks over the content plane's arena chunks
  (A-727: "can be one block") -- a merge-crate storage change (w-engine?), not just an op.
- Naming RULED (main via w-perf, inbox/identity-rule-from-w-perf.md @2db786af6): identity = plain BLAKE3 everywhere that
  crosses a node; store root node-local (w-perf: finish_object returns BLAKE3, alias to root). Entry names (identity, len).
- c2 LANDED by w-perf 14215b0c3: records keep texts as skip-delta svndiff2/LZ4 (KeptText, keep_fresh at apply).
  Asked main: under (B) records keep sink texts as Stored{len} references (no delta), or slice keep_fresh? Awaiting.
- No content-image durability barrier API exists (A-727 slice b owes it): asked main who builds it.
- Big Studio (adas-mac-studio.local-2) runner stopped answering during r33 (release 500 MB + landing timing patch on
  7346bd343; patch in ~/slates-work/w-svn-ship commit 0ca395b, ~/.cache/w-svn-ship/timing.patch). Told main.
- Item 2 LANDED main 63ab25b9f (commit ebd4a5ff0): DaemonConfig::svn_digest_bytes_per_second (aws-lc MD5+SHA-1 at
  derive), svn_text_piece_bytes(), Exporter::with_text_piece. Release numbers owed (Studio down).
- Intake part 2: svn half committed LOCAL ONLY in w-svn-a703 (wip commit "stored texts named by a revision..."):
  records Stored{len}, Revision.stored, codec v2, of_work reads no bytes; svn lib 563/563. Server half waits on
  w-perf's identity alias (BLAKE3->root) and w-balance's barrier (inbox/w-svn-from-w-balance.md). 1b is w-engine's next.
- Reported by reading: c2's exporter `load` rebuilds a whole text through its delta chain in one call.
- Alias LANDED (w-perf 9010c5630). w-perf: I do (b) in my interim: svn_texts.rs HeadTexts store arm (order green ->
  store -> records; keep records arm), lend_in repository = view over records + HeadTexts, replay holds each stored
  identity. w-perf stays out of svn_texts.rs/svn_view.rs/svn land paths; I stay out of vcs content.rs,
  content_holder.rs, vcs_store.rs recover/restore fns, arena_windows.rs (w-perf's a3b for ~1h).
- Server half plan: svn_commit.rs Storing{queue,current,put} + begin_storing/store_step (within_budget, sink
  sealed_piece, copy_slice_bytes a step)/release_stored; Building.text_bytes falls back to content store
  (object_bytes(root_of)); commit_work(state,..,work,now) (no sink); land paths loop store_step with yield, barrier
  (stub local), commit, release on refusal / release extra holds already held.
- LOCAL wip commits in w-svn-a703 (NOT for main until barrier): svn half; server landing in turns
  (svn_commit::Storing/begin_storing/store_step/held_twice/release_stored/release_texts/land_work + LOCAL
  content_durable() stand-in), Building.store arm under lent_state, async SvnStores::commit (tunnel) and
  vcs_http SvnStores::land (DAV), TextSink::sealed_piece. Server lib builds.
- NEXT (read side): svn_texts.rs HeadTexts{green, store, records} store arm + `type SvnView<'a> = WithTexts<'a,
  HeadTexts<'a>>`; lend_in/lent_of lend the view (under lent_state); GATs: vcs_http SvnStores `type Repository<'a>`,
  vcs_ssh SvnStores `type Green<'a>`, CommitWorld<'_, SvnView<'_>>; then replay holds stored identities; then tests.
- 2026-11-20: read side + replay hold + tests done (LOCAL ea3daf5d7); merged origin/main (alias 9010c5630, barrier
  5a189220a); stand-in replaced by vcs_store::content_ticket/content_durable (budget fleet::live_budget), refusal
  Uncommitted::NotDurable; crash test daemon::tests::a_subversion_commits_stored_text_survives_a_crash_right_after_its_entry.
  Building/testing; then land part 2 whole. Next: exporter `load` sliced rebuild (item 2), then DeltaV v1.
- A-761 part 2 LANDED main 28e57ac18 (squash of w-svn-a703; base 234c11f8e). Red: crash test with barrier bypassed
  -> (false,1,None); green. Server focused 128/128, svn 569/569. Exporter reads stored texts via svn_view::with_view.
- NEXT: item 2, exporter `load` rebuild sliced (TextReading: chain a window a step; store a window a step; green a
  copy slice a step; cache = delta's two). Then DeltaV v1 (read w-interop progress first).
- 2026-11-20: LANDED b1206d96c (svn landing waits on the barrier only when it put texts; red vcs_ssh::svn::tests stock
  mkdir on main, my filter vcs_ssh::tests::svn had named no module -- sweep with `cargo test -p slates-server --lib svn`).
  LANDED A-762 507d722b3 + 2cf1b1d35: exporter reads a text a bounded step a call (TextSource::read_step, TextReading).
  Ledger edits now from ~/slates-work/w-svn-ledger (shared checkout had another agent's staged state).
- NEXT: DeltaV v1 for pre-1.7 clients (MKACTIVITY/CHECKOUT/MERGE; review piece 14); w-interop piece 5 done, no overlap.
  Owed: Linux daemon:: run on kiln (r36 never started, no room); 500 MB commit re-measured after w-engine 1b.
- DeltaV v1 (pre-1.7 / no-v2 serf clients) PLAN, sources in ~/slates-work/g234-svn-src/dav-1.14 (1.14.x mod_dav_svn +
  ra_serf commit.c): OPTIONS activity-collection-set -> `!svn/act/`; MKACTIVITY act/UUID 201; discover_vcc PROPFIND;
  checked-in -> `!svn/ver/REV/PATH`; CHECKOUT vcc (body activity-set href) -> 201 Location `!svn/wbl/ACT/REV`;
  PROPPATCH wbl = revprops; CHECKOUT ver/REV/path -> 201 Location `!svn/wrk/ACT/path` (out-of-date check by rev);
  PUT/MKCOL/DELETE/COPY/PROPPATCH under wrk == txr; MERGE source=activity href; DELETE act. Map onto DavCommit: an
  activity = a transaction begun lazily (revprops from the wbl PROPPATCH) and wrk/ACT/path == txr/TXN/path.
  Part 1 slates-svn sans-io (URIs, CHECKOUT body/answer, activity state), part 2 server site, part 3 by-use (stock
  svn 1.15 against a no-v2 OPTIONS test hook, on a machine with svn).
- DeltaV v1 parts, revised: 1 READS -- site.rs serves vcc/default (checked-in -> bln/YOUNGEST), bln/N
  (baseline-collection -> bc/N/, version-name), ver/N/PATH (a version resource: PROPFIND, GET) as repos.c answers
  them; 2 WRITES sans-io (dav/activity.rs: URIs, CHECKOUT body, mod_dav's 201 HTML body pinned from httpd 2.4.x
  mod_dav.h DAV_RESPONSE_BODY_1..5 + "X Y has been created.", Cache-Control no-cache; DavCommit activity + checked-out
  revisions); 3 server; 4 by-use.
- A-766 (DeltaV v1) part 1 LANDED 84c74cc52: vcc/default, bln/REV, ver/REV/PATH; update-report at the VCC. Next: part 2
  (commit verbs over DavCommit), against w-store's seam once agreed (inbox/w-store-from-w-svn-text-source-2026-11-20.md).
  Owed: PROPFIND writer's per-property strings into the connection's buffer (MEMORY SIZING); 500 MB commit re-measured
  after A-760. Flake seen once under load: vcs_svn::tests::an_unlock_runs_pre_unlock_and_post_unlock_as_svnserve_does
  (passes alone 3/3).
- 2026-11-20 afternoon. LANDED: A-766 part 1 84c74cc52; A-772 drive share 64655a32c (replay_bench + bounded-windows
  test; 328 s -> 1.82 s with w-perf's WindowCache; my owned-run fix dead even, dropped); A-780 5586b9194 (PROPFIND
  writer in place: allprop 5,691 -> 445 allocations over 33 responses).
- A-772 OPEN: server per-turn cost + 2,511 MiB peak. Measuring with vcs_ssh::svn::tests::
  measure_svnsync_replay_range_over_the_tunnel (ignored; SLATES_SVN_REPLAY_DUMP=~/.cache/w-svn-gitgit/git-2000.dump,
  SLATES_SVN_REPLAY_LAST=300), release, in ~/slates-work/w-svn-a703 (target ~/.cache/w-svn-local-target). The test's
  import runs a turn per observe_first (one closure spun the watchdog). Plan: sample the process during the replay.
  Per-turn suspects (read): authenticated_access + with_view + rules clone + 1 KB socket write + yield per one-step turn.
- NEXT after A-772: A-766 part 2 (DeltaV v1 commit verbs). Plan: server keeps a pending activity (id, revprops) from
  MKACTIVITY; CHECKOUT vcc -> 201 wbl/ID/YOUNGEST; PROPPATCH wbl begins the DavCommit with its revprops (start-commit
  hook as POST); CHECKOUT ver/REV/path records path->REV, 201 wrk/ID/path; wrk requests = txr with that base rev;
  MERGE of act/ID commits, full merge response unless X-SVN-Options no-merge-response (merge.c send_response per
  changed path); DELETE act aborts. mod_dav's 201 body: DAV_RESPONSE_BODY_1..5 (httpd mod_dav.h) + "WHAT URI has been
  created.", Cache-Control no-cache. Worktrees: w-svn-a703 (measurement), w-svn-next (branch agents/w-svn-a772, on main).
- CLOSE LIST (Ada 16:20): C1 DeltaV v1 CLOSED 3e63bee8d (by use, kiln r15). Next C2 (svn onto the changeset store
  with w-store), then R3 hook flake, R4 recovery replay sliced, R5 exporter rebuild + svnserve-over-SSH red test
  (both landed: 507d722b3, b1206d96c -- write close notes), P6 svn:// update 5.6 vs 2.7 s, P7 svnsync 524/1,068 s
  (A-772: drive 328->1.82 s, tunnel 7.4 ms/rev; daemon peak owed; uncommitted RSS checkpoints in w-svn-next
  vcs_ssh/tests/svn.rs), P8 500 MB commit.
- CLOSED: C1 3e63bee8d, R3 74beb5600, R4 25035f50c, R5 (507d722b3/2cf1b1d35 + b1206d96c). C2 waits on w-store's
  server wiring answer. NEXT: P6 svn:// update 5.6 vs 2.7 s, P7 svnsync (A-772 daemon peak; uncommitted RSS
  checkpoints in w-svn-next vcs_ssh/tests/svn.rs), P8 500 MB commit.
- C2 (main 20:30 ruling: the store is the truth; replacement, no second path; register keeps green head only; crash
  test between store land and register commit; recovery test is the arbiter). w-store has on main: sections/read/text
  (ba760f0ef), sliced landing begin/section/part/commit (71db6d157). Part 1 LANDED 00ff3b25c (land_work lands
  changeset seq=revision -- seq rule changes in stage A to changeset count). Staging sent (inbox main-from-w-svn-c2-plan):
  A history (all records as changesets, register head only, recovery from store, arbiter + crash tests; texts stay in
  content store), B texts async via owner (svn drives yield), C import as one sliced changeset. Asked main the
  no-store decision (inbox main-from-w-svn-c2-no-store). Worktree ~/slates-work/w-svn-c2 (agents/w-svn-c2), target
  ~/.cache/w-svn-c2-target. Register write sites: svn_commit land_record (locks/revprops; callers vcs_svn.rs:1219,1278),
  land (revision + green head entry, svn_commit.rs:623), append_made (svn_made, in a submit tx), svn_created
  (repository.rs:555). Recovery: vcs_store record_turn/recover_repository over register indices.
- P6: measurement test (w-svn-next, uncommitted vcs_ssh/tests/svn.rs: RSS checkpoints + update/checkout) got
  "not whole" 524-byte answers for update; release run printing the answer in progress (bq3pv7luh). git-200.dump made
  (~/.cache/w-svn-gitgit) for faster runs; debug builds die to the watchdog in import.
- C2 stage A built in ~/slates-work/w-svn-c2 (uncommitted at 22:40, testing): every svn record store-first
  (svn_commit::landing: gate/turn, sequence = register object count, reference-only register entries), recovery and
  rebuild read changesets back from the store (vcs_store svn_to_fetch/fetch_svn), svn creation refused without
  --vcs-store, crate::test_store shared helper (TestStore guard, StoredDaemon, run_on_first), svn_commit/import/export
  tests async on stored daemons, arbiter + crash tests in repository::store_tests. slates-server svn+store_tests
  96/96, identities+vcs_store 37/37, barrier/deltify/svn_texts 3/3, cli --test cli a_repository_the_cli_creates 1/1.
  svn_made: landed by a task holding the turn the submit took (main asked: inbox main-from-w-svn-c2-made).
- P6 finding: the measurement's scripted update was wrong, not the server: abort-edit then E210004 "Successful edit
  status returned too soon" (the script's report). Fix the script after C2.
- C2 stage A LANDED 72576cb1f (0703e62d2 + 65f69e1da). Tests: server 136/136 affected; cli svn_interop 4/4,
  svn_http_reports 9/9, cli create 1/1. Main's ruling on svn_made (inbox w-svn-from-main-c2-made): submit acceptance
  deferred until the made revision lands, no recovery remake -> next piece. Then stage B, stage C.
- C2 stage B part 1 LANDED 40c3f8d1e (e69375553): texts in the changeset, recovery fetch, barrier gone for svn. P6/P7
  measured (BENCHMARKS). Next: B part 2 (texts read through the owner). Stock legs kiln r2 on mac-studio.local-2; P8 next.
- 2026-11-21 ~00:50: P6 CLOSED (close note), P8 CLOSED (002a7944f), P7: real-daemon peak measured (8508a8d52: 1,570
  MiB for a 437 MiB dump, half the segment: green chain carries texts), green-fold rejected (c9f79f925), svnsync
  measurement landed (1bfe51f88) and running on the big Studio (kiln r10). Stock legs pass on the Studio (r8).
  Open: C2 B2 (main to decide, inbox stage-b2-value), svn_made (w-work-cow's view), C. B2a wait-path patch kept at
  ~/.cache/w-svn-b2a-wait.patch.
- 2026-11-21 ~01:20 PST state: CLOSED C1 R3 R4 R5 P6 P8. P7 measured (svn:// svnsync 270.7 s vs 1,068; real daemon peak
  1,570 MiB / 437 MiB dump, half the green's chain in the segment; HTTP svnsync init fixed 2dafcad02, replay-report
  absent -> GAPS), awaiting main's close. C2: stage A (72576cb1f) and B part 1 (40c3f8d1e) on main; B part 2 and C sent
  to main with sizing (inbox stage-b2-value, stage-c-analysis), awaiting the call; svn_made waits for w-work-cow's
  post-verdict view (main's routing), w-git builds git's made commits on the same hook. Worktree ~/slates-work/w-svn-c2
  at main 9d792682f, clean. Patches kept: ~/.cache/w-svn-b2a-wait.patch (B2 wait path), ~/.cache/w-svn-fold-attempt.patch
  (rejected fold).
- 2026-11-21 ~02:10 PST: B2 (a) on main: drives (35786a9dd/b934f9e5b), ra_svn get-file (9061acd6c), DAV GET
  (dd31d7597, base origin/main dc04e1a7d). Each held by a store test, red without its wait. Owed in (a): file-revs'
  window_unit, the exporter, the deltify job; then (b). Target trimmed by exact path 24 GB -> 3.9 GB (debug/incremental
  and 37,242 leftover deps/*.rcgu.o); building with CARGO_INCREMENTAL=0, one profile (debug) from now on.
- 2026-11-21 ~02:45 PST: B2 (a) COMPLETE on main: get-file-revs (560ebad9f), export (99ff3b8cb), deltify job
  (d31502d48, base origin/main at merge). Every reader of a stored text reads one RAM does not hold through the store,
  each held by a store test red without its wait. New finding (GAPS only): the svn://, svnserve -t and DAV loops have no
  guard against a drive waiting again for the piece just fetched (the export and deltify jobs have one). Next: B2 (b).
- 2026-11-21 ~04:00 PST: B2 (b) P1 (197b5f2d7: a commit brings back an evicted stored text for its green increment),
  P2 (34259b8eb: the hook view brings one back before holding it; GAPS: a view pins what it shows), P3 (abd5e7b2e: the
  content store lets go a commit's texts once landed, recovery reads none back, deltify job retired) on main, each with
  a store test red first. svn_made was done by A-804 (90e2a7666, not mine). Next: P4 (imports land texts as Payload,
  records keep none in RAM). Target trim of leftovers was declined at the prompt; disk 26 GB free.
- 2026-11-21 ~10:00 PST: B2 (b) P4a 8d7dec8df, P4b f50841803 on main (every new svn text on the store, none in RAM);
  A-818 72a1bbb24 (main's 09:20 "RETIRE"): Record::Deltified and the deltify APIs gone, a pre-P4a log refused typed
  (Retired::Deltified at decode, SvnTextsInline at replay). KeptText stays for records standing alone (told main; a
  fixture move if main wants it gone). C2 stage B complete. Next: P7's findings, re-measured on this tree first.
- 2026-11-21 ~08:30 PST (date clock): A-819 renumber 6652f7717; P7 re-measured by main (r50: peak 1,630 MiB, heap
  not down). Finding (1) BUILT as A-820 be9a33e34 (copies from older revisions made from the head and set back from the
  records; svn greens fold to readers' floor); A-side measurement test 3bf21428b (landed 8940003ed). Owed, main's
  runs: the A/B (measure_copies_from_older_revisions) and the stock svn leg (the big Studio d1323898 offline in kiln).
  Next after P7: KeptText fixtures onto a test-side text source (main's 10:35 item 3).
- 2026-11-21 ~08:30 (date clock): hold hook 30d2e4100 (SLATES_SVN_IMPORT_HOLD) for main's attribution of (2).
  KeptText move STOPPED by main (after P7). NOW: P7 (3) = C2 stage C's records side over A-814 (ImportBatch).
  Plan: (1) a batch changeset = Payload+Texts of all its texts + one Record section per revision (landing::land_changeset
  takes Vec<record>; changeset_records reads all Record sections); (2) replay applies every Record section of a
  changeset in order (svn_fetched holds a queue); (3) svn_import stages each revision: take_texts -> content store
  (Storing accumulated), restore::plan, records.check + apply in RAM, Building against batch.with_engine (needs a
  Greens-taking variant; tree_beneath must use records.youngest not engine.head), batch.stage; flush at a derived bound,
  before a non-revision record, at the import's end: land one changeset then ImportBatch::commit(EntryParts{objects:
  reference(seq)}); refusal: batch.abandon + vcs_store::rebuild + release storing; (4) svn readers refused while
  importing (as hg_view.rs:346). Tests: imports export byte for byte, register names few changesets, recovery equal,
  crash before commit leaves nothing.
- 2026-11-21 ~10:40 (date clock): P7 (3) / C2 stage C records side LANDED as A-822 e2410cb8b: an import lands batches
  (one changeset + one register entry each, bounded by the import's admission, counted by cause), atomic (differs from
  svnadmin load: a refusal names the last landed revision, SubversionLanded, contract 59; the same dump resumes,
  reading held revisions through), no svn session while importing, ImportBatch::stage renumbers Copy ops. Owed: main's
  git-2000 re-measure (time, peak), the A/B for A-820, the heap attribution for (2). Then (2), then KeptText.
  Seen under load ~30: vcs_svn hook tests flake in the full suite, pass alone (15/15 in 4.7 s).

## 2026-11-21 ~14:40 PST — A-835 landed (e15e87490, merged to origin/main as c5e157467; base 8fb2e33db)
(b) and (c) of main's 12:43 ruling on the A-822 watchdog kills. A recorded import batch is applied to its green a
slice a turn under the shard's quantum (push::RecordedBatch::apply_slice, counted repository.import_batch.apply_slices),
the landing turn and the import's read gate held to the last slice; a recovered Subversion green folds to its readers'
floor (vcs_store::remake_greens). Crash test: kill at every slice boundary (0..4 of a 13-version batch, 3 a slice),
recover, resume Ok((0, 13)), byte-identical export, green folded to (13, 13). Red checks run: without the fold at
recovery it fails (folded (0, 13)); with the slice deadline taken out the 400-revision batch applies in 1 slice and
fails. Focused tests 99/99 on the merged tree 10760328f. Still one step a batch: append_versions' pre-record loop
(w-rewrite-ops', inbox/w-rewrite-ops-from-w-svn-a814-sliced-commit.md) and follow/fold/release after the last slice.
Next: finding (2) re-measured on the merged tree; main runs git-2000.

## 2026-11-21 evening PST: the KeptText move is done (main's 10:35 item 3, go at 14:52)
- Piece 1: 41ce54f67 (merged 270df0d72). `migration::Standalone`, records standing alone with their texts held apart.
  `Repository::take_texts` moved from the server. The migration tests run on it.
- Piece 2: e465b0e0c (merged dba4ae793). Moved onto it: `fsfs_view`, `propfind_allocations`, `replay_bench`,
  `fsfs_view_bench`, vcs_ssh's svn tests, and the presenter's tests (texts in a content store).
- Piece 3, A-842: cc55404fa (merged 2ed0eec43). The records keep no text: `KeptText`, its delta chains and window
  reads are gone. A text carried inline is refused `Corrupt`. A replay is two steps. `changeset_ab` is retired.
- Tests: svn lib 584/584, `fsfs_view` 5/5, `propfind_allocations` 1/1, server `--lib svn` 113/113.
- For main:
  - Run request: the stock svnadmin/svnlook reading of the all-PLAIN FSFS view (`fsfs_view
    stock_svnadmin_and_svnlook_read_the_view`, svn 1.15 host).
  - Their call: the view's DELTA writer has no source now (remove it, or feed it derived deltas).
  - The ignored git-2000 measurement's column is renamed `records_stored_text_bytes`.
- Still waiting: main's r2 git-2000 re-measure of A-835, for finding (2).

## 2026-11-21 ~22:30 PST: main 01a20182's 21:35 rulings done
- The FSFS DELTA writer is removed: 792c62308, merged 235d62418.
- w-balance has leave for A-724 piece 3 in my files (`inbox/w-balance-from-w-svn-admission.md`).
- Main runs the stock fsfs_view leg (big Studio) and the git-2000 re-measure of A-835. Waiting on them for finding (2).

## 2026-11-21 ~23:50 PST: finding (2), attribution under way
- r10's lines: heap 1.49 GB in use after the import; segment and content kinds near zero.
- But the daemon has 1.30 GB of heap allocated before any import (smoke, RSS 84 MB), so the heap figure is mostly its
  startup allocation.
- In-process probe (local, uncommitted, in svn_import/tests.rs): 400 rewrites of a 256 KiB file, a 105 MB dump.
  - RSS 179 MB before, 867 MB after.
  - Live owners afterwards hold almost nothing: the green its head, records 24 KB, content store empty, no volumes.
  - The chunk arena holds 317 MB after the import, given back within 1 s of idle; RSS stays 789 MB.
  - macOS RSS keeps MADV_FREE_REUSABLE pages, so the footprint is the true figure.
- Landed: `an_import_measured` samples status --memory, RSS and footprint (via top) before, after, and 2 s and 30 s
  into the hold. 4bc541ca5 and c74f35cc1, merged 78e584f18.
- Waiting: main's git-2000 run of it. Then the red test, on footprint after an import and an idle tick.

## 2026-11-22 ~00:35 PST: the HTTPv2 revprop red is fixed
- d843d2325, merged 946e39c22: the edge serves PROPPATCH on !svn/rev/N and !svn/bln/N, judged by the shared
  RevisionPropertyChange path, with both revprop hooks and the landing turn.
- Main's r6: fsfs_view's stock leg GREEN.
- Waiting: main's rerun of stock_svn_over_httpv2_answers_as_stock_svnserve_byte_for_byte, and r11's finding (2)
  blocks. My finding (2) probe is still local, in svn_import/tests.rs.

## 2026-11-22 ~01:20 PST: finding (2), cause found
- Main's r11 (git-2000, release): physical footprint 43 MiB before, 527 MiB at 30 s idle, 484 MiB held.
  - Live heap +104 MB; arena 51 MB.
  - Malloc's freed-but-kept memory +846 MB, the bulk.
- Reproduced in debug with a 100 MB dump: 363 MB held at 30 s.
- With A-742's large-path floor lowered from 64 to 4 pages (experiment, reverted): 47 MB, at baseline. The import was
  slower, 19.4 s against 12.0 s.
- Asked main to rule: (1) rederive the floor, (2) cut the import's sub-1 MiB churn, or (3) both.
- Filed: content.purged_bytes keeps climbing during idle.

## 2026-11-22 ~03:45 PST
- The HTTPv2 `svn log` red was the test's comparison: stock svn writes XML attributes in a random hash order, even
  against svnserve. Sorted, the outputs are identical. Fixed in 0de4c75ea; main's rerun is requested. The hang after a
  panic is not found (asked for a `sample` of the process if it recurs).
- Finding (2): attributed by allocation site; about 15 copies of each text.
  - b1efa8edc (merged 9c662576f): my three cuts, and the red. `an_import_measured` gates the idle footprint at 2x what
    is still live; it is red.
  - The rest is routed to main for the other owners: content store, engine, push, store owner.
- Local only: the attribution probe in svn_import/tests.rs (a test-only global allocator), not landed.
- Waiting: main's routing, git-2000 rerun, and HTTPv2 rerun.

## 2026-11-22 ~01:25 PST: SlicedLanding::part's copies cut (main's routing of 00:57)
- 2a8dbcd99: the owner gives each part's buffer back; the svn landing fills one buffer a landing. Red first.
- Store owner tests 5/5. Server svn tests 113/114: a DAV hook test flaked on a WouldBlock read at load 74, 3/3 alone.
- Checked first: w-hg holds no unlanded store work.
- Waiting: main's r18 git-2000 attribution, and the HTTPv2 rerun at 0de4c75ea or later.

## 2026-11-22 ~03:00 PST
- f301b9f0d: the measurement's footprint read retries a miss, and its bound allows top's 1 MiB resolution.
- 8db529eec (merged 059578d78): the revprop test's flake was the hook runner. A hook that exited on its own before its
  kill was timed out with its own code 0; it is now judged by its own exit. The DAV refusal answers its chain's best
  message.
- A sibling flake remains: a first-run hook's words pipe inherited by a concurrent spawn. Asked main to rule:
  (a) no inherited fds, (b) end the drain when the hook's group is empty, (c) both.
- Waiting: that ruling; the HTTPv2 rerun at 0de4c75ea or later; finding (2)'s routed copies.

## 2026-11-22 ~03:45 PST: A-858 landed; clippy findings fixed
- A-858 (main's ruling (a) and the guard order of 03:22): bd94fed6f, merged as 9b1b4ccdf.
  - Every child the daemon spawns goes through `slates_ipc::confined_spawn` (posix_spawn, CLOEXEC_DEFAULT on macOS).
  - The hook driver rereads `waitid` after an exit note the kernel posted early.
  - The svn_interop guard unmounts its views before it stops its daemon.
  - Red first: 32 hooks at once, red 2 of 3 on the old driver; 20 of 20 green after.
- Clippy findings: 2d65eefbf (the commit_side split) and 00844de07 (confined_spawn's records own themselves, no
  mem::forget). The unsafe budget for slates-ipc is now 159.
  - All 12 sites are posix_spawn FFI, which rustix does not wrap; the waits and the group kill already go through
    rustix.
- Main pushed the macOS proof, diag-measurer-w-svn-858 at afa560c26, as run 45018545878. Main's clippy rerun is
  45018575168 at 16489f046.
- Waiting on main for:
  - those two results;
  - finding (2)'s routed copies (w-multirepo, w-engine, w-shared), then the git-2000 re-measure.
- Red on run 45018545878 would be a new piece for me: the propset hang still there (its sample shows the wait), the
  guard test failing, or a view left mounted.

## 2026-11-22 ~04:15 PST: put_window's churn cut (main's routing of 03:46)
- 840daa24d: `ContentStore::begin_object_of_length` reserves a text's window buffer once; svn's `store_step_from` and
  `bring_whole` use it. Red first.
- Local attribution: 97.5 MB in 400 allocations → 26.2 MB in 100; the band's churn fell from 10.7 to 8.0 times the text.
- Tests: slates-vcs content 20/20; slates-server --lib svn 113/113.
- Noted for w-multirepo in `inbox/w-multirepo-from-w-svn-begin-of-length.md`.
- Item 2 (the daemon breakdown) is on hold: w-shared's `slates status --memory` gauges come first, then
  an_import_measured prints them.
- Waiting on main: r12's git-2000 number; the macOS run 45018545878 and the clippy run 45018575168.

## 2026-11-22 ~04:35 PST: the refused put's leak and the status red (main's rulings of 03:55)
- macOS proof (main's CI 45018545878, at 00844de07): the propset hang is gone (33.57 s, against 120 s before), and
  the guard left 0 views mounted.
- 7c53d589c: a text refused part-way through its put leaves the store as it was. The writer now stays in `Storing`
  until finished, the fix by construction. Red: 8 entries, 2 MiB left held. svn 113 → 114/114.
- 738b4e0af: the status red was the test's. Stock svn reports a directory's children in hash order;
  `entries_in_order` compares a status's entries in text order. Red first. Checked against the run's own outputs:
  equal once compared.
- The diag branch merged up to 738b4e0af locally (272513fe4, ~/slates-work/w-svn-diag), unpushed. It's main's to
  push.
- Waiting on main: the macOS rerun; r12's git-2000 number; w-shared's gauge names for item 2.

## 2026-11-22 ~04:35 PST: svn over HTTPv2 closed by use
- Main's CI 45021179639 (macos-latest, Homebrew svn 1.15.0, at 272513fe4 on 738b4e0af):
  stock_svn_over_httpv2_answers_as_stock_svnserve_byte_for_byte 1/1 in 33.54 s; the guard left 0 views mounted.
- Recorded in GAPS in 0235c0134.
- Waiting on main: r12's finding (2) reading. Item 2 waits on w-shared's gauge names.

## 2026-11-22 ~04:55 PST: r12 still red; item 2 landed
- r12 (main, at 1721d8c52): 181,750,880 B not given back against 155,539,920 B live; r4 had 150,287,136.
- 8df631d3c: `an_import_measured` prints w-shared's `mem.gauge.*` rows summed over the shards at every reading, and
  the footprint past system live + mapped resident + arena. Red first; svn_interop `the_` tests 7/7.
- Raised with main: the gate's "live" is the heap's in_use growth, which counts untouched bytes (1.30 GB at a 44 MB
  footprint). The gauges will show whether it overstates live.
- Waiting on main: r13's reading, then the node_record streaming cut if the holders point there.

## 2026-11-22 ~06:30 PST: A-862, the import's texts streamed (main's order of 05:28)
- f159578c3: `Importer::import` takes the commit drive's `TextSink`, and each text goes into it a piece at a time as
  the dump is read, named stored. The daemon's sink is the content store; its holds go to the landing.
- Red first: `an_import_holds_no_text_whole_on_the_system_heap`, 8 × 512 KiB. The system-path churn was 4,299,277 B at
  2bb00e1cc and is 92,309 B now.
- Tests: svn lib 585/585, the svn import tests 7/7, server svn 114/114 (on the merged tree too), store_tests 10/10.
- Filed: main's gauged an_import_measured over git-2000 (run-requests.md).
- Next, per main: the next-largest per-text churner given a buffer the import owns, reused.

## 2026-11-22 ~07:40 PST: r16 read; the post-state cut (main's 06:01 routing, 06:28 request)
- 73e46f842: the batch's admission charges a revision's texts once. Since A-862 they're already in the content store.
- bb0802fb8: an increment's post-state is sized once by its texts. Red first: 6,000 bytes held for 5,000, now 5,000.
  The due_before unit red is folded in: 22,220 under the old charge against 11,220.
- In-process sampled attribution (500 files, 300 revisions, 11.65 MB dump): 168.9 MB allocated at 2bb00e1cc,
  156.6 MB with A-862, 142.3 MB with bb0802fb8.
- r16's daemon doubling isn't reproduced in-process. main queued the daemon A/B (2bb00e1cc vs 73e46f842, interleaved)
  as r18.
- Tests: server svn 116/116 on the merged tree; slates-vcs content 20/20.
- Next: r18's verdict, then the next churner. Mine left: the sink's window buffer, one a text, to be reused by the
  import. The rest are the engine's and the content store's.

## 2026-11-22 ~11:00 PST: the sink's buffer reused; the splice cut held; A-760 proposed
- 1a882f47d: the import's sink keeps one window buffer for all its texts (`ContentStore::begin_object_reusing`).
  Red first. Sampled: 142.3 MB to 130.7 MB.
- r18 (main): A-862 is innocent. r16's doubling was the machine; both bases leave about 560-600 MB not given back.
  r12's base, 1721d8c52, is under r19.
- Held uncommitted, awaiting main's ruling: `engine::apply` sized once (`made_len`). Red proven, but dead even
  (130.7 to 130.8 MB): an import replaces files whole.
- Proposed to main: the svn landing names its stored texts as A-760 named runs, removing the post-state, make_mapped
  and encode copies, instead of shaving judge_held and encode one by one.

## 2026-11-22 ~11:40 PST: A-760 named runs for the svn landing, (a)+(b) as one (main's rulings of 11:05 and ~11:35)
- 91495a96a landed: the splice is sized once (`made_len`). Partial splices: 43,945,798 B to 21,594,432 B.
- Ruling (i): a named svn run the content store lacks at replay is brought back from the node's changeset store
  (A-863's Away wait, the owner's `svn_texts::bring_whole`). The bring-back stays on the cold path, counted, and is 0 in
  a run with no crash.
- Invariant to state in the module doc, with where it is enforced: an entry naming a text commits only once the
  Payload holding that text is durable.
- PLAN (consumers of a built svn increment):
  1. `svn_commit::Building`: a stored text named as a `NamedRun` (`push_named`); op srcs patched after the inline bytes
     are complete, before `doc.canonicalize()`. Red: a counter of stored-text bytes copied into the post-state reads 0.
  2. Read the runs in the build step (`read_step` over `named_objects::PlaneObjects`), counted.
  3. `ImportBatch::stage`: judge with `judge_named` (a new engine `submit_named`, `&mut`), so the buffers become files.
  4. `RecordedBatch::apply_slice` (push.rs ~506): read the step's runs again before `apply_recorded`. The texts are
     still held until the batch's release. Piece (c) would adopt the scratch member's files instead.
  5. The client commit's `land` (svn_commit ~1286), via `version_of`, and `land_made`: read, then judge as they do.
  6. `verbs.rs` ~20399, the replay: on ObjectMissing for a svn green's run, wait as Away; the owner brings the text back
     with `bring_whole`, then reads on. Also check `verbs.rs` ~10715's `apply_recorded`.
  7. Reds: the entry's size independent of its texts; crash oracle killed (1) between the texts' landing and the
     entry's commit, and (2) between the entry's commit and its checkpoint; the bring-back count is 0 without a crash.
  8. Sampled number on the tree dump, sent to main for the daemon A/B.
- Rulings (main, ~11:50): take the pre-pass in the svn green's recovery. It brings back only the named runs of entries
  past the checkpoint, sliced a text a step, its bound stated. The append's `judge_named` is its own landing after
  (a)+(b): red = 0 cloned read bytes; tell w-git and w-hg first.
- ORDER FOUND: `verbs::rebuild_recovered` replays the greens (`rebuild_records`, ~19259) BEFORE the repositories'
  stores (`vcs_store::recover`, ~19300). `bring_whole` needs only `vcs_disk` and a content store, not the svn records,
  so the pre-pass goes in `rebuild_record`'s Green arm for an svn green: make the content store, then bring back.
  Verify `content_made` can run there.
- 39c2b8b07 landed A-865, A-760's pieces (a)+(b) for the svn landing.
  - Increments name their stored texts; entries record names.
  - Recovery's pre-pass (`svn_texts::bring_back_named`) brings back what the content store lacks.
  - Sampled 130.8 MB to 108.0 MB.
  - Reds: 5,000 bytes inline before; the kill-between-slices successor fenced ContentMissing before; region holds are
    kept on the early store.
- r21 (main): 3c1c8b504 (w-shared's gauges) raised the footprint not given back. w-shared is fixing it.
- Next: the append's `judge_named`, its own landing, with w-git and w-hg told first; then piece (c).
- 43a6df642 landed: the shared append judges with `judge_named`. A named work's read buffers move into its verdict:
  50,000 bytes cloned before, now 0. Reuse after a refused append is impossible by construction (append takes its
  changes by value). w-git and w-hg checked and are clear.
- Next: piece (c), the batch's commit adopting the scratch member's files instead of re-reading the runs.
- r29 (main, big Studio): 43a6df642 passed the give-back check 2 of 2, at about 300 MB of footprint. main later found
  the give-back reading bimodal on every arm (r39); two rounds aren't enough to judge.
- Piece (c) as a swap was ruled out (w-engine's fast-forward, paused).
- 4142eb60e landed option 2: `Green::apply_recorded` takes its work mutably and moves a whole run's re-read buffer
  into the file. Red: the file was a copy. Sampled 108.0 MB to 96.2 MB.
- Tests: slates-merge all 17 targets; svn 118/118 (three named runs; quiet runs twice stopped by the watchdog at load
  42-82); git_push, hg_push, store_tests, made_commit green.
- Left per text: two `read_step`s (the batch's re-read would go with w-engine's fast-forward); the content store's
  `release_one` copy (w-shared's).
- a5aa2d576 landed: `ContentStore::release` answers `Freed` (Held, Gone, or Table(bytes)). A freed window is never
  read back out of its storage. Red: 5 windows read back, now 0. Sampled 96.2 MB to 84.3 MB. w-shared was told first.
- Found: the afternoon's watchdog stops in the svn and vcs_store suites were my own uncommitted local probe
  (`finding_2_probe_what_an_import_leaves_held` spawns `ps` blocking on the shard), not load. It's #[ignore]d locally.
- Left per text in-process: the two `read_step`s (~11 MB each); the batch's re-read goes with w-engine's paused
  fast-forward.
- Main's 17:00 rulings:
  1. The DAV flake is mine; neither sighting ran my probe.
  2. Then A-760's items 3-7: the green's block names the plane's object. One item a piece, each with an oracle
     against `apply_recorded`. The engine work is mine while w-engine is paused; tell w-perf when I start.
- The DAV flake, diagnosed. w-git's line: `daemon_edge.rs:408:16` at ~27cae9bf7, the test reader `line_of`: the
  client's read gave up waiting for the PROPPATCH answer.
  - Cause: the hook's pipes are `std::io::pipe()` (`hook_driver.rs` 224-227); on macOS close-on-exec lands a call late.
  - The in-process test binaries spawn stock clients with `std::process::Command`, not `confined_spawn`. One lands in
    that window and inherits the hook's words write end.
  - A first-run hook has no deadline, so its drain waits until the client leaves.
- Proposed to main: (b) end the drain when the hook's process group is empty, red first with a plain std child
  holding the pipe; or (d) confine the tests' spawns. Awaiting the ruling.
- f974b5b13 landed: a hook's words end once nothing of the hook's can write them. The check is
  `slates_ipc::group_holders` (`proc_listpgrppids` on macOS; Linux and Windows' atomic pipes answer that others may
  hold it). A setsid child counts as a foreign holder. Red: 4.15 s held, now ends at the hook's exit. The DAV flake is
  closed.
  - GAPS, OPEN: a first run's missing deadline. The macOS close-on-exec sweep, by file and line, is reported, not
    fixed.
- Next: A-760 (b), items 3-7 (the green's block names the plane's object). Tell w-perf when starting.

## A-866 LANDED d386763ba (merge 275183739), 2026-11-22: A-760 (b) piece 1 of 4
- A block made whole of one named run spanning its object keeps the object identity (`Green::name_of`), through
  judge_named + make_step + commit and apply_recorded alike. No memory change.
- Oracle `engine::named_tests::a_file_made_whole_of_a_named_object_is_named_for_it_until_changed`: red first
  (`left: None`); census moved 355, shared 20, changed away 143, kept 87, stepped 210. slates-merge --lib 276 passed.
- Plan: inbox/main-from-w-svn-a760b-plan.md. w-perf told: inbox/w-perf-from-w-svn-a760-items.md.
- NEXT (main starts it): piece 2, a checkpoint naming a named file instead of its bytes.
- Run request for main: clippy -p slates-merge --all-targets (the lib change and the new test).

## 2026-11-22 evening: main's reds, in its order
- clippy large_enum_variant (A-866 grew Judged to 288 B): FileMaking boxed, Judged 224 B. Landed 8c9e71649 (merge
  28ec7c89d). Rerun asked.
- codex cold copy: bring_texts asks holds_whole, not contains. Red a_subversion_copy_from_a_stored_revision_reads_a_window_that_was_let_go
  (Err(Unstored) -> Ok(3)). Landed (merge 1010c7e7e). Bug record docs/bugs/2026-11-22-a-subversion-copy-refused-...
  Siblings NOT changed (reported): svn_texts::named_lacking (recovery pre-pass) and svn_presenter's bring-back, both `contains`.
- codex bring room: finish_brought abandons its writer on a refused finish. Red a_refused_subversion_text_bring_returns_its_completed_windows
  ((262144, true) -> (0, false)). Landed bc026ec5a (merge 96e6f2170). Sibling NOT changed: hg push/intake.rs:1276 (w-hg's).
- A-760 (b) piece 2 WIP set aside as ~/.cache/w-svn-wip/a760b-piece2.patch (checkpoint named table, codec only); resumes next.
- contains siblings fixed (main's order): named_lacking + svn_presenter ask holds_whole. e5cdfda3f (merge 35657c523).
  Reds: codex a_subversion_restart_replays_a_named_text_with_one_released_window; mine
  a_hook_view_brings_back_a_stored_text_whose_windows_were_let_go (test_store::a_text_whose_windows_are_away).
- shuttle_green ContentUnread arm (E0004 in the nightly lane): 3241ae61e; compiles under --cfg shuttle.
- NEXT: A-760 (b) piece 2 (patch at ~/.cache/w-svn-wip/a760b-piece2.patch).
- named_tests lint fix (7 errors): aca97583a. confined_spawn macOS test allow: cb2a660bf. svn copy test message: e9b2797f3.
## A-869 LANDED c14f88038 (merge bb4551eff): A-760 (b) piece 2, checkpoints name files
- Checkpoint::named section (version 5 kept, old ones read as before); read_step a slice a step; restore refuses unread
  typed (Unrestored). Recovery: GreenReplayPhase::Restoring; pre-pass brings back checkpoint names; replicas refuse
  (merge.replica.checkpoint_unread). Clean-restart test waits for the deferred checkpoint (w-hg's flake).
- Oracle with restores: 113 restores / 148 caught up / 131 by name; red first. Codec golden + hostile + read tests.
- Run request for main: the register split (A-746's row: checkpoints MiB) on the 1,000-revision svn import, before
  (c14f88038^) and after; clippy wall over merge + server.
- NEXT (main starts it): piece 3, the history keeps a named version by name. OWED: copied tables still carried.
## 2026-11-23: remake NotHeld red (from A-869) fixed: cf1529f1d (merge 08854f773)
- remake_green keeps the named texts the plane lacks from the engine it replaces (Green::bytes_named); replay reads
  plane then kept (ReplayObjects). Bug record docs/bugs/2026-11-23-a-refused-publications-remake-....
- Sibling reported: takeover adoption replay has no pre-pass (latent since A-865).
- Reaper: freed 3.4 GiB (target objects + test executables 2.76 GiB, old logs/dumps 639 MiB).
- NEXT: piece 4 under main's ruling (I): 4a identity digest + named::Identity plain BLAKE3 + goldens + layout bump.
- Re-adoption bridge (main's (a)): 9a868c263 (merge 5fc5c9ed5). One planner svn_texts::to_bring; bringers bring_whole_in
  (recovery) and bring_whole (bring_back_named_live); green_adopt::readopt_with. Red: a_readopted_green_brings_back_...
  End to end unreachable until A-727 (d) (w-shared's; told the entry points).
- NEXT: piece 4a (identity digest), under main's ruling (I).
- Helpers lifted to test_store (for w-shared's A-727 (d) fleet reds): eaf17a862.
## A-873 LANDED 75250d9c3 (merge 9dbc985dc): A-760 (b) piece 4a, identity entry hashes a file's digest
- Block keeps plain BLAKE3 digest (named: identity hash); entries from blocks; history/old bytes hashed as read;
  scratch oracle hashes bytes. Goldens: tests/identity.golden + ignored writer + determinism gate; named == carried.
- Anchor layout 19 refuses 18 typed. Side finding withdrawn: object_id is plain BLAKE3 at every length (doc fixed).
- NEXT: 4b (one read seam Bytes|Named), w-git and w-hg told first.
## A-760 (b) 4b LANDED c9c6b6c04 (merge 8ba209cbf), 2026-11-23: one read seam
- engine::Content (current: Bytes(&[u8]) | Named) and ContentAt (at a version: Bytes(Cow) | Named); engine + tests use
  *_bytes readers. 25 server sites moved; git StepRefused::Named, hg Unmade::Named, mount EAGAIN/ERROR_RETRY, work
  BaseGone, page/checkout Unsupported, svn_made Unreadable; svn held texts fall through to the store.
- No producer, so no red (main's ruling). svnsync counters + slowdowns handed to w-svnperf (inbox patch).
- NEXT: 4c+4d (holds + dropped bytes + history through the plane + refuse-and-bring-back + reanchor by identity).
## 4c+4d IN PROGRESS (uncommitted in ~/slates-work/w-svn-c2), 2026-11-23 ~10:30
- Main's rulings: byteless named blocks; holds drained where a green changes; old restore read path replaced; sync
  readers through PlaneObjects, Away typed, async bring-back bounded+counted; splice base read through Objects as runs
  are (refuse only on Away); every bridge (NFS/FUSE/virtio-fs/WinFsp) retries inside its task, never EAGAIN/EIO to
  the kernel; send main the bridge map BEFORE building that part; reanchor checks named replace by identity.
- Done so far: red (oracle holds_model asserts named_bytes_held()==0; was 80). dims: Slot.bytes Option + len,
  NameHold Take/Give, Taken, Blocks::{add_named, content, len, hydrate, let_named_bytes_go, take_holds},
  Files::{get->Content, len_of, insert_named(path, identity), remove->Taken, hydrate, take_holds}.
- Next: fix ~45 engine compile sites (lengths -> len_of; bytes -> Content handling; judge base -> ContentUnread +
  hydrate step), history Source::Block byteless -> ContentAt::Named, reanchor identity, then server drains.
- 2026-11-23 later: ENGINE HALF DONE (slates-merge all green: lib 280 + integration). Local WIP commit d0ccc5933 on
  agents/w-svn-c2 (not pushed, not for landing as is). Engine: lend/let_named_bytes_go by identity index; holds tracked
  by owner (track_name_holds; give_back_name_holds; dropped_with_name_holds bug counter, no thread-local queue per
  main's (5)); crate::unlent scope refuses any verdict/rebase/commit-read over an unlent named block
  (ContentUnread, nothing cached); unlent_reads / recorded_unlent / commit_unlent name them all; apply_recorded and
  commit_judged/make_step refuse (CommitRefused::Unlent); checkpoint read path removed (restore by name, infallible);
  ReanchorRefused::Named; Revert::Named; copy fixes by digest. Oracle: lend loops + holds in step + census
  judges_lent/replays_lent > 0 + dropped_with_name_holds unchanged.
- Server so far (compiles): Restoring phase + remake kept bytes removed; replica restores by name; Greens::take_name_holds
  + given_back on remove/insert/retain/lineage end; vcs_store::name_holds::freeing = the one free decision (Takes then
  Gives, then release_texts / release_unheld_windows) per main's (4); replay lend_step (sliced); verdict_of,
  push judged_lent/commit_judged_lent, apply_accepted/apply_slice apply_recorded_lent, import_batch judge_staged.
- Main approved the bridge map with changes: (1) exact bring-back of the windows covering the parked request's range,
  held until terminal step, no retry counts, re-serve Away = counted bug + transport I/O error; (2) a parked table per
  bridge bounded by its in-flight bound, typed refusal at the bound; (3) fix refining copy_step's swallowed error;
  virtio-fs left (no lend today). Land engine + server halves together; send main the hash.
- NEXT: run server focused tests; sync readers through PlaneObjects + typed Away (green_tree, read_green_range,
  work_files, owner_files/page, checkout, refining, landing, mounted_work, git/hg/svn_made); async bring-back at each
  caller; bridges NFS/FUSE/WinFsp park tables; reds (mount read + work read after text left the green, with window let
  go; work editing named file).
- 2026-11-23 evening: main ruled (B): a landed text's windows go once durable (let_go_landed_texts in every svn
  landing + import batch), counted both sides; P3's test asserts "no window resident" + holders==1 + named_bytes_held==0
  (contains() changed meaning; doc says why). Built: bring_back_range (svn texts by repository via disk.text, else
  read_through), ReadPin pins honoured by landed_pin; call-scoped away notes (away_scope/note_if_away) at GreenTree,
  page_of/whole_of, lend reads; NFS serve_local_parked (table nfs_parked, bound = held connections); FUSE park via
  bridge-fuse dispatch_kept + Dispatched::into_request (one a mount); WinFsp park via Request::kept/refuse_io (one a
  mount); verb-level park in run_recorded (reads + submit, unrecorded, resume_all over pins, bound = clients slots);
  refining copy_step retries over brought-back windows (no lost error). Engine: keeps_base/apply_over (whole replace
  needs base length only), digest fast paths in differing_spans/lands_on_head, copied_edit/effect_of.
- FUSE (linux) and WinFsp (windows) compile unchecked locally (no cross C toolchain): main to run platform checks.
- Server svn focused: down to 4 failures; reds written: a_work_over_a_subversion_green_reads_and_edits_a_named_text_
  its_windows_let_go, a_mount_read_of_a_named_text_let_go_is_noted_brought_back_and_read_again.
- OWED after 4c/4d (main, from w-git's push profile): (i) a read-only lend (is_owing, branch_version) takes and restores
  the whole Lineage by value (~10k memmove samples / 30 s push): borrow instead; (ii) each lend makes and drops one
  Green::default (Lent::new over the slot, previous Lent's Drop mem::take). Reds: counters of lends taken vs borrowed,
  and Greens dropped per lend, asserted after the fix.
- OWED after 4c/4d (main's FYI, r270 vs r70, w-svnperf done so these are mine): piece_fetched 14.7/revision on svnsync
  (A-876's bar <10; fetch_wait 1.86 ms vs ~1 ms expected); per-piece StoreHandle::ask room round trip on shard 0
  (vcs_svn::converse -> follow -> svn_texts::fetch_piece -> Room::drop, 418 samples / 60 s): batch a conversation's
  piece asks, or keep one room per conversation. Profile ~/.cache/main-rounds/svnsync-confirm/sample-svn.txt.
## A-760 (b) 4c+4d LANDED main 240917592 (2026-11-23 ~20:40)
- Commits: be143a60a (4c+4d), 2609663a5 (TLS teardown: STATE/LOST_STEPS/UNDELIVERED/TURNS/LIVE/KEPT/DISCARDING
  try_with, red a_daemon_stopped_with_a_landing_turn_held_ends_without_a_panic), dcd7bab5f (platform gate fixes:
  FUSE Parked, u64 casts, complexity splits dispatch_parking/next_request/settled_reply, import test harness supplies
  owed bases), merges 7e92647b0, bc5621949, 240917592.
- Gates: macOS focused 230 + merge 280 + fmt; diag 45325338128 (Linux FUSE + Windows WinFsp tests green; clippy reds
  main's vcs tests.rs:821); cross-clippy zig/xwin clean for my crates (main's daemon_edge.rs lints reported).
- Cross-clippy recipe from this Mac: Linux CC_x86_64_unknown_linux_gnu=~/Library/Caches/cargo-zigbuild/0.23.4/
  wrappers/1861/zigcc-x86_64-unknown-linux-gnu-8781.sh (+CXX, AR=.../ar) cargo clippy --target x86_64-unknown-linux-gnu;
  Windows cargo xwin clippy --target x86_64-pc-windows-msvc. Use --no-deps.
- Set aside: /tmp/w-svn-4d/svn_import_probe.patch (pre-existing uncommitted finding_2 probes in svn_import/tests.rs).
- NEXT: lend items (borrow on read-only lends; no Green::default per lend), svn piece_fetched 14.7/rev and per-piece
  room round trip; made-commit held verdict re-lend gap.
## A-888 LANDED main a018ee86b (2026-11-23 ~22:00): goal 93 C2 "store is the truth, no second path" (piece 1)
- Item 1 answer to main: C2 whole on every production route; the bare-dispatch path (svn_made::make: version committed,
  revision landed by a task after) was a second path in code. Removed: take_turn answers Taken::Unrecorded for a svn
  green with no recorded request -> Refusal::RecordedSubmitRequired { tool } (wire tag 85, golden 5500000002000000; API
  Invalid::RecordedSubmitRequired, contract 63); a held verdict with no turn refuses the same. Commit 6495fa96c.
- Red: a_submit_to_a_subversion_green_outside_a_recorded_request_is_refused_and_moves_nothing (main: accepted, head 1,
  youngest 0). Tests moved to recorded submits: paths::tests::submitted_recorded; identities message/identity tests.
- Pre-existing red on main (not mine to fix in this piece, reported): vcs_http::svn::tests::the_daemons_lender_serves_a_
  repositorys_committed_records (svn.rs:635 "the text": lender reads a stored text through the records alone).
- NEXT piece 2 (main 20:51 approved, guard-then-apply): (1) made_commit_landing commits the version alone when no
  revision can be made (Unmade) or the check refuses -> refuse typed+recorded, nothing applied (Unsupported for special
  /link-with-props/copy-not-made, StoreRefused for check/NotNext/Unreadable/Digest); (2) made_commit::commit_held runs the
  register entry's commit after commit_accepted and commits the version even when it errs -> guard all of commit_made
  before apply. Red written (uncommitted): a_special_file_refuses_its_submit_and_the_repository_takes_the_next.
- Sweep for main (others' owners): git commits.rs Before::Unmade commits version alone + record_and_commit Err commits
  version (counted UNMADE); git commits::make_owed (version committed, commit made after by the queue); hg_made.rs
  made Err/None and Unmade commit version alone, commit_made Err commits version; hg_made::make (bare path, recorded in
  the same transaction); made_commit::commit_held shared by all three tools (tool Err still answers the accept).
## A-891 LANDED main 891de2741 (2026-11-24 ~01:00): svn made commit guard-then-apply (piece 2)
- Commit ff000bf3b (subject says A-890; renumbered A-891: w-chaos's unclaimed A-890 was on main first).
- Unmade/check-refused version refused typed+recorded (made_commit::refuse); register entry guarded before the verdict
  applies (made_commit::commit_guarded, MadeEntry::guard, push::check_entry, GuardedMade::commit).
- Reds: a_special_file_refuses_its_submit_and_the_repository_takes_the_next, a_made_entry_its_guard_refuses_leaves_
  nothing_of_the_version (both Submitted v1 on a018ee86b). t_a664_svn_an_edit_beneath_an_earlier_copy_is_a_revision.
- Load flakes seen (pass alone): vcs_ssh tunnel hooks test; verbs an_unbuilt_works_shard_asks_its_greens_owner_again.
- NEXT: import BudgetExceeded on 23 shards (batch reservation > green_chain_bytes cap; scripts /tmp/w-svn-imp/{red,fix}.py),
  then dump order (patch /tmp/w-svn-p2/dump_order.patch: sibling deletes are APR hash order), then svn info 500 diag.
## A-892 LANDED a51883940; A-893 LANDED d27106abc (2026-11-24 ~02:20)
- A-892: import batch within the green's retention room (Flush::Retention, Deferred); lone revision past room refused
  ImportFault::RetentionPastRoom (tag 11, contract 64). Owed (GAPS): batch chain entries vs green_chains cap.
- A-893: depth-1 PROPFIND of a collection streamed (PropfindWalk, 207); 500s name their cause; edge logs 5xx.
- NEXT: dump-order normalization in crates/cli/tests/svn_interop.rs (uncommitted in worktree, unit test
  a_dumps_order_of_a_directorys_deletions_is_no_part_of_an_answer); then speed list (writes 24x, svnsync piece_fetched,
  svn:// update), lend borrow items, made-commit held verdict item 2, virtio-fs park.
## 2026-11-24 ~03:30: held-verdict answer 4c73eb315 (never differs; costs a run), A-895 chain room cda02d89d
- A-895: batch takes its chains' room; judge_named → encoded_len → commit_judgment; PastChainRoom deferred whole
  (Flush::Chain); lone entry past room = typed TableAtCapacity; Loaded::batches logged at import end.
- NEXT (main's order 02:25): 3 virtio-fs park; 4 lend items (borrow read-only lends, no Green::default per lend);
  5 speed (HTTPv2 writes 24x, svnsync per-piece room round trip, svn:// update; apply bring-back before the made step).
## 2026-11-24 ~04:15: stage_owed lint split eea0c46d7; A-898 virtio-fs park 40f7c2414
- A-898: BridgeAccess::with_bridge hands an AwayProbe (one signature); device keeps one away chain unpublished; loop
  awaits bring_back (server: bring_back_reads, Send) and re-serves first; second miss answered (away_again).
- Owed (main): Linux guest over a work on an svn green, crates/server/tests/virtiofs.rs.
- NEXT (main's order 02:25): 4 lend items (borrow read-only lends; no Green::default per lend); 5 speed.
- 11-25 later: A-920 (2b sliced staging, main 01a20182 ruling): piece 0 2d6ede61a (reservation once; judge step 262 -> 227 ms) + docs f8a812d15; piece 1 0214fa210 (scratch commit on A-917 run; judge step 227 -> 89-92 ms CPU at 9,000; whole left: prep sole->mapped 6.8 ms, commit end+keep 13 ms, records step 30 ms; hog test HOG_NS 150 ms). Next: piece 2 record check (101 ms), 3 judge, 4 build, 5 held tables+apply; then apply whole parts; fmt3 p99.
- 11-25 later: A-920 piece 2 5c5783cdd (record check plans rows a slice a step; check step 101-106 -> 17 ms CPU at 9,000; golden + generated oracles). Hog hold derived from measured flush d02143977 (w-perf told by inbox). Next: piece 3 judge (57 ms, reservation as a walk over the ops), 4 build 51, 5 records 30, commit whole parts.
## 2026-11-24 ~05:00: A-899 virtio-fs serves a work, 7416cae15
- virtiofs attach ensure_slot; pass lends GreenTree via mounted_work::source/lend (one lend); A-898 park now reached.
- Integration case a_guests_read_of_a_let_go_subversion_text_is_brought_back_and_answered (in-process seam, unix).
- Scratch moved out of /tmp to ~/.cache/w-svn (CLAUDE.md forbids /tmp).
- NEXT: item 4 lend items (in-place lend: no by-value move, no Green::default per lend; counters red first); 5 speed.
## 2026-11-24 ~05:20: A-900 lend in place, 3d65116a6 (item 4 done)
- Lent::new swaps arenas into the seated engine; drop swaps back; Lineage::lend_counts (taken, borrowed, dropped).
- A-899 held on Linux (run 45395756328, 32/0/1 twice).
- NEXT item 5, speed: HTTPv2 writes 24x (0.6 vs 14.7 s); svnsync per-piece room round trip (piece_fetched 14.7/rev);
  svn:// update (round 3: 0.999 vs 0.891 s); bring-back before the made step (A-891 GAPS note).

## 2026-11-24 ~09:45
- Import spin closed: red `t_c2c_a_revision_of_thousands_of_files_imports_with_every_step_under_the_shards_window` (b2bea8d2c); cause A-887's cubic replay_reads, fixed by w-perf 5606962eb.
- Diff r251 red was the harness: canonical_tags on non-XML answers (a0c46b705, `a_diffs_text_is_compared_as_written_whatever_its_blocks_order`). Main's next HTTP round confirms nothing else differs past line 362.
- read_named O(R^2) fixed (a0c46b705, `a_works_runs_are_read_a_slice_a_call_across_runs`).
- Next: locate the judge's superlinear part (batch.stage 290 ms @3k files vs 1,865 ms @6k, debug) by a counter, then fix; then item 5.1 (HTTPv2 writes).
- 10:30 judge quadratic landed 7861187a4: Post::run_places index, alone by counts, holds_place binary searches; import red at 6,000 files 1,511 ms -> 592 ms (debug). Left: per-revision slicing (linear one step), Post::span/len walks. Next: item 5.1 HTTPv2 writes unless main reorders.
- 11:50 lint 42589bb44 (green_adopt split, noted_in_scope cfg(unix)). Slicing part 1 landed 1efdd7b45: RunTable index for Post (b271cc280), yields between staging/apply/tail sections (97cbebc2f); import red 9,000 files 1,208 ms -> 473 ms. Next: slice the tail and stage_texts by budget; then staging step (judge) and apply_recorded (merge, shape to main first); then 5.1.
- 13:55 tail slicing landed 8658733f8 (TAIL_SLICES counted, hand_over by set). E160004 Corrupt fix landed 92516ced4: Due::Text pause in the commit drive, BroughtBases on svn://, SSH, HTTP; refusal sites logged; svn:// red + matrix both routes 31/33 locally. Left: HTTP in-process red, drive-refusal log via serve, git-2000 write-step measure (main), slicing pieces 2-3 (shape to main first), then 5.1.
- 19:45 landed 09b2f0d3c: A-910 incremental fold + oracle (949ca8619), staging_steps stub reverted + TSan skip (2cb9633ba), tail identity sliced + texts_carried by lookup (2f340066f). Earlier: drive-refusal logs 1174cb060, TSan crash test 8c0d8fce8, slicing 2a 30195b30e. Import max step still ~490 ms = checkpoint task's first-turn identity fold (A-579 turn_budget_ns: interval 0 -> all at once); asked main. Next: piece 3 (Applying), 2b.
- 22:30 landed: 964888c59 first checkpoint's identity a quantum a turn (zero interval only; 2 reds); 1af90f6e4 pace test waiter mid-step (84a property); ac3bf0650 landed-pin counters by kind + crash test evidence message. All-verbs diffs at 3e910809f: comparator's, fixed on main (c94252e87, 71c2f07a7), 0 diffs at a61f1dfd4. TSan crash leak: no native red (images don't restore a window whole); fix waits on a failure showing whole-at-start. Import max ~480 ms = pace ceiling with no waiter, by design. Next: piece 3 (Applying), then 2b.
- 11-25: landed 085f6dfad (site 4: recovery lets go named texts found whole) + d50169633 (resident honest for one-window objects); d3539309d (Unstored on Linux texts bound: shared windows; cut oracle); f8fd11504 (sites 2,3 measured and fixed; site 1 and step refusals measured clean); a55d7cfa6 (svn on rule (B): pin at seal, unpin at landing/every end; scoped pin retired; between-batches red). HTTPv2 revprop PROPPATCH: CLOSED (fixed by d843d2325, w-interop green at b2888af84). All-verbs diffs at 3e910809f: comparator's, closed. P7 flush cut: w-perf's. w-balance admission: go given (inbox). Next: piece 3 (patch ~/.cache/w-svn/piece3-commit-run.patch), then 2b.
- 11-25 later: d2383560f (unpin at durable landing; w-perf's store_tests red); 47eb329ab (TSan crash test: store made before judging, every named run counted; native red via PUBLICATIONS_LEFT); 3cc90d154 (brought/found split). MD5/SHA-1 item closed (63ab25b9f). Next: piece 3 (patches ~/.cache/w-svn/piece3-applying-engine.patch; applying_tests.rs to rewrite from session).
- 11-25 ~04:00: landed 65289bd4a (both recovery paths at every kill point), 90b7ccd74 (export piece knob), b2256e401 (step_quantum_ns/copy_slice_ns logged), 5d426b725 (A-917 piece 3: Applying; apply slice 196 -> 71 ms at 9,000 files). Owed in A-917: named read at begin, recorded_unlent walk, commit preparation. P7 review given; w-perf landed 93d7dca13 with both points. Next: 2b (resumable judge), shape to main first.
- 11-25 ~05:10: A-917 owed phases cut (ruling (a)): 04fe9cc26 (spent() not APPLY_UNITS; named read sliced), 17d865200 (ApplyStep::Lend, no recorded_unlent walk), 596e1b95a (commit prep phases; retention/checkpoint own slice). Apply's longest slice by CPU: 29-77 -> 7.3-7.7 ms at 9,000 (debug). Still whole: begin_applying replay check (4.3 ms), prep transitions. Export fmt2 measured by main: median 23.1 us <= 24.9 us bar. Next: 2b shape to main (unless main wants the remaining 4 ms first); then fmt3 window p99 512 us.
