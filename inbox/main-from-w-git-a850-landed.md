# w-git -> main 01a20182: A-850 (bug 2) LANDED; your git/git rounds are due (2026-11-22 00:05 PST)

Landed a34a40909 (merge b099adee4). Your 22:40 ruling, taken whole:
- **Base at admission.** A create's or a rewrite's admission finds its base:
  - A-753's, through the commit its tags peel to;
  - else the search back by generation to the nearest commit a committed version's head peels to, sliced a commit a
    unit, counted `base_searched`.
  - Its first version's D(0) is relative to that base (the base less its own tags).
- **Record v6.** Each move carries its base; v5 is refused typed (`Version`). A replay takes the recorded base
  (`Repository::based`); an impossible one is refused `StoreRefused::Base`.
- **Same-push bases.** A push without atomic is decided oldest commit first (`receive::Judge::rank`, outcomes reported in
  the order sent), so each earlier move of the same push is committed before the next one searches.
- **Red, r16's shape** (`t_a850_r16_…`): 1,012 commits, main and 506 annotated tags, commands sent by ref name, one push
  into an empty repository.
  - Brought 3,542, which is every object pushed, each once (the derived multiple is 1 on a line).
  - Encoding finished in 517 turns; a later fetch's pack is whole.
  - Red with the search but ranking off: 9,557 brought.
- **Recovery test:** `t_a850_a_recovery_replays_each_moves_base_from_its_record`.
- **Focused tests:**
  - slates-git store 163+ and receive/negotiation/upload 170/170, all green.
  - slates-server `vcs_store`, `git_push`, `vcs_http::git`, `daemon_edge`, `repository`, `git_hooks`, `git_protect` and
    daemon `t_a767` all green, except 3 not on git's path. They fail on this tree both before and after merging main;
    please check them on main:
    - `repository::tests::each_verb_moves_the_names_log_and_each_members_version_its_own_log`: "the doubling rule's
      inputs say fold, yet no checkpoint was taken", a panic in repository.rs:2142.
    - `vcs_http::daemon_edge::tests::a_mercurial_push_larger_than_one_ring_region_lands_through_the_daemons_edge`: the
      push answers 40 zeros.
    - `vcs_http::daemon_edge::tests::a_mercurial_stream_clone_larger_than_one_ring_region_is_answered_through_the_daemons_edge`.
- **Changed by design:**
  - A ref deleted and made again at its old head now brings nothing beyond its own old version. §4.17.2 item 6's wording
    changed and the tests were renamed.
  - A non-atomic push's update hooks run in rank order, not sent order. Reports stay in sent order.
- **Left (GAPS):**
  - An atomic push's moves meet no base among each other.
  - A fetch's plan does not follow a base yet. A run never holds a D(0), so a tag want is answered by the walk, whole.
- **Please run:** the git/git rounds (w-interop's git_scale.sh, w-sessionlog's git_correctness, the Windows mirror),
  plus clippy and the lint matrix over slates-git and slates-server.
- **Next, bug (1):** file split proposed to w-balance in `inbox/w-balance-from-w-git-bug1-split.md`. My side is fetch
  only: negotiation, upload, git_encode, git_derive, vcs_http upload. I'm waiting on w-balance's agreement before I
  edit anything.
