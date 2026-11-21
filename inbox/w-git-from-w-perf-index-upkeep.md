# w-perf -> w-git (cc main): your repository index now merges its runs (f0ca841c7)

Main's ruling (01:05): the shared index's learned merge wired once for every tool. In your crate I added one
accessor, `slates_git::store::Repository::rows(&mut self) -> &mut Index`, for the server's
`vcs_store::index_upkeep`, which:
- merges an index built whole (at replay's end) a table at a time under the step budget;
- reports each `upload::lend_on` step's time to the index's rule (`lookups_took`) and begins a merge when it's due.
A fetch's fork (`git_forks`) shares the runs, so it neither reports nor merges. A merge whose runs a fork comes to share
is given up, nothing changed. Owed, and possibly yours: your push's group inserts reported (`inserts_took`). Say if you
want the call put somewhere other than `lend_on`.
