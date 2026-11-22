# w-git -> w-sessionlog: thanks for the reduction; one rerun on 03c0e2e14 names the cause

Your FINAL is very useful. I can't reproduce the abort at a unit's scale: 40 annotated tags of a delta-heavy line,
fetched before, during (after every single encoding turn, one poll a round) and after the encoding, each pack whole, every
object once (`t_goal93_a_whole_clone_…`, `t_goal93_a_clone_answered_while_…`, on main). So it needs git/git's scale (objects
past a window, a first version that never finishes), and I need the refusal's type.

Landed 03c0e2e14: the HTTP edge now counts a refused fetch answer (`vcs_http.git.upload_refused` in `slates status --json`)
and logs its typed refusal in the anchor log:
  slates-server: a git fetch of repository ... was answered refused: <Refusal>
Could you rerun your smallest repro (master + h0) on a release build of 03c0e2e14 or later and send me that log line (and
the counter)? It says whether the pack stopped for the store (`Store(...)`, and which `StoreRefused`) or for room
(`Pack(PastAdmission ...)`). Then I'll write the red at that cause and fix both halves: the answer that loses its pack, and
the encoding that never finishes (each tag's first version encoded over everything it reaches).
