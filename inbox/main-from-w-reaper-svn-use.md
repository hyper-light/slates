# w-reaper -> main: svn correctness by use, the test landed (2026-11-21)

- **Landed:** `svn_interop::a_dump_reads_and_writes_alike_through_slates_and_stock_svnserve`, #[ignore]d, as
  007bbca86 and 33dc5f546 (origin/main 41bdf1bbe).
  - It runs stock svn 1.15 against stock svnserve and slates' svn:// listener over one dump: log, info, checkout,
    `update -r`, switch, blame, `diff -c`, revprops, `svnrdump dump`, an svnsync mirror's `svnadmin dump`, and the
    writes (commit, copy, merge, a revprop set).
  - It compares each answer byte for byte with the URLs made plain, and skips loudly without the tools or the dump.
- **Focused proof** (r37, the big Studio): the golden dump, N=2, log, 0 differences.
- **Its first run found a false difference, fixed in the test:** `svn --xml` writes attributes in hash order.
  `canonical_tags` sorts them, held by an always-on test (`an_xml_tags_attribute_order_is_no_part_of_an_answer`).
- **The rounds** are in run-requests.md: git-2000.dump, the command, the knobs, and what red looks like.
- **Owed in the test:** the svn+ssh and HTTP v1/v2 legs, each another `UseLeg` beside svn://.

## Later, 2026-11-21 ~12:30 PST: the svn+ssh and HTTP routes are written but unproven (blocked on kiln)
- **The change:** b8b78e729 on the branch agents/w-reaper-svnuse. One anchor serves all three edges; every verb runs
  per route (svn://, http:// DAV, svn+ssh://) against the one stock control; `SLATES_SVN_USE_ROUTES` picks them. The
  writes run on the first route alone, since a second would write the control past the slates repository the other
  routes serve. It compiles and fmt is clean.
- **HTTP v1 vs v2:** stock svn speaks whatever version the server offers, so this exercises v2. v1 can't be forced
  from the client.
- **Why it isn't proven:** its one focused case (the golden dump, N=2, log, all three routes) never ran. kiln dropped
  r38 and r40 before they started (the Studio's clone stayed at 33dc5f546), and a foreground run timed out on kiln
  itself.
- **To land it:** run the case once on adas-mac-studio.local-2:
  `SLATES_TEST_CLI=1 SLATES_SVN_USE_DUMP=crates/svn/fixtures/dump/full.dump SLATES_SVN_USE_REVISIONS=2 SLATES_SVN_USE_VERBS=log cargo test -p slates-cli --test svn_interop -- --ignored a_dump_reads_and_writes_alike --nocapture`
  at the branch's tip. If it's green, merge the branch, or tell me to try again once kiln answers.
- **The Studio's scratch:** ~/.cache/w-reaper-clone, ~/.cache/w-reaper-target and ~/.cache/w-reaper-svnuse.log.

## Later, 2026-11-21: the routes landed by someone else; my branch is dropped
- **Superseded:** 013c0f0bf on main (SLATES_SVN_USE_ROUTE: svn, http, http-v1, ssh) does what my branch b8b78e729
  did, plus an http-v1 leg. I've dropped my branch (agents/w-reaper-svnuse deleted) rather than merge over it.
- **Check the http-v1 leg before trusting it:** it tells the client SVN_I_LIKE_LATENCY_SO_IGNORE_HTTPV2. From memory,
  not verified: ra_serf honours that only in builds compiled with SVN_DEBUG (options.c), and Homebrew's svn 1.15 on
  the big Studio is a release build. If so, the leg silently speaks HTTPv2. The server counts v1 activities
  (`vcs_http.svn_v1_activities`, A-766); one MKACTIVITY on that counter after the leg's commit proves it really ran v1.
  A daemon can't withhold v2 except in the server's own unit tests (`V1_ONLY_PREFIX`, cfg(test)), so A-766 part 4's
  server test is the v1 path that holds either way.
- **What's mine on main:** the by-use test itself (007bbca86, 33dc5f546). The routes' focused proof stays owed
  (013c0f0bf: main's call).
- **Scratch:** my clone and target here are deleted. Still on the big Studio: ~/.cache/w-reaper-clone,
  ~/.cache/w-reaper-target and ~/.cache/w-reaper-svnuse.log. My runner's kiln can't reach the Studio right now, so
  those need main.
