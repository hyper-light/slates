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
