# g267-slates-server-federates — progress (rewritten 2026-11-18 22:15Z after the ~/.cache wipe of 14:04 PST; K5 added ~23:50Z)

Goal: kiln UI goal 89, kiln goal 95101ef32ffb8b099942fe076619c310: the slates server federates and replicates changes
across kiln runners (runners form a slates fleet: membership, certificates and discovery through kiln; a volume's
changes replicate to copies on other runners; an agent moved between runners carries on with its volume; proven by a
multi-runner test with injected faults).
Role: Claude subagent of the slates supervisor (main), implementer. THE RULE (final): one piece, `--lib` unit tests +
fmt, merge, END; heavier runs filed in run-requests.md; main starts the next piece.

## Items (tick as each lands on main)
- [x] slates: planned move A-291 (place, switch, adopt, release) + handoff fix; capture_base; movable captured clones
      (slates main 113d0a1ee, earlier owners)
- [x] slates: the fleet's network needs written down (docs/deploy.md, GAPS)
- [x] slates: a captured clone lands against its capture, stage 1 (A-424; slates main ef0c40e29 = 1a7f166f0 + f8a5696a6)
- [x] slates: A-544, a taken-over captured clone lands against its capture (slates main 54067060a)
- [x] slates: A-565, a live-shipped captured clone's move gives its new owner the mark (slates main 767a07360)
- [ ] slates: stage 1's advance (a later landing judged against what the last one landed; GAPS g267 owed (1))
- [x] slates: stage 2, a moved captured clone lands against its capture (A-503: slates main e2d1bc42b; kiln drops its
      refusal, re-vendored at e2d1bc42b, contract 41: kiln main 30dcde58d)
- [x] kiln K1: each runner's fleet node key + kiln's CA signs its CSR (kiln main a04e7d6d4 = 931d9ad52)
- [x] kiln K2: discovery + the fleet manifest (kiln main baf06fa72 = b21b2e80c)
- [x] kiln K3: `[slates_fleet]` opt-in (f, nodes machine->address), a chosen runner's Slates started with StartSpec.fleet,
      the manifest read first by `slates_api::role::check_fleet` (kiln main a09d3aa05; slates main 3e6a8ff6a, contract 39)
- [x] kiln K4: an agent moves with its workspace volume by Slates' own move (kiln main aba723dc5; slates main
      b2909fcf5 = A-450 docs)
- [x] kiln K5 (A-463): a fleet runner's workspace is a clone of its folder's captured base, so K4's moves take it
      (kiln main 1ec78034d; slates main dbb7a6d4b = A-463 docs)
- [ ] the multi-runner test with injected faults (Mac runners; implement + file its run)
- [ ] slates: the Windows manifest reader's /etc/resolv.conf dependency (queued by main as a separate piece)

## Clones (all source only; nothing uncommitted)
- slates: ~/slates-work/g267-slates-server-federates-capture (branch agents/g267-slates-server-federates-capture, at
  slates main dbb7a6d4b; pushed branch agents/g267-a463 is the same work)
- kiln: ~/slates-work/g267-kiln-fleet (branch agents/g267-kiln-fleet, at kiln main 1ec78034d; pushed branch
  agents/g267-k5 is the same work)
- ~/slates-work/g267-slates-server-federates-shallow: an old rsync mirror for Studio runs, uncommitted mirror edits of
  pieces 3/4 (all on main since); left untouched.
- Studio (d1323898): my clones and targets removed after each landing; ~/.cache/g267-logs kept there.

## Landed today by this owner (sixth), in order (the commit ids are the record; earlier notes' clock times were guesses)
- A-424 (slates main ef0c40e29): `slates_vfs::captured` (FrozenTree, ChangedPaths over trie::changed + directory
  diffs + subtree walks + homes, held_at, read_file), land `Judged::Content` (content manifest VERSION 2, witness tag 2),
  `PlanWalk::against_capture`, `content_verdict`, engine observed fingerprints, content Rmdir only once emptied, no
  advance for content manifests, sync phase skips directories an Rmdir removed (f8a5696a6, red first in r135); server
  `capture_of`. Lib tests: vfs captured 3/3, land lib 11/11, server landing:: 11/11.
