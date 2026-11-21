# w-hg -> main: a clone sends manifests as their stored deltas (f98d99685), and three git tests failing on main

**Landed: f98d99685** (base c7335062d). `getbundle` sent every revision as a whole text against the null base. For hg's
history that's about 57,000 manifests of up to 179 KB per clone, each rebuilt from its chain on the server and diffed
again by the client. Now a manifest kept as a delta (A-729) goes out as that delta, lent in place
(`Store::manifest_delta`), against the manifest sent just before it in the same answer. Anything else goes whole, as
before.
- Red first: `serve::indexed::tests::a_clone_sends_a_manifest_kept_as_a_delta_as_that_delta` ("every manifest was sent
  whole" with the lend disabled). Each manifest is rebuilt from its base and checked against its node.
- slates-hg lib 372/372. `slates-cli --test hg_interop` against stock hg 7.2 all pass (clones over HTTP and SSH, round
  trips, bookmarks, pushkeys, the forced head). Local macOS.
- Owed: file revisions the same way, once the changeset shape keeps Mercurial's deltas (the store keeps them whole
  today). The clone's time against stock's 283 s needs a release run, waiting on a runner (r42 still queued on
  adacachy).

**For w-git (please relay): three git tests fail on main**, at f98d99685's base, run locally on this Mac at load 37-47,
twice:
- `vcs_http::daemon_edge::tests::a_long_history_pushed_over_the_daemons_edge_is_fetched_whole_over_it`
- `a_push_of_a_long_history_and_many_branches_lands_every_ref_through_the_daemons_edge`
- `a_partial_clones_lazy_fetch_of_many_blobs_is_answered_over_the_daemons_edge`
Each panics at daemon_edge.rs:396 with "the answer ended inside a line (Err(Os { code: 35, kind: WouldBlock }))": the
test socket's read timed out before the first byte. My change doesn't reach them (git paths). They may be timing under
load, or w-git's AtHead change.
