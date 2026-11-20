# g225 — progress (rewritten 2026-11-18 ~23:50 UTC from the agent's own context after the ~/.cache wipe)

Agent: Claude subagent "g225" (was g225-forming-fleet-reaches, kiln goal 225, part of UI goal 78).
Rule (Ada, final): ONE piece -> failing `--lib` unit test (red once) + fix + design/GAPS/bug-record lines -> the only
compiling is `cargo test -p <crate> --lib <filter>` + `cargo fmt`, on the Studio, waited for inside the turn (clone over
https; ssh has no key there) -> fetch, merge origin/main, `git diff --stat origin/main HEAD`, `git push origin HEAD:main`
-> END with progress, run requests, a 3-line report. No clippy, xtask, cross or other-platform builds: file them.

## NOW: goal 81 (UI #81 dbe13f39), my part 01a1af515aa6705aac9a155dd6fd0e36 "A forming fleet reaches every candidate at start (live-fixes)", resumed 2026-11-19
- Main's order: take "the 48th failure's cause" first. Read: the goal text's "47 of 48" is live_chaos's fresh-member flow
  at one CPU with learner-3; its one failure (the formation wait never ending) is A-331's cause (a fresh member joined
  before its admission kept contact with no one), fixed on main ddcf00768 with its failing-first test
  (consensus::a_fresh_member_joined_before_its_admission_keeps_contact_with_every_live_member). Told main.