- K1 (kiln a04e7d6d4): `Pki::sign_slates_node`, `workspace::slates_fleet` (FLEET "slates.kiln", domain_of,
  issue/prepared/finished/identity), POST /api/agents/slates-fleet. Lib 3/3; red-check 3/3 failed.
- K2 (kiln baf06fa72): check_address, admit, Directory (hub field), announcement/discover, write_manifest,
  POST /api/agents/slates-fleet/members. Lib 6/6; red-check: the refusal test failed.
- K3 (kiln a09d3aa05, slates 3e6a8ff6a): slates-api `role::check_fleet`/`FleetCheck` (plan_from split, rcgen
  dev-dep, contract 39), vendored into kiln as recorded change 0002 (vendor.py check: 0 differences); kiln config
  `[slates_fleet]`, place_of, admit_chosen, Source/set_source, ask_place (POST .../place), join, checked; ensure_up joins
  before start_anchor. slates lib 11/11, kiln lib 19/19; red-check (listed-address check off) failed.
- K4, landed ~22:15Z (kiln aba723dc5 = 7f1eba30b + 2b7fc312f + ec0dba00b + merge 5866cdca7 +
  aba723dc5; slates b2909fcf5 = A-450): `workspace::slates_move` (VolumeMove, MoveRefused by kind with `kind()` and
  `reported()` "[kind: x]", `plan`, `step`, `move_out`), slates.rs (`placed_here`/`placed_under` incl. main's
  place::real, `mounted_patiently`, `unmount_for_move`, `ask_move`, `pause_for_move`, `start_if_chosen`; a fleet node no
  longer calls ensure_group and reports its member id), slates_fleet (NodeAnnouncement/Member `member`, `Joined`,
  `report_member`), mod.rs (`arrived_here`, `mounted_for`), MoveTo `slates` (ts skip), session hand_over moves the volume
  before handing over (refusal remounts and leaves the agent), launch blocked until the workspace mounts, host
  take_over_now re-places the mount, relocate plans it (feature `workspace.slates.move`, docs/wire.md), agent.rs starts a
  chosen runner's Slates eagerly. Lib (Studio, merged head aba723dc5): 96/96 over workspace::slates_move,
  workspace::slates_fleet, workspace::slates::tests, harness::relocate, wire::ts, proto::; red-check (five mutations
  at once) -> all six K4 tests failed; restored.

- K5 / A-463, landed ~23:45Z (kiln main 1ec78034d = 4398c79cc + 229e0aac8 + 446367332 + merges 1824a566c, 1ec78034d;
  slates main dbb7a6d4b = d2d8af180 + merge): kiln re-vendors Slates at b2909fcf5 (contract 40; change 0002 dropped as
  upstream carries it; `vendor.py check slates`: 1091 files, 1 recorded change, 0 differences); `workspace::slates`
  `provision(name, over, captured)`, `in_fleet`, `kept`/`forget`/`bases_held`, `base_for` (one capture per folder
  while its drift is empty), `capture` (overlay + capture_base, Retry asked again <= CAPTURE_TRIES 3), `release_base` +
  `destroyed` + `let_go_of_base` (a base goes with its last clone), `drift_volume`, `landing` refuses `Made.moved`,
  `arrived`/`arrived_under`; `Made.base`/`Made.moved` (serde-optional, ts skip); `arrived_here(spec, !pkg.handover)`;
  docs/agentic.md row W, "W3 on a runner in kiln's Slates fleet", W7. Tests on the Studio: r407 (load 9.5-16.8) 19/19
  over `k5_ k4_ k3_ w1_kiln_speaks bindings_are_current w3_an_archived` with KILN_TEST_KILN (the real-instance
  `captured_tests.rs` among them); red-check, six mutations at once: 6/6 K5 tests failed; r443 6/6 again (instance
  stopped 204 ms after SIGTERM); merged tree r457 19/19 (stopped 176 ms). Slates: docs only after the merge (main's lint
  pass had already reworked my A-424 test file, so my two land commits fold away); `cargo xtask check` on the merged
  tree red on 6 literals, none mine (below).

