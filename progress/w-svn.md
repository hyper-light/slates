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
