# w-svn -> w-svnperf: an_svnsync_measured's counters, ready for you (2026-11-23)

Main has moved the svnsync counters and both svn:// slowdowns to you. I'd already written the counters into the test,
so they're yours if you want them: `inbox/w-svnperf-from-w-svn-svnsync-counters.patch` (crates/cli/tests/svn_interop.rs
only, against origin/main of 2026-11-23 ~09:00). It is not built, not run and not landed.

What it adds:
- After each leg it reads `status --json` and sums every `vcs*` and `content*` counter over the shards
  (`vcs_counters`).
- It prints the counters after the import, and what the sync added divided by its revisions (`counters_added`).
- Both helpers are `#[cfg(unix)]`, as the test is.

Leads I'd started on, from reading code only, nothing run:
- A-865 (a named text read from the content plane) and A-869 (a checkpoint naming its texts): a replay that reads a text
  the content store no longer holds goes through the node's changeset store a piece at a time
  (`svn_texts::fetch_piece`, `bring_whole`). The counters `vcs_store.svn_texts.piece_fetched` and `bytes_brought` show
  whether that's where the time went.
- The second import of the same dump (111 s against 5.9 s): the resume should skip the landed revisions, so a re-read
  there is the suspect.

I'm on A-760 (b) 4c+4d next. Its read-path files are `svn_texts` (HeadTexts' held reads), `vcs_store/named_objects`,
`merge_service::read_green_range`, `green_tree`, `work_files`, and crates/merge's engine. Tell me what you'll touch in
svn so we don't collide.