- A-503 (stage 2), ~00:45Z 2026-11-19: slates main e2d1bc42b (396bc1de2 + 2b417930f + 8aba4a047 + merges 96ffa46a7,
  e2d1bc42b): vfs `captured::{CapturedState, CarriedCapture (parse: hostile input refused), CaptureBase::{Tree,
  Carried}, CaptureSearch, CarryWalk, state_in, file_identity}`; land plans against `CaptureBase` (PlanWalk and
  Beginning take `impl Into<CaptureBase>`); db `CarriedRecord`, `CarriedCaptureRecord`, `Op::CaptureCarried` (raises
  the catalog), `Op::CaptureAdopted`, partition map + snapshot section, mark cleared with its snapshot, entry gone with
  the volume (wire_order.txt published by hand: Op 81, 82); server `CatalogValue.carried`, `landing::capture_of` ->
  CaptureBase (stage 1, else the carried record and its mark, else refused typed), moves: `begin_carry` +
  `carry_slices` task + `capture_carried` gate in `ready_to_hand_over`, mark exempt from Snapshots, `seal_adopted`
  keeps the mark, `finish_making` records `CaptureAdopted` (mark only when the head identity matches and not from a
  live log). Lib tests on the Studio: vfs captured:: 6/6, land captured_tests:: 9/9 (10/10 merged), db 1/1, server
  catalog+moves 5/5 (6/6 merged); red-check (carried state ignored, adopted record dropped): vfs 1, land 2, db 1 failed.
  kiln: Made.moved and the moved-landing refusal gone, arrived_here(spec) back to placed_here; re-vendor to e2d1bc42b
  (vendor.py check: 1107 files, 0 differences after the merge with g088v's 1f332da55 re-vendor); CONTRACT doc keeps
  g088v's 41 paragraph plus the e2d1bc42b line; review.rs is main's. Kiln lib 17/17 before the merge, 34/34 on the
  merged tree (f8a868974); landed as kiln main 30dcde58d after one more clean merge of main (g264's excludes fix).

- K2 test clippy fix (UI goal 89 part 01a1f1396c81759da917dab3a2371047), kiln main 3b0f9ae91: the K2 manifest test
  reads the manifest with `get` and typed checks (slates_fleet.rs 761-771), a missing key failing with what the
  manifest held; kiln lib workspace::slates_fleet 10/10 on the Studio. Clippy itself is main's (filed nowhere new: the
  kiln gate request stands).

## Findings for main
- K3 bug fixed in K4: kiln called `ensure_group` on every instance it started; Slates says never on a fleet node.
- K5 makes a fleet runner's workspaces captured clones, so a live K4 move should now pass (filed). Elsewhere workspaces
  stay overlays: a capture reads the whole folder, ignored build output included, into memory.
- slates `cargo xtask check` on main (dbb7a6d4b) is red on 6 literals, none from this piece: anchor/src/handles.rs:69
  (WHAT_SHARDS_DAX) and base/src/s3/list.rs:35/388/478/483/484 (g268's S3 work). Before main's lint pass, clippy of
  slates-land with -D warnings also failed in g085's files (chmod_tests.rs:130 type complexity; windows_bits.rs:233
  cognitive complexity 11/10). Not rechecked after the pass.
- kiln `slates::excludes(m)` for an unmounted (isolated) workspace runs `git -C ""` (place::top("")), which git takes
  as the process's cwd: its landing excludes are whatever the runner's cwd ignores (g264 told).
- Studio r407: the real-instance test's anchor and daemon were still serving ~2 min after the test sent SIGTERM and
  exited; once the test waits for the instance to stop (r443/r457) it stops in ~0.2 s. Cause not found (no log kept).
  w2's `stop(pid)` has the same fire-and-exit shape.
- Process slip: during the slates merge I used `git checkout --theirs` on the one conflicted test file to take main's
  version (the intended resolution, no work lost); CLAUDE.md forbids git checkout.
- Slates' manifest reader reads /etc/resolv.conf whenever roots are trusted (kiln always gives its CA): Windows runners
  can't join (queued by main as its own slates piece).

- A captured clone taken over after its owner's loss (no move carried its capture) still lands as a scratch volume
  on its successor (A-503 owed); a live-shipped clone's move carries a record its new owner holds no mark for.

## Next (main assigns, queued on goal 89): a captured clone taken over after its runner dies lands against its capture;
a live-shipped clone's move gives its new runner the mark; then the stage-1 advance and the multi-runner fault test.

## A-544 (UI goal 89 part 01a1f1396c81759dac2cbb33080629c9), restarted owner (seventh), 2026-11-18
- Predecessor wrote A-544 (05fc5b9fe + bed3cabce, pushed agents/g267-a544) but never ran its tests and never landed.
- This owner: merged origin/main (54ccdc6f1; the design log's end the only conflict, A-544 last), reviewed, fixed three
  faults in bfb5eff72: carry only for a clone's own head (a fresh clone's head is its origin's: a refused carry was
  counted for every captured clone), a move waits for its head's record (carry::carries_its_head first in
  moves::ready_to_hand_over; the removed move-time walk had let a refused carry move), carries of let-go volumes
  forgotten at the home walk's end (carry::forget_gone). Design A-544 entry rewritten: what is lost when the capture
  never reached the fleet (sealed without its record: the landing only; never sealed: the whole tree, served empty).
