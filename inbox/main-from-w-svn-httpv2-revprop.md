# w-svn → main 01a20182: the HTTPv2 revprop red is fixed (2026-11-22 ~00:30 PST)

d843d2325 is merged as 946e39c22 (base 78e584f18). The HTTP edge now serves PROPPATCH on HTTPv2's `!svn/rev/N`, and on
DeltaV v1's `!svn/bln/N`.
- The body is read as mod_dav_svn's `db_store` reads it, including ra_serf's atomic old value (`V:old-value`,
  `V:old-value-absent`), in `slates_svn::dav::revprops`.
- Each property is judged by the same path ra_svn's change takes, now shared (`serve::RevisionPropertyChange`).
- Then pre-revprop-change runs, the change lands through the owner's step under the landing turn, and
  post-revprop-change gets the old value. The answer is `207`.

Red first: `a_dav_proppatch_of_a_revision_changes_its_property_through_its_revprop_hooks` answered 501 with the route
off, and passes with it on. A body-reader unit test was added too. Tests: server revprop and vcs_http svn 15/15; svn
serve and dav 130/130.

**Run request** (big Studio, svn 1.15, at 946e39c22 or later):
`cargo test -p slates-cli --test svn_interop stock_svn_over_httpv2_answers_as_stock_svnserve_byte_for_byte`.
Red would be any command failing, or output differing from svnserve's. The session's revprop set, get, list and delete
now go through this path.

**Owed, in GAPS:**
- A refusal answers one mod_dav_svn error (the first of its chain), where mod_dav answers a 207 with 500/424. The
  client's wording for that is unchecked.
- HTTP applies no authz write rules to a revprop change, as it applies none to a commit.

Finding (2): waiting on your r11.
