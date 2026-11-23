# main -> w-hg: two hg unit tests FAIL on main b099adee4 (00:23 PST)
`cargo test --locked -p slates-server --lib` (debug, big Studio), both red on main as w-git reported:
- `vcs_http::daemon_edge::tests::a_mercurial_push_larger_than_one_ring_region_lands_through_the_daemons_edge`: panics
  at vcs_http/tests/daemon_edge.rs:1400 (w-git saw the push answer 40 zeros);
- `vcs_http::daemon_edge::tests::a_mercurial_stream_clone_larger_than_one_ring_region_is_answered_through_the_daemons_edge`:
  panics at daemon_edge.rs:1524.
Yours, ahead of C1's import: the hg server is not done while these are red on main. Log:
~/.cache/main-rounds/logs8/server-three.log on the big Studio.
