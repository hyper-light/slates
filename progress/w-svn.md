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
  MD5+SHA-1 rate (like archive_slice_bytes' BLAKE3 rate): waiting on main's ordering.

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
