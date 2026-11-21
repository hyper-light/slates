# w-forming -> main, 2026-11-21: BLOCKED on a machine with stock hg 7.2 + svn 1.15 (goal 93 piece, step 2)
My goal 93 piece is not done: vcs_concurrency.rs is landed (7f1e5a13e) and compiles clean, but its one focused case
(N=2, hg same-head + svn same-file) has never run, and no reachable machine can run it:
- kiln `runners`: no runner has svn or uvx on its PATH (tool:svn, tool:uvx: none, every platform).
- Probed for /opt/homebrew/bin, /usr/local/bin, ~/.local/bin: Studio-4 (this machine, 2768d0d5) has none of them.
- adas-mac-studio.local (cec37f60, 2 GB free) and adas-mac-studio-2 (b24c5c95, 80 MB free, 3 CI jobs) did not answer
  in 15 s; adas-mac-studio.local-2 (d1323898) has failed every placement today ("did not answer the sync in 60s"), and
  the balancer keeps choosing it first.
- Installing stock Subversion/Mercurial is a tool install (CLAUDE.md §2 item 14): not mine without Ada's word.
What would unblock it (main's or Ada's call): disk freed on a Studio that already has the tools (the svn_interop and
hg_interop tests name "the Mac Studio"), or Ada authorizing an install (brew install subversion; uv for
`uvx --from mercurial==7.2 hg`) on one runner, or main running the focused case (commands in run-requests.md).
85b C3 and P4 also stay open: their rounds are main's under the 10:48 correction (commands in run-requests.md), and
they need the same kind of machine with room.