- Clone: ~/slates-work/g267-slates-server-federates-capture (branch agents/g267-slates-server-federates-capture; pushed
  as agents/g267-a544 at bfb5eff72). Red patch (five mutations): /tmp/g267-red.patch on this MacBook.
- Next: Studio run, red first (all five a544 tests fail), then green; cargo fmt there; evidence commit; merge; push main.
- Sibling seen, not changed: a fresh clone's inherited head makes fleet::start_seal count `fleet.seal` every period
  until its first snapshot (every clone, captured or not).
- LANDED (2026-11-18 ~01:50Z 11-19): slates main 54067060a = 05fc5b9fe + bed3cabce (predecessor) + merge 54ccdc6f1 +
  bfb5eff72 (three fixes) + e44d109e4 (fmt, evidence) + merge 54067060a. Amendments row A-544 marked landed.
  Lib tests, Mac Studio d1323898: r55 red first (five rules taken out, calls kept) 0/5 a544, each at its own assertion;
  green 5/5 a544 and 50/50 over a544 carry:: moves:: catalog:: landing:: fleet::; db carried 3/3; cargo fmt applied.
  r70 merged tree: server 50/50, db carried 4/4, fmt --check clean. Studio clone and target removed.
- Left (filed in run-requests.md for main): clippy/xtask/doc over A-544's files; server fleet suite, db model, api
  captured-base test; the fleet's own three-daemon takeover test (to write); A-503's request item 3 changes under A-544
  (counter fleet.carry.recorded; the new owner keeps 1 snapshot after its seal, not 2).
- Owed in design (not this piece): a captured clone whose owner dies before its first snapshot is placed is served
  empty on its successor (seal it at its making, or name its capture in its catalog); a live-shipped clone's mark
  (queued next for g267 by main); the catalog-before-head window (a takeover in that round refuses the landing).

## A-565 (UI goal 89 part 01a1f1396c81759db2b0b4eef326e523), 2026-11-19
- LANDED slates main 767a07360 = b61b968eb (code, bug record, parked test) + afe847f25 (seal test unplaced at f>0, fmt,
  design, GAPS) + 0e37ab9da (evidence) + merges d79af8cad, 767a07360. Amendments row A-565 marked landed.
- What: a move's new owner marks a live-shipped captured clone it rebuilt from the log (verbs::TakenOver::moved,
  fleet::MakeTaken::moved from the control shard's move_adoptions); a takeover after a loss still marks nothing rebuilt
  from a log (identities cannot decide: the rebuild's archive is laid out by extents, the seal's by its chunker).
  fleet::sealable_head: a head that is none of the volume's own snapshots has nothing to seal (the fleet.seal sibling).
