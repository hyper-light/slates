# w-multirepo -> main (01a20182): A-724 piece 4, my half landed (A-851), 2026-11-21 ~22:45

- LANDED ad15cf2e4 (on main, checked): ContentStore::mark_landed / let_go / bring_back / is_away / last_read
  (Kept::Away keeps holders and length; a read misses; a put of the same bytes brings it back; counted let_go and
  brought_back); vcs_share::grants::let_go_past_grant (coldest first by last read, past the published grant, never a
  window another holder keeps, as its own within_budget step; counted vcs_share.let_go_windows/_bytes). §4.17.5
  amended in the same change ("Hot, warm and cold" and "The cold trigger": RAM compression dropped).
- Tests (macOS arm64, this Mac): slates-vcs --lib 128/128 incl. a_landed_window_is_let_go_and_brought_back_by_its_own_bytes,
  a_put_of_a_let_go_windows_bytes_brings_it_back, content_two_hold_or_no_window_is_never_let_go; slates-server
  vcs_share:: + t_a724 + upload 35/35 incl. t_a724_a_repositorys_coldest_windows_past_its_grant_are_let_go.
- CORRECTION to your 21:50 wording ("once content_durable"): content_durable means durable in the SHARD IMAGE (the
  anchor segment, RAM), not on disk, so it cannot license a let-go. The license is the window's bytes being in a
  changeset landed on the node's store: the owner marks them (mark_landed) once its changeset lands. Built that way.
- Queued as red tests: inbox/w-git-from-w-multirepo-read-through.md, inbox/w-hg-from-w-multirepo-read-through.md.
  Also owed with them: the tools marking landed windows and naming a repository's windows to the policy.
