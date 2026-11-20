# w-shared -- UI goal 93's shared pieces of §4.17.1 (supervisor "slates-7a")

## NOW: A-636 part 2c split (told main): 2c-1 = repository record as own register (REPOSITORY_CLASS 6, seq =
  policy.version + names pos + 1, fleet::owed_repositories, takeover adopt_own -> ShardState::pending_repositories);
  2c-2 = successor rebuild by part 1's rule. 2c-1 BUILT uncommitted in ~/slates-work/g261-a636p2b2 (on main eea7e6497),
  docs written; run: server fleet::state_log_tests takeover:: state_log mirror:: repository::tests (pending).

## LANDED 2026-11-20: A-636 part 2b-2, commit 4c38e425e, main 3fee2f575..eea7e6497 (merge eea7e6497). Worktree
  ~/slates-work/g261-a636p2b2 (keep for 2c's base or remove). NOW: A-636 part 2c (successor adopts a repository's logs and
  records whole). Note for 2c: nothing ships a repository's RECORD (name/tool/format/owner/policy) to holders yet.

## NOW 2026-11-20: queue item 1, A-636 part 2b-2 (route by id, family ids). Worktree ~/slates-work/g261-a636p2b2 (branch
  agents/g261-a636p2b2, ff'd to 9f51d7525, change uncommitted). Main AGREED placement follows the family (A-333). Built: db
  register family ids + line_key, Partition::family_counter (snapshot), Db::family_number_at, in_family join rule; server
  verbs::fresh_family_id_at (repo, root, branch, push create, lone green); design status + amendment entry, GAPS. Target
  ~/.cache/w-shared-local-target (own). Runs: db lib register:: repository:: snapshot 124/124. Next: db --test wire_order,
  model; server lib repository:: tests; fmt; commit (msg ~/slates-work/w-shared-scratch/a636p2b2-msg.txt); merge; push;
  amendments.tsv A-636 row; then 2c. 9f51d7525 re-run (own target) 10/10 green, reported.

Queue (briefs/w-shared.md): 1. A-664 part 3's commit (g308) + follow-ons; 2. A-636 part 2b-2 then 2c (g261);
3. A-697 (a)-(f) then A-658 3c (g305). Studio d1323898, sync false, CARGO_TARGET_DIR ~/.cache/g308-target, JOBS=2.

## LANDED 2026-11-20 ~05:00Z: branch's first map from its source's, main 9f51d7525. Follow-ons left: holders' maps (BLOCKED:
  holders hold no vcs store; needs A-636's fleet half), copy through a work (BLOCKED: no client copy verb), sliced whole
  builds/releases, hg and svn objects. Then queue 2 (A-636 2b-2/2c), 3 (A-697, A-658 3c). Worktree ~/slates-work/w-shared-mode-land.

## LANDED 2026-11-20 ~04:45Z: mode fix main 73cbdfc42. NOW: piece 1 follow-ons (holders' maps first; g308 brief).

## LANDED 2026-11-20 ~04:30Z: piece 1 A-664 part 3's commit, main 6dfb8fc0c (c6f7976c3 + merge). NOW: piece 1b, the mode fix
  (chmod+x then mv; test ~/slates-work/g308-scratch/mode-test-for-next-piece.rs). Studio clone ~/.cache/w-shared-clone, target ~/.cache/g308-target.

## RELAUNCH (2026-11-20 ~04:20Z, fresh w-shared): p3k had died (signal 15 at 04:06Z, the session exit). Rerun as kiln r2
  (same command, SKIP_RED=1 studio-p3j.sh, logs ~/.cache/g308-logs-p3j on the Studio). origin/main still b63f8a05d.

## EARLIER (2026-11-19 ~20:05 PST): piece 1, A-664 part 3's commit
- Branch agents/g308-a664p3c at 09a38d73d (merge of main b63f8a05d = origin/main at start). p3j run had died at clone.
- Studio run p3k = SKIP_RED=1 ~/.cache/g308-scripts/studio-p3j.sh agents/g308-a664p3c (logs ~/.cache/g308-logs-p3j),
  local output ~/slates-work/g308-scratch/run-p3k.out.
- NEXT on green: fresh worktree off origin/main, `git merge --squash agents/g308-a664p3c`, runs bullet, one commit, push.
  Then piece: mode fix (test ~/slates-work/g308-scratch/mode-test-for-next-piece.rs).
