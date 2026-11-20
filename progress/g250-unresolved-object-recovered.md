# g250-unresolved-object-recovered: progress (rewritten 2026-11-18 ~23:40 UTC from context after the ~/.cache wipe)

Goal: UI goal 84, kiln d9f7a7952515869a9600cdb9cb020359. "An unresolved object is recovered, not just kept (A-272 (ii),
live-durability). Adoptions are kept as records, so Settled rests on one; the current members run a round for an
Unresolved object, so f+1 counts the members that exist now, here the replacement holding the live log. Lineage gets its
own model, read as text only."

Role: implementer (Claude subagent of main). THE RULE (preamble, final): one piece, its --lib unit tests, cargo fmt,
one-crate clippy + cargo xtask check, merge, END. Build on the Mac Studio d1323898 (sync false, GitHub clone of a pushed
scratch ref, own CARGO_TARGET_DIR under ~/.cache there, deleted after). Never build on this MacBook.

## Landed on main (all g250)
- A-309 pieces 1-3: 1e8f4a49e. Completion records (Settled rests on a kept record, `RegionalConfiguration::completed`);
  Retirement.current (the replacement holding the live log is a survivor and asked); fallback ranking; the bound drops a
  carrier with what it carries; past tolerance the current members' round adopts (ReadyPastTolerance,
  fleet.takeover.recovered_past_tolerance); the live takeover past tolerance; docs/wip/models/Lineage.tla (text only);
  crates/db/tests/lineage_model.rs (the oracle).