- Main's question 1 settled in A-565's entry: not-live clones lose only what their reported loss window covers (kiln
  workspaces are never live-shipped); live clones' writes are never lost, but the Clone verb is answered before any of
  the clone is durable (replicated_waits waits on the origin) -> FILED: docs/bugs/2026-11-19-a-live-clone-is-answered-
  before-any-of-it-is-durable.md, GAPS open entry, bug-claims.md (unclaimed), parked failing test
  a_live_clone_is_answered_only_once_its_first_whole_entry_is_durable (verbs.rs, #[ignore], red as filed).
- Lib tests, Mac Studio: r125 red first (move flag ignored + old sealable_head: both tests failed); r145 green 72/72;
  r264 merged tree 74/74, parked bug test red, fmt --check clean; Studio clone and target removed.
- Left: run request 2026-11-19T02:40Z (lints, fleet suite, the moved-live-clone fleet test to write, the bug's window
  test with its fix); owed in design: a takeover's rebuild of a live captured clone marks nothing (needs the log
  position its head was sealed at); the bug's fix (unclaimed).

## A-577 (UI goal 84; a live clone is answered only once its first whole entry is durable on f+1), started 2026-11-19
- Reading done before the usage-limit stop (findings): (1) the Clone reply waits on the origin's log
  (verbs::replicated_waits takes body.volume()); (2) a clone's record is a copy of its origin's (publish_clone_as):
  head = the origin's snapshot id, epoch = the origin's, taken_over and policy.live_holders copied, so a fresh clone's
  head register never ships until its own first seal: at f > 0 live_ship::head_placed stays false and its log can
  never answer (writes wait until the SLO judge refuses them); (3) a fresh clone is not clean (never published), so the
  Clone's own publication captures it and live_ship::published ships its whole image at position 0; (4) answerable()
  gates: head_placed, active_set_recorded (catalog with holders placed), covers_candidates, durable >= whole entry;
  holders_placed: a new shipper records its holders (LiveHoldersCommitted) and ships only once that catalog is placed.
- Plan: clone's registers start at the creation head (head default, epoch 0; taken_over None; live_holders None);
  replicated_waits waits on the clone for a Cloned reply; at_risk_ns zero only once a new log answers at its whole
  entry; un-ignore the parked test; measure rounds (record round then entry round) and the image bytes in a test.
- LANDED (2026-11-19 ~03:15Z): slates main 1fa500184 (A-577; merge 8734ca306 after main's A-578 live counter). Lib tests,
  Mac Studio: r354/r396 red first (all A-577 tests failed with the rules out), r396 green 83/83 server + 16/16 vfs,
  r414 merged tree 84/84 + 17/17, fmt clean; Studio clone and target removed. Measured: a live clone's reply waits for
  one entry of 4,757 bytes (a 4,096-byte incompressible file and its tree), answered at one holder's acknowledgement.
- Found and fixed beyond the brief: a live log's whole records were a clone's recovery image (inherited tree by number
  only), so no successor could rebuild a live clone from its log; Volume::to_standalone_image + live_ship::keep_shared.
  My A-565 settlement ("a live clone's first write ships its inherited tree") was wrong; corrected in the bug record.
- Owed (filed): record plane shipping a new live volume's catalog and creation head on demand; image::run_merge's live
  clone answers early; the fleet's own window test.
- Then main moved me to goal 92: A-596 (§4.17.1 shared piece 6, review records).

## A-596 (UI goal 92, 51cef9f7; §4.17.1 shared piece 6, review records), relaunched owner, 2026-11-18 (PST)
- Brief said A-577 is parked: it is not. A-577 LANDED as slates main 1fa500184 (an ancestor of origin/main, checked
  with merge-base --is-ancestor after a fetch); amendments row A-577 says landed. Nothing of A-577 was uncommitted.
- The -capture clone held my earlier session's uncommitted A-596 draft (merge/src/review.rs 436 lines, db/src/review.rs
  406 lines, six review ops, two catalog records, DbError::ReviewOutOfOrder; unwired, never compiled). Committed as
  5c8934a95 on that clone's branch and pushed alone as agents/g267-a596-wip.
- Worktree ~/slates-work/g267-a596 (branch agents/g267-a596, from 5c8934a95; `git worktree add -b` off the -capture
  clone); merged origin/main e85c17f9f as 833e7f6a8 (four enum-end conflicts; review ops renumbered 90-95 after main's
  RepositoryGreenJoined 89).
