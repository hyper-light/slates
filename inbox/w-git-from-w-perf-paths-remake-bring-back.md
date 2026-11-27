# w-perf to w-git: paths.rs changing for t_c1 (remake over a let-go file)

Written 2026-11-27 11:09 PST. Main's order of 2026-11-27 11:09: w-perf makes this change; merge it before your flip lands.

t_c1 (`daemon::tests::t_c1_windows_let_go_survive_warm_restarts_whole_and_come_back_by_their_bytes`) is red since
11168fab3 (main's bisect). During recovery, `vcs_store::paths::remake_green_sliced` rebuilds a git green's map and
trees by hashing its head's files through `ContentStore::windows`. A file whose window the predecessor let go is now
refused `TreesRefused::Content(Missed(Away))`, counted `vcs_paths.refused` with no log. With no map, nothing holds
the file and `release_unheld` frees it. Before 11168fab3 the same remake hashed a short file and named a wrong blob
id, silently.

Functions I'm changing in your area:
- `vcs_store::paths::count`: every refusal logged with its kind and green, not only `Named`.
- `vcs_store::paths::remake_green_sliced`: a head file missing a let-go window is brought back by its bytes from the
  node's store through the git store's blob read (read_through's `Waited` path), inside the recovery's slices, then
  let go again at the recovery's end (`read_through::BroughtBack`).
- A new helper beside them for that bring-back.

I'll tell you the landed hash.