- A-332 (past tolerance in the volume's loss report, CONTRACT 21): 85c15916d.
- A-335 (a stale copy is let go only once its holder keeps no content for it): 81687545c.
- A-340 (the retirement bound's drop of an incomplete takeover typed and counted): e599cae8a.
- A-341 (an unasked holder lets its copy go where the kept retirement shows the object within tolerance): 731763108.
- A-349 (a superseded owner ships nothing more): 41a797db1.
- A-362 (a takeover re-adopts over a superseded copy): 9706632ae.
- A-364 (GAP-A9-7 record half: ledger adopted position by position; FencedLedger.tla): 5a930fc64 (model 9140359a6).
- A-373 (a held position yields to a higher epoch's value): e667c60bd.
- A-394 (a verb forwarded to a successor before it made the volume is refused Overloaded): 5a6f1e6ee.
- A-412 (a holder behind its own records after a takeover catches up, fetching inputs from the record's owner):
  df23e81df (build 034016685). Unit: server lib merge_origin + merge_service 30/30 (Studio). Fleet test re-filed below.

## Left, in order (main starts each piece)
1. A-484 (NOW): A-341's owed piece (a). A completion record keeps what a holder needs to judge a copy first seen on it:
   the recovery owners' settled neighbourhoods and the hosts that stayed members through the takeover (the retirement's
   survivors still members at completion), so `within_tolerance` judges a record too and `Lineage::Settled` /
   `stale_after_confirmation` let an unasked copy go as Covered instead of kept_unasked. Count only survivors that are
   still members (a same-id re-admission can make a cohort look within tolerance now when it was not at the round).
   Regional layout 5 (codec, golden). Lineage model judges it; red with the record's judgment removed.
2. A-341's owed piece (b): the settle rule for a live owner. Only copies with no takeover in their line; holder outside
   the owner's neighbourhoods (a takeover asks whole neighbourhoods, not cohorts); owner owing no share of a takeover;
   no pending move of the object; settled cohort >= f+1 hosts and within tolerance; released through let_go_stale. The
   lineage model needs a new oracle for it (the A-309 oracle rejects every live-owner release).
3. Lineage.tla text for 1 and 2.
4. The content release signal (design written earlier, not built): a successor confirms only once adopted content is
   re-placed under its own placement; an owner's settle certifies content too; closes A-335's open release and A-276's
   named-holder item.
5. A clause-by-clause check of the goal text against main, each clause named with its test.

## Clones
- ~/slates-work/g250-catchup (A-412, landed). ~/slates-work/g250-unasked holds an older uncommitted lineage_model.rs
  change (the first, unsafe A-341 rule): left untouched. Other g250-* clones are landed work.

## Run requests filed (re-filed after the wipe)
- A-412's fleet test on main (Linux x86_64 and arm64, macOS).

## A-484 in progress (worktree ~/slates-work/g250-record, branch agents/g250-unresolved-object-recovered-record, from main 7113415eb)
Rule (decided): an unasked copy is judged by its WRITER's takeover (the first since the copy was written), never the last
in its line: the landed A-341 judged the last departed, so a copy whose writer's takeover was past tolerance (its
successor confirmed without learning the object) went at a later takeover within tolerance -- possibly the last copy.
Judged on the writer's kept retirement or its completion record; a record keeps `recovery` (the recovery owners' settled
neighbourhoods) and `held` (survivors still members at completion); a kept retirement counts survivors still members
(a non-survivor cohort host admitted again under its id no longer counts). Moved objects are never covered (a restored
acceptor cannot tell its writer from the move's owner). The acceptor keeps an in-RAM `written_by` (from_persisted: the
owner if never moved from a retired owner, else unknown -> kept).
Done (uncommitted): db register.rs (CompletedTakeover.recovery/held + recovery_cohorts, within_tolerance, confirmed_for,
copy_stale, complete_retirements, Acceptor::written_by, 4 new unit tests + 1 updated); cluster raft_wire.rs (layout 5,
codec, golden, lying-count test); server takeover.rs (unasked_copy_stale in stale_after_confirmation and the Settled
path, docs).
Next: server daemon.rs tests (A-341 test's record phase now covers; new failing test for the writer rule), lineage
model mirror, design A-484 + GAPS, Studio run (db/cluster/server lib + lineage model), red-check once, fmt, clippy x3,
xtask check, merge, push.

## 2026-11-19 ~00:30Z A-484 LANDED main 3d174a1cb (build 858078b15; merges 657abef2c, 3d174a1cb: conflicts only the amendment log's end)
- Rule: an unasked copy is judged by its writer's takeover (Acceptor::written_by; RegionalConfiguration::copy_stale =
  confirmed_for + within_tolerance), on the kept retirement or the completion record (CompletedTakeover::recovery,
  held; regional layout 5); within_tolerance counts survivors still members (kept) or the record's held hosts; moved
  objects never covered. Bug record docs/bugs/2026-11-18-an-unasked-copy-was-judged-by-the-last-takeover-in-its-line.md.
- Unit (Mac Studio, debug): db register:: 84/84; db lineage_model 4/4 (new: lagging holder judged on the record);
  cluster raft_wire:: 21/21; server takeover:: + holder tests 23/23 pre-merge, 24/24 post-merge; fmt clean; clippy -D
  warnings clean for slates-db, slates-cluster, slates-server; xtask check structural ok, literals none in my files
  (45 elsewhere: anchor, base, land -- main's routed findings).
- RED once each (mutated copy on the Studio): A-341's rule (last departed) -> server test copy let go at the second
  install (kept (true, false)); every member counted -> db test cohort within tolerance after re-admission
  ((false, true)).
- Hygiene: scratch refs wip-g250-a484-1..5 deleted; Studio clone and target deleted.
- Run requests filed (run-requests.md 2026-11-19T00:30Z): model beyond default; cluster/server suites + fleet takeover.

## Left, in order (main starts each)
1. A-341's owed piece (b): the settle rule for a live owner (copies with no takeover in their line; holder outside the
   owner's neighbourhoods; owner owing no share; no pending move; settled cohort >= f+1 hosts within tolerance;
   let_go_stale), with a new lineage-model oracle (the A-309 oracle rejects every live-owner release).
2. Lineage.tla text for A-341 and A-484.
3. The content release signal (successor confirms once content is re-placed; settle certifies content).
4. The clause-by-clause goal check against main, each clause with its test.

## 2026-11-19 ~01:20Z A-508 LANDED main 069303a1d (build 5b16f4a91; merge: conflict only the amendment log's end)
- Rule: a live owner's copy the owner wrote (written_by), held outside all of its neighbourhoods, goes as a stale copy
  goes (content kept while retained) where the owner's settled cohort for the object is whole and >= f + 1 wide
  (RegionalConfiguration::settled_away; takeover::settle_away, let_go_settled_away; fleet.settle.released,
  fleet.settle.kept_outside). Kept while a member has left the cohort, for a bootstrap's one-host cohort, moved objects.
- Unit (Studio, debug): db register:: 85/85; server takeover:: + holder tests 25/25. RED once (mutated copy, the live
  branch keeping every copy as main did): plain register stayed, (true, true).
- Not done under the new rule (agents compile only --lib): clippy, xtask, the lineage model -- filed in
  run-requests.md. Model mirror of A-508 + its oracle (a live owner's copy let go only while a record at least as new is
  on f + 1 members) is owed as its own piece (needs the model compiled, main's).
- Hygiene: scratch ref deleted; Studio clone + target deleted.

## Left, in order (main starts each)
1. The lineage model's mirror of A-508 and its oracle (main compiles/runs).
2. Lineage.tla text for A-341, A-484 and A-508.
3. The content release signal.
4. The clause-by-clause goal check.

## 2026-11-19 ~02:10Z A-518 LANDED main b69463eec (build a1cc8f8da; merge conflict only the amendment log's end)
- docs/wip/models/Lineage.tla: rules as actions (AskedSettled, AskedConfirmed, UnaskedCovered = A-341/A-484 writer's
  takeover on retirement or record, SettledAwayRule = A-508), state for writer/seq/commits/record cohorts+held;
  properties ReclaimOnlyStale (rules 1-3), SettleKeepsFPlusOne (rule 4), NoLastCopyLost (every rule); invariants
  TypeOK, SettledOnRecord, Bounded. MC_Lineage.tla + MC_Lineage.cfg (bootstrap, 1 object) + MC_Lineage_two.cfg.
  Text only, never parsed or run (no compiling under THE RULE); TLC filed for main.
- Left (main's queue): TLC results/counterexamples; the Rust lineage model's mirror of A-508; the content release
  signal; the clause-by-clause goal check.

## 2026-11-19 ~02:50Z A-529 LANDED main dc8679815 (build bee04aec3; merge conflict only the amendment log's end)
- The content release signal, first half: a takeover share is done only once every adopted volume is served from the
  successor's own placement (takeover::adopted_served, served_from_placement; share_done's 7th fact); the healer
  replaces an adopted volume's placement naming a member outside its candidates (heal_decision's adopted case,
  fleet.heal.adopted_replaced). Unit (Studio): server lib 29/29; RED once (gate ignored): [true, true, true].
- Left (main's queue): holders drop a stale copy's content on a certified confirmation / completion record (with a
  regional layout bump for mixed fleets); settle certifies content (A-508 holders drop content); replace at once for
  adopted volumes; the Rust lineage model's mirror of A-508; the clause-by-clause goal check; TLC results (r140).

## 2026-11-19 ~03:40Z A-538 LANDED main 26d91cc73 (build 4e8bf15db; two merges, conflicts only the amendment log's end)
- The content release signal's second half: Retirement/CompletedTakeover::certified, certifies_content; regional layout
  6 (layout 5 still read, every takeover uncertified); a stale copy judged on a certified confirmation, its holder
  outside the successor's placement, releases the content its own stale records name (content_retention::
  release_certified, fleet.takeover.released_certified), keeping a placement ahead. Settled-line lagging holders keep
  content as before.
- Unit (Studio): db register 86/86; cluster raft_wire 22/22; server 33/33 (and 33/33 after the first merge). RED once
  (an uncertified takeover's content let go): the uncertified A-335 test failed.
- Left (main's queue): an owner's settle certifies content (A-508's holders drop content); the Rust lineage model's
  mirror of A-508; the clause-by-clause goal check; TLC (r140).

## 2026-11-19 ~04:40Z A-546 LANDED main acf4dbecc (build 30f9bf10f; merge conflict only the amendment log's end)
- An owner's settle certifies content: settle report waits on content_placed_on_current_neighbourhood; the healer
  replaces (unsettled_replace, fleet.heal.unsettled_replaced) a placement naming a host outside the current
  neighbourhood while the change is in flight; settle_certified set in the configuration (regional layout 7; 6 and 5
  read uncertified); A-508's let_go_settled_away releases the content its own records name on a certified settle
  (fleet.settle.released_content). Unit (Studio): db 87/87, cluster 22/22, server 35/35; RED once (uncertified
  settle's content let go): A-508's uncertified test lost its head, (false, false).
- Left (main's queue): the Rust lineage model's mirror of A-508; the clause-by-clause goal check; TLC (r140);
  a settled line's lagging holder still keeps content.
- Hygiene owed: the Studio (d1323898, adas-mac-studio.local-2) went offline mid-cleanup (runner restart); its
  ~/.cache/g250-src-j and ~/.cache/slates-work-target-g250 may remain: delete when it is back. Scratch ref deleted.

## 2026-11-19 ~05:40Z A-556 LANDED main 01e4137be (no merge needed: main had not moved)
- The live-durability second failure (GAPS 2026-11-09): readiness_cohorts / readiness_current_cohorts / falls_through
  narrow a round's wait to the carried retirements whose round ranked the object into the departed host; successors
  still ranked over every recovery cohort; within_tolerance (kept case) judges the narrowed cohorts. Also raft_wire's
  encode_regional_at split (encode_retired) for main's clippy finding. Unit (Studio): db 88/88, cluster 22/22, server
  28/28; RED once (every carried cohort joined): (2, 2, 2).
- Studio hygiene: the A-546 leftover clone (g250-src-j) deleted; this piece's clone g250-src-k and the target remain
  until the next cleanup (deleted below if the runner answers).
- Left (main's queue): the Rust lineage model's mirror of A-508 (and of A-556's narrowing); the clause-by-clause goal
  check; TLC's second configuration (main reruns smaller).

## 2026-11-19 ~07:00Z A-585 LANDED main c59ab83d5 (goal 81 dbe13f39, chaos across real processes, failure half)
- ledger.rs: Cohort::lose_store (a store lost under a replica), Holder::lost: a lost holder answers no read and
  accepts no record under its old id (the takeover's quorum and the offer skip it), as the server's fresh member id per
  start guarantees. Tests: a_holder_whose_store_is_lost_answers_nothing_under_its_old_id (red before: takeover adopted
  an empty log, Ok(0)), a_write_its_client_left_is_kept_or_dropped_whole. Unit (Studio): db ledger 3/3.
- Filed: the store-destroyed and candidate-killed live_chaos flows; a client-killed-mid-write process flow is owed (no
  such flow exists yet).

## 2026-11-19 03:11Z A-585 piece 2 LANDED main d418d2f15 (goal 81 dbe13f39, chaos across real processes, failure half)
- crates/cli/tests/live_chaos.rs: `a_client_killed_mid_write_leaves_its_write_whole_or_absent_through_a_takeover`
  (commits 792b12380 and 9c05e8792). The flow runs three bare daemons and streams 20 acknowledged writes. It stops both
  candidates, then starts a `slates write` client (the same client-process route as the clients-killed-waiting flow;
  live_chaos has no re-invoked test-binary role). It SIGKILLs the client once the owner's `fleet.live.awaited` moves,
  which means the write is parked between sent and answered. Then it kills the owner's node and resumes the candidates.
  The next owner is judged by NoLoss (the killed write absent or whole), StaleNeverCommits, Agreement, Continuity and
  ReadSafety, through `judge_takeover`. Bare, not anchored: an anchor ends a stopped daemon once its beat lapses and
  starts one holding no copy of the log, a second fault past f. The design's A-585 addendum and its GAPS line are
  updated.
- Compiled nothing: the piece is an integration test with no library helper. Filed: clippy for slates-cli, and 24 runs
  on each Linux architecture (run-requests.md, 03:11Z).
- Left: main compiles and runs it. A compile error or a red run comes back to me as the next piece.

## 2026-11-19 A-602 STARTED (goal 92 51cef9f7, §4.17.4 Subversion piece 8: locks in slates-svn)
- Registered A-602 in amendments.tsv (next free moved to A-603). Worktree ~/slates-work/g250-svn-locks, branch
  agents/g250-svn-locks from origin/main e85c17f9f. Nothing written yet; reading §4.17.4 items 3.4 and 7, the notes' §7,
  partition.rs's guard-then-apply pattern and the svn crate's pieces.
- LANDED main 8df36c272 (9e260b414), 2026-11-19. `crates/svn/src/lock.rs` holds the lock table. Its three guards are
  `lock` (with steal), `unlock` (with break) and `verdict` (the check, plus release or keep). Each returns
  `Decision { entries, answers }`, and `apply` applies an entry unconditionally (the partition.rs pattern).
  - The head is the `HeadView` trait. The clock is a `now: AprTime`, which `AprTime::from_wall_ns` makes from the seam's
    `Clock::wall_ns`. Token bytes come from a `draw` closure, so the crate stays pure.
  - Stated rules (§4.17.4 item 7): an expired lock met is removed by a recorded entry; a refused commit records
    nothing; the room purges expired locks before refusing `E160000`; a NUL in a comment or token is refused.
  - The oracle is `Fsfs` in `lock/tests.rs`: lock.c, fs-loader.c, fs-wrap.c, transaction.c and serve.c transcribed.
    Its census met every rule. There are 20 case tests.
  - Unit runs (Studio): slates-svn lib 74 of 74, fmt clean. Red once, on a copy with the owner check skipped.
- Evidence: the Subversion sources at d434dc14 (lock.c and others, sha256 prefixes in the research notes §1) are kept
  in ~/.local/state/slates-fanout/g250-scratch/svn-src-d434dc14.
- Hygiene: the scratch ref wip-g250-a602-1 is deleted. The Studio clones g250-src-k/l/m and its target are deleted.
  The worktree is removed.
- Filed: clippy and xtask on slates-svn, and the oracle at 4096 cases (optional).
- Left of locks: the wire verbs (piece 7), the hooks (piece 11), DAV LOCK/UNLOCK (piece 13), the entries' register
  encoding, a HeadView over the index, and the room from the shard. g268's commit rule (A-592) calls
  `LockTable::verdict(changes, &CommitLocks, now)`.

## 2026-11-19 A-609 STARTED (goal 93, §4.17.4 Subversion piece 13 part 1: HTTPv2/DeltaV XML bodies as codecs)
- A-609 registered by main. Reading the notes §3, webdav-protocol, http-protocol-v2, ra_serf and mod_dav_svn sources at
  d434dc14 (scratch: ~/.local/state/slates-fanout/g250-scratch/svn-src-d434dc14). Worktree to come:
  ~/slates-work/g250-svn-dav, branch agents/g250-svn-dav.
- LANDED main dbd6d0d92 (9b16fbd4f), 2026-11-19. `crates/svn/src/dav.rs` holds the module, with these parts:
  - `dav/xml.rs`: a bounded reader with no DTD ever, and the writer's escapes and base64 lines;
  - `dav/request.rs`: OPTIONS, PROPFIND, MERGE with its lock-token list, and ten REPORTs, decoded as mod_dav_svn reads
    them;
  - `dav/response.rs`: OPTIONS headers and body, the small reports, locks, mergeinfo, inherited props, log, MERGE and
    multistatus, written byte for byte;
  - `dav/time.rs`: svn dates;
  - 23 fixtures under `dav/fixtures`: 22 recorded from stock svn 1.15.0 against a recorded responder on the Studio,
    and get-deleted-rev from ra_serf's writer.
- Unit runs (Studio): 98 of 98 on the branch, 178 of 178 on the merged tree; fmt clean. Red once on a copy (unknown
  entity read as `?`).
- Left (part 2 of piece 13): the update-report and replay editor drives (svndiff windows in base64), PROPPATCH,
  LOCK/UNLOCK bodies, list-report and file-revs-report. Main's: clippy and xtask, and the live mod_dav_svn capture.
- Hygiene: scratch refs wip-g250-a609-1..3 deleted; Studio clones and target deleted; worktree removed. g268 owns the
  lock.rs order fix (A-617); A-609 kept out of lock.rs.

## 2026-11-19 lints LANDED main b24a8c826 (ee51ac193): svn tests' prop_oneof! and unwrap-in-Result fixed (main's clippy).

## 2026-11-19 A-627 STARTED (goal 93, §4.17.4 piece 13 part 2 in the dav module)
- Registered A-627. The scope is the update-report and replay editor drives (svndiff base64 over the index), PROPPATCH,
  LOCK and UNLOCK with Timeout against the A-602 table, and list-report and file-revs-report. The golden vectors come
  from the recorded responder (~/.local/state/slates-fanout/g250-scratch/dav-capture) plus mod_dav_svn's format
  strings (sources in g250-scratch/svn-src-d434dc14). Failing tests first.
- A-627 progress: worktree ~/slates-work/g250-svn-dav2 (branch agents/g250-svn-dav2). Four new fixtures are captured
  from svn 1.15.0 on the Studio: list-report (svn ls -v), file-revs-report (svn blame), proppatch-revprop (svn
  propset --revprop) and lock (svn lock -m). UNLOCK has no body; it carries a Lock-Token header.
- A-627 notes: file-revs' txdelta is base64 with NO line breaks (`dav_svn__make_base64_output_stream`:
  `svn_base64_encode2(stream, FALSE)`), while property values break lines. The svndiff Encoder API is
  `new(Version::{Zero,Two})`, `header()` and `window(&Window, Compression::{Stored,Lz4}, &mut out)`. list-report's item
  and file-revs' formats are read (reports/list.c, file-revs.c).
- A-627 LANDED in part, main 4c7208ab0 (c9939e60c), 2026-11-19.
  - `dav/listing.rs`: list-report and file-revs-report, read and answered byte for byte; a file revision's delta goes
    through svndiff::Encoder in flat base64.
  - `dav/locking.rs`: PROPPATCH changes, LOCK's lockinfo, Timeout (Second-N / Infinite) and Lock-Token.
  - Four new fixtures, captured from stock svn 1.15.0. 11 tests; 283/283 on the Studio; red once.
- Left of A-627 (next part): the update-report answer (update.c's editor drive: target-revision, open/add dir and file
  with checked-in hrefs, delete-entry, absent, set/remove-prop, txdelta in flat base64, the md5 prop), the replay
  drive (replay.c), LOCK's answer (util_lock.c activelock plus X-SVN-Creation-Date and X-SVN-Lock-Owner), and the
  mapping of LOCK and UNLOCK onto A-602's guards. update.c's format lines are noted above (255-268, 360-410, 440-500,
  541-574, 600-650, 770-800, 850-885).
- Hygiene: scratch refs deleted; worktree removed; Studio clone and target deleted.
- A-627 continued LANDED: main b524f0440 (059b095d2). `dav/drive.rs` is the update-report drive in send-all mode,
  byte for byte against update.c, plus 3 tests; 286/286 on the branch and on the merged tree; red once.
- STILL OWED for A-627 (next part):
  - the replay drive: replay.c's format lines, the editor-report;
  - LOCK's answer: util_lock.c `dav_lock_get_activelock`, plus mod_dav_svn lock.c's X-SVN-Creation-Date and
    X-SVN-Lock-Owner headers;
  - LOCK and UNLOCK onto the A-602 guards. Merge main first, since g268's A-617 changed lock.rs's answer order. Map
    `locking::lockinfo` + `timeout` to `LockRequest`, and `lock_token` + break to `UnlockRequest`.
- Hygiene: scratch refs deleted; worktree removed; Studio clones and target deleted.