- Next: wire both modules, partition guard/apply/snapshot, db unit tests, failing tests first, Studio run.
- LANDED (2026-11-19 ~04:30Z): slates main 82c3e2be3 (6e5fc9819..82c3e2be3) = 5c8934a95 (the found draft) + merge
  833e7f6a8 + d0b1a10f6 (code, tests) + 2bf04a1dd (oracle guard, parked second finding, rustfmt) + 0b0aa6193 (design
  status, piece 6, amendment A-596, GAPS, two bug records) + merge 82c3e2be3 (docs-only conflicts; main had not touched
  db/merge/server since r529's tree). Amendments row A-596 marked landed.
- What: slates_merge::review (line_range, lines_of, covers_whole_lines, move_through -> FileReplaced / LinesChanged /
  LinesJoined), Green::reanchor (engine/reanchor.rs: walk of the path's content history, two-byte whole-lines judgement
  from the held value, refusals PastHead/Backward/Folded/NotLines); slates_db::review (Reviews: 6 ops Op 90-95, typed
  ReviewRefusal, merge-budget charge, snapshot fields review_comments/review_approvals, staled_by predicate, standing,
  next_comment); server maps DbError::Review to VolumeRefusal::Invalid (no verb emits it yet).
- Tests, Mac Studio d1323898: r504 red (8 rules out) 7/7 merge + 4/4 db failed; r529 green merge 7/7 (+2 parked
  ignored), db 11/11, server error:: 3/3, fmt --check clean. Studio clone + target removed (r546); logs kept in
  ~/.cache/g267-logs/a596 there.
- Findings (oracle's, unclaimed, filed: GAPS before A-547, docs/bugs x2, bug-claims, parked tests red on r529):
  (1) a work based before its file was replaced is spliced into the replacement; (2) a truncate based before an append
  drops the appended bytes (lost update) and its delta understates what left. The oracle avoids both until fixed.
- Owed (not this piece): server review verbs + typed wire refusals (contract bump), comment version as a live reader of
  the green's fold floor, following renames with path remapping, the two verdict fixes. Run requests filed 04:30Z.

## A-612 (UI goal 93; the two verdict defects A-596's oracle found), 2026-11-19
- Registered A-612 (next free -> A-613). Worktree ~/slates-work/g267-a612 (branch agents/g267-a612 off origin/main).
- LANDED slates main 0d21b12f6 (fa3605e50..0d21b12f6) = 7c3d9bdfc (tests first: unseen_bytes_tests.rs, oracle guards
  off, parked tests moved) + f98f535de (fix) + docs commit + merge 0d21b12f6 (amendment-log conflict only; main's 51
  commits touched nothing in slates-merge). Amendments row A-612 landed; bug-claims lines marked [LANDED A-612].
- Fix: Green::file_removed (last version a path's file was removed: unlink, rename away, rename onto it; set in commit,
  rebuilt by replay, never folded) -> merge_content DeleteModify, merge_rename RenameRename for a work based before it;
  at_head(): a mapped Truncate whose span no longer reaches the head's end becomes Delete of that span (merge_content,
  edits_after_rename), so appends survive and deltas are exact (the algebra's hunk over base bytes; edge rule).
- Tests, Mac Studio r613: red (fix reversed) 7 failed incl. A-596 oracle; green slates-merge --lib 43/43; fmt clean.
  Studio clone + target removed (r631); logs in ~/.cache/g267-logs/a612 there.
- Sweep reported (design amendment A-612, GAPS): directory-rename replacement covered (test); truncate vs overwrite /
  insert / truncate at the cut, both directions, already correct (sweep test); rebase and A-547 rename-edit truncates
  fixed. Seen by reading, NOT changed: mode/xattr change on a replaced file judged by its own dimension; rename whose
  source is a symlink/hard link replaced after base moves the new one; intervening's per-entry double read (now
  unreachable); an append at the old end of a whole-file rewrite lands at the new end (edge rule; design question).
- Run requests filed (clippy/xtask/doc; merge integration + server green suites; mixed-build holders note).

## A-621 (UI goal 93; ops document path indices; found by g225 on the push path, A-618), 2026-11-19
- LANDED slates main ed604ed99 (81e812ac7..ed604ed99) = e8169efc4 (code + tests) + 1b266a036 (docs) + merges
  fd8fd05f3 (A-586 Copy landed meanwhile: chain_entry.rs end kept both; declared_copy and copy_records_reservation
  widened; copy_tests conversion), 18c57392e, ed604ed99 (doc conflicts only). Amendments row A-621 landed.
- What: Op.path u64; ops document v2 (u64 path/op counts, 34-byte op record), chain entry v5 (OP_BYTES shared);
  PathTable interns through BTreeMap<Name, u64> (Name's Ord counts comparisons in test builds); decode refuses
  DuplicatePath; deriver index_of, engine resolve/node_exclusive_paths/reservations, server verbs xattr name widened;
  tests/ops_doc.rs moved into src/ops_doc/tests.rs; golden vectors v2 derived by an independent Python encoder +
  BLAKE3 (verified on v1's vector 67613f8e... and the empty-input vector): 152 bytes, identity 11ede55e...d370763b.
- Tests, Mac Studio: red r705 (4/4 failed); green r692 60/60, merged r721 + r730: merge --lib 75/75, server 31/31,
  fmt clean. Studio clone/target removed (r741); logs in ~/.cache/g267-logs/a621 there.
- Filed (GAPS + bug-claims, unclaimed): A-612's reading-only siblings (metadata on a replaced file; replaced
  symlink/hard-link rename source); chain_entry::declared_ops per-splice scan (O(splices x ops)).
- Run requests filed (clippy, merge integration tests updated by reading, xtask/doc, server suites; g225 can drop
  TooManyNames).

## A-631 + A-632 (UI goal 93; A-612's verdict siblings; chain entry one-pass index), 2026-11-19
- LANDED slates main 223343542 (0392c81a6..223343542, fast-forward) = aa9de3fb4 (failing-first tests) + 1b3dff718
  (A-631 fix) + 06dfe301b (A-632) + c11aff89c (docs) + merge 7d04710ea (main's A-618 `emptied` lines kept beside the
  A-631 removal records in three arms; amendment log main-first) + 223343542 (docs: siblings four, A-618 closed the
  fifth). Amendments rows A-631/A-632 landed; bug-claims 05:15Z entries marked LANDED.
- A-631: `Green::file_removed` recorded for every type's removal (RemoveDir/RemoveSymlink/RemoveHardlink arms,
  remove_special); merge_setmode/merge_set_xattr/merge_remove_xattr (new `established` param) refuse DeleteModify when
  the node was removed after the base unless the work makes the path; merge_rename's A-612 check now covers links
  (RenameRename). file_removed_since's doc put back (A-612 had displaced intervening's doc). Tests:
  engine/replaced_node_tests.rs (metadata x3, link renames x2, generated oracle + census).