- Open, no failing run on current main: (a) the stuck dial (keep/fail-w8-29.log on the Studio, 2026-11-18 10:19, the
  two-region test a_placed_snapshot_is_held_whole_by_f_plus_one_hosts_of_the_mirror_region: the region-0 bootstrapper
  12049..'s record dial to 2667.. one call for 15.8 s, budgets_spent 0; 2667.. had joined from 12049's state; the root
  group's joint changes began; council voters stayed [12049..]); diag-formation-4: 0 failures in 2,417 runs;
  (b) the term-1 joint entry (CI arm64 run 7); (c) the Windows two-region root (run 44298844583; lead
  consensus.root_join.from_another_peer 1). Read for (c): adopt_fetch refuses JoinFromAnotherPeer when the join state's
  raft id is not the answering peer; join_from rewrites a donor's id to the learner's own (root_group.rs:341), so a
  learner's answer carries its own id; on-time and late fetch replies are paired with the session's own host. No cause
  found yet. Asked main for a current failing run's log; else start (c) as a failing --lib test.

## DONE: A-637 (UI goal 93, §4.17.2 item 5: a submodule's link as a special node), LANDED main ad75339d3 (c3de50ef5..ad75339d3), 2026-11-19
- Worktree ~/slates-work/g225-a637, branch agents/g225-a637-gitlink. slates-merge: SpecialKind::Gitlink { commit:
  ObjectKey } (kind byte 4: FIFO fields + id len + id; 1..=32 bytes, BadKind/Truncated refusals), SpecialKind::same_type
  (link to any commit one type; other kinds whole value), engine merge_special uses it; A-618 push: Mknod (no owner,
  time, mode 0), move = Unlink+Mknod, remove = Unlink; PushRefused::Submodule removed; build lays Mknod node bytes.
- Two defects found and fixed in the piece: (1) the build never laid Mknod payloads (any node a push made would be
  refused as truncated); (2) the engine judged any SpecialKind difference a type change (every link move TypeChanged).
- Runs: red 5/96, then 2 (engine rule), green 97/97, merged 104/104 + git lib 338/338; mutations 3/4/1/3 caught.
- Owed: a link's form in a volume and checkout (git: empty directory); the server never meets one yet.
- MY MISTAKE on A-638's landing: merge c533da2b7 committed GAPS.md conflict markers (my merge output was piped through
  `tail -3`, hiding GAPS' CONFLICT line; only the design doc was resolved before `git add -A`); they reached main in
  c14a01278; main removed them (75f8b0e0b, 0727f8303). Rule now: after every merge, `git diff --name-only
  --diff-filter=U` and grep every touched doc for `^<<<<<<< |^=======$|^>>>>>>> ` before committing (done for A-637's
  four merges: zero markers each time).
- NOT STARTED (no room left in this session): the `.gitmodules`/`.gitattributes` blob checks (git fsck_blob: the
  gitmodules config parse and its url/path/name/update rules, gitattributes size and line length, symlinked
  .gitattributes); they need git's config-file grammar and golden vectors from t7415/t1450 with stock git 2.50.1.

## DONE: A-638 (UI goal 93, §4.17.2 pieces 5+6: the store as the push path reads it), LANDED main c14a01278 (dc7418022..c14a01278), 2026-11-19
- Worktree ~/slates-work/g225-a638, branch agents/g225-a638-store-push. crates/git/src/store/views.rs: IndexedObjects
  (receive::Repository: index alone, holds by row, one-window lent as kept, long bases kept in never-moving OnceCell
  segments charged to the view's room, refusal named) and PushedObjects (A-618 PushObjects: index then quarantine).
  objects/tree.rs Entry::canonical_mode = git canon_mode (stock git 2.50.1 table on the Studio). repository.rs
  `indexed` pub(crate) (one word in A-614's file). merge Unavailable::{Malformed, PastRoom}. slates-git -> slates-merge.
- Runs: red 7/7 vs stubs; green 294/294 then merged 333/333 (+2 ignored), merge 92/92; mutations: holds-lends 2,
  no-reuse 2, standard-modes 1, no-zero-copy 4, room-uncharged 1. Main's A-614 part 2 added Incoming.depth mid-piece.
- Proposal for main: a Cow return on unpack::Outside / receive::Repository would drop the kept arena (A-608's, A-620's).
- Studio leftovers KEPT on purpose: ~/.cache/g225-diag-*, g225-forming-fleet-reaches-src, g225-scripts, g225-logs,
  slates-work-target-g225-diag (4.1 GB) back run-requests.md line ~546 (formation diag loop) -- delete once main ran or
  dropped it.

## DONE: A-618 (UI goal 93, §4.17.1 shared piece 4: works from a client's push), LANDED on main dbdfc253f (f141e2843..dbdfc253f), 2026-11-19
- Worktree ~/slates-work/g225-a618, branch agents/g225-a618-push-works. Code: crates/merge/src/push.rs (new module
  `slates_merge::push`: `PushObjects` trait (tree entries, blob_len, blob), `ObjectKey`, `EntryKind`, `Push`, `build`,
  `Reads`, `PushRefused`), push/tests.rs; engine: the root "" always present for modes/attributes (increment.rs
  `present_directories`, engine.rs `merge_setmode`/`merge_set_xattr`); a removed file/dir/link drops its mode and
  attributes once nothing stands at the path (`commit` + `drop_metadata`, shared with `remove_special`).
- Commits: b90f1d4e4 (tests first, stub) + c0b9eefc2/c78d251aa (test compile fixes), 58fe96d93 (walk + root), c13c23c2b
  (census runner), 74833c9ff (removed-metadata test, red) + 64ce04c0b (fix), fbe44ce2a/6eafb524d (docs), c8765a985 (cap
  dropped for A-621), merges 3309fd2cb, 61cd46b91, 409fe0172, dbdfc253f.
- Studio: red 11/11 (stub + root absent); metadata test red before fix (mode 0755, attribute, dir 0700 kept); green
  88/88 merged with A-621 (23.82 s); red 4 ways by mutation (no-skip 3, mode-ignored 2, engine-root-absent 8,
  retarget-no-unlink 2). Census over 256 histories met skipped subtrees, kind-change refusals, rmdirs, modes, links,
  whole replacements. Lesson: never two checkouts of one workspace on one CARGO_TARGET_DIR in a run (the second ran
  the first's binary: cargo hashes members by workspace-relative path).
- Decisions (main approved 2026-11-19): root = empty path; submodule links refused typed until the special-node kind
  exists; kind changes refused typed. TooManyNames dropped after A-621 (64-bit path indices); the room bounds a push.
- Left (next pieces, main starts them): (1) FILED as A-637 (unassigned, amendments.tsv): the special-node kind for git
  submodule links (§4.17.2 item 5; cross-crate:
  merge special.rs encodings + server merge_service/verbs/work_log), then drop `PushRefused::Submodule`; (2) git's
  store implementing `PushObjects` (A-614, g083) and receive-pack submitting the work (piece 6); (3) the server's
  checkout passing over `slates.vcs.*` attributes; (4) one post-state assembly shared with `verbs::assemble_post_state`;
  (5) kind transitions in the deriver (GAPS §8f); (6) my A-590 leftover: `.gitmodules`/`.gitattributes` blob checks
  (fsck_blob) over the ids tree::fsck hands on (no room in A-618).
- Filed: run-requests.md 2026-11-19 g225 A-618 (clippy x3, xtask, docs, merge integration + server suites, checkout).

## DONE: A-590 (UI goal 92, 51cef9f7): slates-git §4.17.2 piece 4, LANDED on main c2d33ed26 (0a4f1bbfc..c2d33ed26), 2026-11-19
- Worktree ~/slates-work/g225-a590, branch agents/g225-a590-git-objects (pushed). Commits: bb268f45c (module, tests,
  fixtures), 9eb58992c (E0618 fix: a test shadowed the fixture helper; rustfmt at 100 columns), 2c90d5bc0 + 7f1aded8a
  (docs: §4.17.2 status, A-590 log entry, GAPS §4.17 paragraph), merges 9fd2d6a33 and c2d33ed26 (doc ends only:
  main's A-582 part 3, A-591, A-596, A-602, A-607, A-588, A-605, A-581 first, then A-590; lib.rs merged clean).
- Code: crates/git/src/objects/ (mod, tree, commit, tag, fsck, headers, ident, dotgit, refname, hex), on A-600's
  ObjectFormat/Kind/ObjectId; tests crates/git/src/tests/objects/ (golden, tree, commit, tag, names, hostile).
- Fixtures: crates/git/fixtures/fsck/ (make-fsck-fixtures.sh, manifest.txt, SHA256SUMS, table.rs, 360 objects: 90 per
  format + 180 SHA-1 name objects from t0060/.git/tag-name lists), run twice on the MacBook with Apple Git-155, byte for
  byte equal; the predecessor's 158 objects reproduced exactly. Predecessor's untracked draft backed up first:
  ~/.local/state/slates-fanout/g225-a590-predecessor-draft-2026-11-19.tar.gz (superseded; delete when convenient).
- Studio: branch 51/51 (29 new, 16.65 s); merged tree 105/105 (15.56 s); fmt clean. Red five ways: commit check
  answering empty (8 failed), U+FFFE accepted (2), backslash segment instead of rest (1: golden), duplicates only
  between neighbours (3), mode judged before the next entry is decoded (1: bad-tree-mode-unjudged read
  error:badTree,warning:badFilemode vs git's error:badTree). Studio scratch + target (455M) deleted.
- Found while building: git's tagger quirk (a bad tagger before an ignored extra header is reported and accepted;
  slates refuses: the one divergence, documented); ~1000000 and ~9999999 match all four special names in git (the
  fall-back short-name loop skips the prefix after a leading ~); Rust's UTF-8 reader accepts U+FFFE/U+FFFF, git's does
  not (handled); push checks are strict (index-pack uses FSCK_OPTIONS_MISSING_GITMODULES, .strict = 1).
- Filed (run-requests.md 2026-11-19 g225): clippy on three OSes, xtask (typed zeros `0_usize` may read as `0_`, same
  as hg/svn/rt ship), docs with -D warnings + xwin doc, and the fixture script under Linux git (informational).
- Left (next pieces, main starts them): the .gitmodules/.gitattributes blob checks (fsck_blob) over the ids
  tree::fsck hands on; the push path's use of the checks (piece 6); per-id severities from configuration if policy
  ever needs them.

## Goal 81 lead (c) read 2026-11-19: the credit of a fetch reply is correct; no fix, A-589 not taken
Main's ask: find where a fetch reply is credited to a peer other than the one whose join state it carried (Windows
two-region root, run 44298844583, 470.. counted root_join.from_another_peer 1 and join.from_another_peer 1).
- Every reply is paired with the host its own request went to: on time, `broadcast` returns (host, reply, endpoint) per
  request (fleet.rs root learner fetch ~9720-9750, the council learner alike); late, each straggler task sends
  `Reply(host, ..)` for its own host (cluster lib.rs `Stragglers::recover_replies`), folded by `fold_late_replies`.
- A session is the host it was dialed for: `client_for` pins the peer's own certificate from the manifest
  (`Endpoint::client(.., certificate, ..)`), so another node cannot answer on it.
- A node's raft id is always its own `fleet.host()`: groups are made with it (daemon.rs boot learners, consensus.rs
  bootstrap and adopt_fetch's replacement), `join_from` rewrites a donor's id to the learner's own (root_group.rs:341,
  config_group alike), `restore_from` refuses another id (ReusedIdentity), and nothing sets a FleetNode's host later.
- So `join.raft.id != peer` (consensus.rs adopt_fetch) can be met only if the asker knows the answerer under a HostId
  other than the answerer's own `fleet.host()`. Hypothesis, not shown by the run's logs: a node that came back under a
  new member id while a peer still names it by its old one; the certificate pin cannot tell the two apart (one
  certificate across incarnations). The run's two refusals were each counted once, and 470.. later held C_new, so they
  did not keep it out of the groups; why the two new root voters never elected each other is still not found.
- (d) read: a non-voter's known voters are its adopted configuration's recorded voters (`known_voters`/`known_voter`:
  config_group.rs ~1377, root_group.rs ~414), and `adopt` takes only a newer version (root_group.rs ~890), council
  (A-380) and root (A-406) alike: a learner is no longer held to its join state's voters, which was the 409 s stall's
  shape. The one-CPU learner run is filed for main (run-requests.md 2026-11-19, g225, goal 81 (d)).

## STOPPED: A-580 (UI goal 89 part 60178ceb, the multi-runner test with injected faults) -> passed to g232, 2026-11-19
No code written, nothing to commit (the kiln clone ~/slates-work/g225-a580-kiln had no changes; removed). Reading done,
for g232:
- Where it belongs: a new `mod fleet` inside kiln's `crates/kiln/tests/slates_e2e.rs` (reuses Instance, Project,
  workspace_of, op, slates_log, tree, nonce, canonical) driving three runners on one Mac through kiln's REST API.
  - Server: `common::start(dir, &common::Options { extra: "[slates_fleet]\nf = 1\n[slates_fleet.nodes]\n..." })`;
    `[slates_fleet.nodes]` keys are the runners' `--name`s (sanitized: lowercase, digits, '-', '.'), values
    `127.0.0.1:<base>` with a free block of two UDP ports each (base, base+1).
  - Runners: as vm_e2e's private `runner()` (common::kiln(), `agent --server --token --name --work-dir --state-dir`,
    `KILN_MACHINE_ID=<name>`, HOME of its own) plus `Instance::env()` per runner (`KILN_SLATES_INSTANCE=<unique>`,
    `KILN_SLATES_DIRECT=1`, `KILN_SLATES=""`) and `vm_e2e::stub_host_env` for the stand-in harness.
    vm_e2e's `Fixture` has private fields: add a pub `Fixture::of(port, admin, name, dir, runner_pid, log)`
    (`_server: None, _runner: None`) to reuse `vm_e2e::open`, `Pace::until`, `op`.
  - Turns: the stand-in's prompt words `WRITE=<f> APPEND=<f> REMOVE=<f> LAND` (tests/fixtures/vm-stub/harness/stub);
    each finished turn (STUB_DONE) is an acknowledged change for the ledger; read back through the workspace's mount.
  - Move: `POST /api/ops/agents.move {"agent": id, "to": "<runner name>"}` (harness/ops.rs move_to ->
    relocate::start); refusals say `... [kind: x]` (`slates_move::MoveRefused::reported`). A pure parser of that kind
    (`refusal_kind(text)`, inverse of `reported()`) was the planned lib helper, its --lib test failing first.
  - Faults, one at a time, the fleet whole between: kill a runner (SIGKILL its instance's processes, found by
    `of_instance(&vm_e2e::host_processes()?, name)`, and its runner process); partition one (SIGSTOP anchor + daemon
    past the detector's horizon, then SIGCONT); restart one (SIGKILL the daemon alone, its anchor starts it again);
    lose a link mid-move (agents.move to B, SIGSTOP B's instance mid-move, SIGCONT). Kiln's manifest has no `bind`
    (slates A-277), so slates' fault relay (crates/cli/tests/support/relay.rs) can't sit between kiln's nodes without
    a kiln change: per-link cuts need it.
  - Each must show: every acknowledged turn's files served byte for byte (NoLoss), the move completed (session on B,
    its workspace mounted there) or refused by kind with the agent still on A, and every live runner's node serving
    again (the fleet re-formed).

## DONE: A-564 (UI goal 85: a verb forwarded to a fenced owner shard answered NotFound) LANDED on main 2514818a5 (63c83464e..2514818a5), 2026-11-19
- Worktree ~/slates-work/g225-a564, branch agents/g225-a564-fenced-owner. Test b1e7ca76c
  (verbs::tests::a_verb_for_a_volume_whose_owner_is_fenced_is_refused_by_the_fence_never_not_found: a volume on shard 1
  of 2, shard 1 fenced, its Status asked by a client seated on shard 0 and forwarded from PEER via serve_forward on the
  control shard; both must be ConsensusRecoveryUnavailable); fix ac6208269: refusal_of_access (Fenced|Retention ->
  ConsensusRecoveryUnavailable, Absent|Borrowed -> Overloaded), call_owner (try_with_state + within_shard_time; unsent
  or given up -> Overloaded), serve_forward/PromoteRegion/partition lookup/await_deferred_completion/named_if_unshared
  retyped, send_forward's owner borrow by kind. Docs 3f0c4d03a. Merges 643884477, 2514818a5 (log end twice).
- Studio: RED once (same node: NotFound, 0.75 s); GREEN 0.77 s; 17 near tests; fmt two splits applied. Scratch deleted.

## DONE: A-566 (UI goal 85: consensus_on_control answered Overloaded after its operation ran) LANDED on main 87c1be22e (acdbf6974..87c1be22e), 2026-11-19
- Worktree ~/slates-work/g225-a566, branch agents/g225-a566-consensus-answer. Test a58950843
  (verbs::tests::a_recover_answered_after_it_ran_says_so_and_its_resend_takes_effect_once: 3 shards, shard 2 fenced via
  an observation setting consensus_failure, Recover from shard 1 via serve, resent under the same id; asserts
  DurabilityPending, the resend meets it, the root group replaced once); fix e34b9fca6: `published_or_pending` answers
  DurabilityPending { held_ns } when a shard did not apply the publication (recorded; new id re-runs + re-publishes);
  Overloaded only for an operation not run. Docs c5140533b. Merge 87c1be22e (log end).
- Callers read: Bootstrap (bootstrap_authorized -> Acknowledged / AlreadyInitialized), RecoveryPlan (read), Recover
  (receipt by digest first). Left (A-65, pinned by cross_shard_wait): a wedged control shard's queued op may run after
  Overloaded; the resend meets it idempotently. Sibling: a forwarded submit answered Overloaded after it ran (§4.9 11-06).
- Studio: RED once (Overloaded { shard: 2 }, 0.74 s); GREEN 0.75 s; near 5/5; fmt one join applied. Scratch deleted.
- Next: A-564 (queued): a verb forwarded to a fenced owner shard answered NotFound.

## DONE: A-563 (UI goal 85, A-557's sibling: a scatter's Overloaded recorded as its completion) LANDED on main 1b2adf352 (551d6d2cd..1b2adf352), 2026-11-19
- Worktree ~/slates-work/g225-a563, branch agents/g225-a563-unrecorded-refusal. Test bed2d905f
  (verbs::tests::a_resend_of_a_read_scatter_refused_overloaded_runs_it_again: fenced control shard, client seated on
  shard 1, List + Grants served via `serve` as from the ring, answered Overloaded, then resent under the same ids);
  fix c28f9dc89: retry_deferred's `Seen::New if not_started(&reply) => unrecorded_not_started` (the A-533 rule at the
  deferred path); docs f565539af: §4.9 2026-11-17 status, A-563 entry, GAPS sibling closed (+ consensus_on_control's
  Overloaded-after-effects noted), bug record, A-557 record. Merge 1b2adf352 (log end).
- Studio: RED once ("list's resend was answered from a record", 0.74 s); GREEN 0.74 s; near filters 5/5; fmt one split
  applied. Studio scratch + target deleted.
- Next: A-564 (queued for me): a verb forwarded to a fenced owner shard answered NotFound.

## DONE: A-557 (UI goal 85: a read scatter answered whole without a shard's part) LANDED on main fc0d178fe (de1031f1d..fc0d178fe), 2026-11-19
- Test 9892495fd (verbs::tests::a_read_scatter_whose_part_its_shard_refused_is_refused_naming_that_shard: a two-shard
  daemon, control shard fenced by inject_consensus_failure, a client seated on shard 1 over an in-process region pair
  lists/grants/status, reads its ring); fix 619941eaa: part tasks send Option<part>; gather/gather_grants/gather_status
  take the shard and settle None by settle_unserved (= settle_refused, counted scatter.part_unserved) -> Overloaded
  { shard }; docs: Refusal::Overloaded, scatter_refusals. Docs 0e3624138: §4.9 status, A-557 entry, GAPS (2026-11-05
  sibling closed; recorded-Overloaded sibling filed open), new bug record, 2026-11-05 record. Merge fc0d178fe (log end).
- Studio: RED once (listing answered Listed { volumes: [], next: None }, 0.75 s); GREEN 0.73 s; --lib scatter 2/2; fmt
  clean. Merged tree not rerun (merge-now order; main's 32 commits touched none of the files the fix uses).
- Siblings reported (bug-claims.md 02:10Z, unclaimed): a scatter's Overloaded recorded as its completion
  (retry_deferred); a forwarded verb to a fenced owner answered NotFound; settle_refused's orphan mark at a stop.
- Filed in run-requests.md: clippy/xtask, ack_release + the client driver restart test, the Linux/Windows arms of the
  test's region helper. Studio scratch and target deleted; 11 merged clean local worktrees removed (git worktree remove).

## DONE: A-547 (UI goal 85): inventory + the first unowned bug, LANDED on main 3941782cf (ec57251b0..3941782cf), 2026-11-19
- Inventory (reading only): bug-claims.md "2026-11-19 g225 open-bug inventory" -- 24 lines, 34 bugs (11 of them the
  goal-83 B items, whose descriptions are recorded nowhere but g079's list), from GAPS on main 20f541482, November bug
  records' open siblings and main's Windows symlink-landing finding; ordered by severity; not re-verified against code.
- Fix (first unowned, GAPS:1496): an edit after a rename was accepted and lost. Test db9b9f7fa
  (engine::tests::an_edit_after_a_rename_over_a_file_is_merged_with_the_renamed_bytes); fix c8339ba92:
  merge_rename maps the destination's ops through the source's intervening deltas (edits_after_rename; overlap =
  conflict), ContentSource::Copied(from, ops), Applied::Copied { ops } (tag COPIED_SPLICED 17; empty ops keep COPIED),
  effect_of replays them, rebased_from restates them after the Rename; server's parked green_post_state test
  un-ignored. Docs: §4.16 Composition at seal, GAPS fixed (owed: the rebase oracle's generator), bug record, A-547.
  Merges 741f077ca (main's A-542 node_exclusive_tests module beside mine) and 3941782cf (design log end).
- Studio: RED once (old code: "the edit after the rename is lost"); GREEN slates-merge --lib 4/4; merged tree 12/12.
- Filed: slates-merge integration suites, green_post_state (the un-ignored test), clippy/xtask.

## DONE: A-533 (UI goal 85, part 01a1f12cc1557026948d1505dfda4827: A-513's remainder) LANDED on main 20f541482 (26d91cc73..20f541482), 2026-11-19
- Worktree ~/slates-work/g225-a533, branch agents/g225-a533-unbuilt-works. Test 627fa348a; fix a8a0ccc4a:
  `work_home::unbuilt_refusal` (a verb of a work recorded unbuilt answers Unpublished, counted
  merge.work_unbuilt_unpublished); `verbs::not_started` includes Unpublished (never recorded, per the protocol;
  verb.overloaded_unrecorded now counts Overloaded alone); `verbs::ask_owners_to_replay` + `daemon::ask_again_for_unbuilt_works`
  (after recovery every shard asks its unbuilt works' green owners again by run_on_waiting messages; the owner replays
  when it next runs and sends the install back by message; no timer, nothing waits). Docs: §4.16 A-513 status owed
  done, GAPS A-513 remainder closed, A-513's bug record, design A-533 entry. Merges: df3872017 (main's A-528 removed
  daemon::observe_at, restored for the test) + 1feab65f9 (its re-export) + f38733e20, 20f541482 (design log end).
- Studio: RED once (code before: the unbuilt work's read answered NotFound); GREEN 6/6 focused (both new tests, the
  Overloaded resend, A-513's, the unpublished-image retry, work_home::) on the branch and again on the merged tree.
- Filed: clippy/xtask, the whole server lib (not_started's wider rule), the recovery suites.

## DONE: A-512 (UI goal 85, A-488's sibling: the Shards route's take of an offer) LANDED on main 9bd5e26d6 (e3fbf2ee0..9bd5e26d6), 2026-11-19
- Worktree ~/slates-work/g225-a512, branch agents/g225-a512-take-once. Tests cb1ef73ee + e5aa3aeef (the scenario in a
  helper so the test holds only assertions); fix 69bf76ea0: `Offer::claimed_by` (the exchange's daemon nonce); `take`
  claims the offer and answers copies of its ends (another exchange: NoOffer; copies refused by the fd limit: Busy);
  `settle` (Handed retires, NotHanded frees); the exchange (`claimed`) settles after every take it asked;
  `take_from_owner` asks once more after a lost answer. A-440/A-492 tests now read the claim. Docs: module doc, §4.6
  socket-route claim, GAPS siblings line (merged with g230's A-513 closing rebuild_remote_works), A-488's bug record
  section, design A-512 entry. Merge 9bd5e26d6 (design log end + GAPS line).
- Studio: RED once (code before + final test: the stream's end closed with the lost answer, second ask NoOffer);
  GREEN shards_route:: 6/6 (branch, before the merge; main's A-475 changed one test line in the merge, not rerun per
  the merge-now order). Nothing else compiled. Filed: clippy/xtask, the merged module's tests, the client's route suites.
- All three A-488 siblings now closed (A-505 direct map, A-512 Shards take, A-513 rebuild_remote_works by g230).

## DONE: A-505 (UI goal 85, A-488's sibling: direct_map's send_to_client) LANDED on main ad0ca0f0f (cd8e7bcfe..ad0ca0f0f), 2026-11-19
- Commit c05f722d1 (worktree ~/slates-work/g225-a505, branch agents/g225-a505-send-once): `ClientSlot::maps_sent`
  (state.rs, daemon.rs constructor); direct_map.rs: `Delivered` lifted out of the Linux code with `send_once` (a map
  recorded as sent is answered Sent, never sent again), `kept_as_handed` (an answered send, or no answer at all, keeps
  the copy as handed: Mapped; the client gives back a copy whose descriptor never came, A-174), `forget_replied` +
  `replied` (the record ends as deliver_to delivers the Mapped reply on the client's shard, after every ask);
  `send_to_client` sends through the record; Linux `hand_off` asks once more after a lost answer; `handed` follows the
  verdict, counting dax.local_map_handed_unanswered. Docs: §4.6 hand-off sentence, GAPS class A siblings line, A-488's
  bug record extended with an A-505 section, design A-505 entry. Merge ad0ca0f0f (design log end).
- Studio run: RED once (record check removed: the second ask sent the copy again, sends 3 vs 2); GREEN direct_map:: 4/4;
  fmt clean. Clippy (run before main's no-lint order): my test direct_map.rs:1791 cognitive complexity 12/10 -> filed
  for main (split the assertions into helpers). xtask: 6 literals, none mine (anchor handles.rs, base s3 list.rs).
- Filed in run-requests.md: the clippy fix, and the Linux build/lint/tests of the Linux glue + client direct-map suites.
- Left (main starts each): the Shards route's take (merge on g232's A-492 rework), then rebuild_remote_works (judge the
  "unbuilt" acceptance under §4.16 first).

## DONE: A-488 (UI goal 85 17ad60b6, bug 3) LANDED on main 7267db674 (5fef5b1ef..7267db674), 2026-11-19
- Fix 67055138c: `VolumeSlot::adopted_seal` (state.rs); `verbs::seal_adopted_head` answers the recorded seal and records
  the one it takes (a missing slot refused NotFound); moves.rs docs; GAPS class A owed line closed; design A-435 owed
  done + A-488 entry; bug record docs/bugs/2026-11-18-an-adopted-moves-seal-was-taken-again-after-its-answer-was-lost.md.
  Test 9b43a1ed0: `an_adopted_moves_seal_asked_again_after_its_answer_was_lost_is_taken_once` (moves.rs). Evidence
  6155df2b7. Merge 7267db674 (design log end; main's A-485 moves.rs change merged cleanly).
- Studio kiln r13: RED once (pre-fix product): second ask Sealed(4294967296, 3) after Sealed(0, 2); held [0, seal2].
  GREEN moves:: 4/4; fmt clean; clippy -p slates-server --all-targets -D warnings clean; xtask check: 45 literal
  findings, all other changes' files (land 39, base s3 5, anchor 1). Merged tree 7267db674: moves:: 4/4, fmt clean.
- Siblings found by reading (in the bug record, not fixed): shards_route take of an offer; daemon rebuild_remote_works
  (std::mem::take of work_home.unbuilt); direct_map send_to_client.
- Run requests filed (this date): A-488 fleet move flows; A-406 suites (re-filed); goal 225 diag loop (re-filed).
- Amendment registry: A-488 landed. Studio target dir deleted; diag-loop dirs kept for main's run.

## Goal 225 (a forming fleet reaches every candidate) — where it stands
Landed on main: A-331 ddcf00768 (zero-contact stall), A-337 bb4a42d75 (a pair's lower anchor keeps a voter that dialed
it), A-351 3bf968a0b (a retired owner heard again is readmitted), departure budget 5050a6f62, A-380 a873f4a04 (council
voters versioned in the regional configuration), A-406 341f6474f (code bb5321e20: root voters versioned in the root
configuration; unit tests on the merged tree: db 4/4, cluster 36/36, server lib 6/6, root-learner fleet test 1/1; red
once). A-406's suite run was filed in run-requests.md at 19:53Z (lost in the wipe; re-file if main has not run it).

Left (item 4, "initial membership did not commit", plus the 409 s learner stall):
- (a) stuck-dial shape: the bootstrapper's record dial to one peer sat in its first handshake call 10.8-15.8 s; mechanism
  unknown; the instrumented loop (branch agents/g225-diag-formation-4, 53bbd1d5b; script ~/.cache/g225-scripts/
  diag-loop4.sh on the Studio, if it survived) was filed for main at 19:20Z (lost in the wipe).
- (b) joint-entry shape (diag-green-44269355181 arm64 run 7): council leader L 778.. term 1 last_log 6 commit 6; follower
  6098.. last_log 7 holding a joint entry of term 1. Only L writes term-1 entries, so L wrote index 7 and lost it. By
  reading: raft.rs shortens a log only by a follower's conflict truncation (place_entry), a follower's snapshot install,
  and a whole-state restore (startup, an uninitialized node's join, operator reform); bootstrap refuses a second run.
  No path found. RISK if real: a different index-7 entry in term 1 would be kept by the follower's place_entry as
  matching (same index and term): Log Matching broken. Needs the diag build's DIAG lines from a failure.
- (c) Windows two-region root shape (run 44298844583): root bootstrapper X committed C_new = [470.., 667..] and stepped
  down; both new voters hold C_new uncommitted and never elected each other; nothing read in the root drive, timer,
  pre-vote or vote blocks it. Lead: 470.. counted consensus.root_join.from_another_peer 1 and
  consensus.join.from_another_peer 1 — a fetch reply credited to a peer other than the one whose join state it carried.
- (d) 409 s learner stall: confirm A-380/A-406 cover it from main's one-CPU learner runs.
