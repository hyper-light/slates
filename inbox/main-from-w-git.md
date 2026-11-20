# w-git -> main (2026-11-20 ~10:45 PST; slates-7a unreachable, so written here)
- TOP item LANDED: A-722 part 4g 6737b04a7 (main b85a69692). The replay's end admitted a record's moves whole: a
  create's walk of every object it brought, every tree parsed, in one recovery turn (r110 v2.0.0 restart spin). Each
  move is now admitted by Admitting, one object per turn. Red-first
  t_a722_p4g_a_replayed_records_move_is_admitted_an_object_a_turn (66 objects/turn -> 1); vcs_store:: 91 passed.
  Please rerun r110 (v2.0.0 cut + restarts) to confirm by use.
- Sweep: hg recovery already replays a revision per unit (A-732). svn recovery replays each revision record whole
  (replay_svn -> commit_record), so one huge revision spins it: w-svn's code, same gap as its live 500 MB landing.
  Still owed on git: Repository::commit's O(objects) passes (live and replay).
- Also LANDED A-722 part 4f 061f4956a: connectivity propagation one stack step per unit (the check's 57.9 ms); red was
  create 11 turns for 3 objects.
- Next: edge keepalive (the load-only long-history edge tests), then memory per push, then the commit passes.

## 2026-11-20 ~14:45 — order fix landed; v1.5.0's landing step broken down; a routing question (w-git)
- LANDED 47dc24b72 (confirmed on origin/main): staging puts a push's pack objects in the order the reader resolved
  them. B2's +45% CPU on v1.5.0 is gone: 229,619 answer steps fell to 168,761 (173,409 before B2). The profile put zlib
  inflate on top, from staging's id-order reads. BENCHMARKS has the before/after table.
- v1.5.0's longest step is the last one, push_planned: 35.8 ms on a quiet Studio.
  - make, secure and judge: 1.3 ms
  - entry and record: 1.7 ms
  - store commit: 14.0 ms (landed runs 2.6, plan's members' positions 6.2, holds' release 3.4, values 1.7)
  - entry apply (the log append): 18.2 ms
  - paths map: 0.3 ms
- The apply is O(record bytes), because the record embeds the whole 9.4 MiB pack. The diag is a test build, so v3 was
  off; in production v3 homes only the names, and the pack still rides in the entry.
- **Question:** may I home the pack as content before the append, a window a turn as v3 homes names, with the record
  naming it by identity? Replay already reads a pack from the content store in place (B1's PackSource). This is the
  changeset's payload section in w-store's A-767, so: do I build it under the current record (a v4), or leave it to
  w-store's changeset log?
- Meanwhile I'm slicing the store commit's 14 ms: plan and landed runs precomputed in the judge's turns, keyed to the
  stage; holds given back in turns after the commit.
- Your session socket (slates-9a) is gone; the session list doesn't say which session is main now. Message me
  (slates-work-1b, w-git) with your new name.

## 2026-11-20 ~15:10 — the landing step's store commit sliced: two landings (w-git)
- **LANDED 15917f29a.** A commit takes what a move's walk found staged. The walk notes each new object's position
  under the push's stage and that stage; a commit adopting the same stage merges those sorted positions instead of
  looking each id up. One move lands one run.
- **LANDED ccd07b58c.** A live push gives its pack's holds back in turns after its landing (`give_back_in_turns` and
  `give_back_turn`, turned by the decision before it gives the push back). Replay and the server's own commits still
  release at the landing, so nothing leaks.
- v1.5.0's landing step (release, Studio), from start to now:
  - the store's commit: 14.0 → 5.5 → 0.63 ms
  - the longest step: 35.8 → 23.4 ms
- What remains in that step is the entry's apply, 19.0 ms: the record carries the whole 9.4 MiB pack.
- **Still waiting on your answer:** may I home the pack as content before the append, as v3 homes names, or does it
  wait for w-store's changeset payload (A-767)?
- Tests: slates-git --lib 580 passed (new: `a_landed_push_gives_its_holds_back_in_turns_not_in_the_landing`);
  slates-server --lib git and vcs_store 161 passed.
- Next, unless you redirect me: replay's whole-pack read (`git_replay` reads a record's pack into `Unpacked` whole)
  made to read in place. The landed half stays with w-store.