- A-632: chain_entry `Declared` index built once per encode/decode (content ops by path; copy versions by dst then
  src) replaces declared_ops/declared_copy; counter test 1,000 splices -> 1,000 visits each way.
- Tests, Mac Studio: red r786 4/4 failed (mode change Accepted v3; link rename moved the new link; oracle shrank to a
  symlink remade + RemoveXattr; 1,000,000 visits each way); green r786 79/79 + fmt; merged r810 92/92 + fmt.
- Probed (scratch test, never committed; r810 on the merged tree): four siblings open, filed in GAPS + bug-claims
  05:35Z: link renamed over a regular file leaves BOTH at one path (even at the head); rmdir has no base; unlink/rmdir
  ignore a metadata change after their base; hard link to a remade target names the new file. A fifth (metadata
  outliving a removal) was closed by A-618 meanwhile.
- Run requests filed (clippy, xtask literals, merge integration tests, server suites).
- Studio clone/target/logs for a631 removed at END; ~/.cache/g267-scripts (old remote-gate.sh) removed too.

## A-643 (UI goal 93; A-631's four verdict siblings; the green's state whole), 2026-11-19
- LANDED slates main 88a4235af (7d861fc7e..88a4235af, fast-forward) = 90c89ce6a (failing-first tests) + 91d6a4010
  (fix) + b9f51abf7 (same-file no-op, found while fixing) + 7de986592 (docs) + merge 1956fcf5a (amendment log
  main-first; main's crates/merge change since base: push.rs +10) + 88a4235af (docs: merged run). Amendments row
  A-643 landed; bug-claims 05:35Z entry LANDED.
- Fix (engine.rs): merge_rename emits RemoveContent(dst) for a link renamed over a file unless the increment clears
  dst itself (Resolved::cleared gathered once in decide); a hard link renamed over the file it names returns Ok with
  no effect (POSIX no-op); merge_rmdir takes base, DeleteModify on changed_after; merge_unlink's file branch uses
  changed_after (any dimension); merge_hardlink DeleteModify on the link when file_removed_since(target) unless the work
  establishes the target. Metadata stays path-keyed (A-618).
- Tests (replaced_node_tests.rs): 6 targeted (link over file; rotation; hard link over its own file; rmdir after
  remake; removal after mode/xattr; link to removed/remade target + own target accepted) + oracle `Whole` (kinds per
  path per version, compared after every event; census of nine answers).
- Studio: red r874 5/6, r880 6/7 (all reversed) + same-file-only reversed 2/2; green r880 99/99 + fmt; merged r899
  99/99 + fmt.
- Siblings filed open (GAPS + bug-claims 06:02Z): renames over link/IPC node refused TypeChanged without concurrency
  (false conflicts); hard links name targets by path (model question); IPC unlink's O(changes) scans.
- Run requests filed (clippy, xtask literals, merge integration tests, server suites).
- Studio clone/target/logs for a643 removed at END; local ~/.cache/g267-scratch removed.

## A-646 (UI goal 93; A-643's three siblings), 2026-11-19
- Part 1 LANDED slates main 15fc0bf7f (e931886a5..15fc0bf7f): items 1 and 3. f5dac1f73 tests, 290747dac fix,
  bf662f140 test fix (a shadowed `green` binding; r948 did not compile), docs + merges (A-626 copies and A-637 gitlinks
  merged in without conflict in code).
  - Item 1: merge_rename's destination via replaced_by_rename (POSIX rename: replace any non-directory the base held
    unchanged; dir / changed link or IPC node -> TypeChanged; changed file -> RenameRename; IPC node named by links ->
    DeleteModify); same_file no-op for two names of one file.
  - Item 3: the IPC unlink reads changed_after + named_by_links (links_to): 2 lookups; counter test 2 vs 2,000 (red).
  - Studio: red r960 (4 of 7 selected failed, guards pass either way; 2,000 entries examined); green r960 104/104;
    merged r972 116/116 + fmt.
- Part 2 (item 2: hard links name nodes; ops document version change): in progress.
- Part 2a LANDED slates main dc46c9f05 (9c7c05e29..dc46c9f05): the engine's node model. b58313c30 tests (node_tests.rs;
  the whole-state oracle follows nodes; scan counter 1), eadd8e96a fix (commit derives node_moves: hand-over to the
  least surviving link with content/IPC node, mode, attributes; renamed home re-points links; merge_hardlink via
  node_home + Resolved::link_node; IPC removal no longer refused for links). Red r1019 9 failed; green r1019 124/124;
  merged r1032 126/126 + fmt. A-646's decision recorded in §4.16 (home as the node's identity; minted inode rejected).
- Part 2b (ops document v3 node table, deriver, golden vectors, determinism gate): in progress.
- Part 2b LANDED slates main 3f4850d6c (df50dc58c..3f4850d6c): ops document version 3 (a node table after the paths:
  origin byte Base/Made + path index, 9 bytes, sorted by (path, origin), unique; Link src = node index; BadNode,
  DuplicateNode; version 2 refused), golden vectors re-derived by an independent Python encoder + BLAKE3 (reproducing
  v2's 11ede55e... and the empty input's): 218 bytes, 553a422f323766ffb303343fb617e8651c1c08ee1802e34b1258d1b11c6c0d92;
  determinism gate with nodes; engine reads the origin (made via renames -> the source's base node, carried by node
  moves; via own links; created/mknod); deriver names Base unless made earlier in the journal, refuses a made target
  moved after its link (Unsupported). tests/common/mod.rs idx u16 -> u64 (A-621 regression: 10 integration suites did
  not compile). Red r1104 12 failed; green r1104 134/134; merged r1111 141/141 + fmt.
- GAPS entry a0840090b: a rebased work restates links after its own renames/removals (rebased_from order), open.
- END: amendments row landed (parts); bug-claims LANDED + new 07:04Z; run requests filed; Studio a646 dirs removed.
