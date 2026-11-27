# A warm restart's map remake hashed a let-go file

- **Date:** found 2026-11-27 (main's diag CI 9, run 46018002586 at 56b609053: windows-latest and windows-11-arm; then
  macOS at 68e6fe9e3); bisected by main to 11168fab3 (41bc4bbf0 good, 11168fab3 bad); diagnosed and fixed the same day
  (w-perf, main's ruling of 11:09).
- **Status:** fixed in `crates/server/src/vcs_store/paths.rs` (`remake_green_sliced`, `put_file_whole`,
  `let_go_brought_again`, `count`) and `crates/vcs/src/content.rs` (`ContentStore::aways_of`, `let_go_again`).
- **Subsystem:** a git green's path map and git trees made again by a recovery (A-664 part 3, sliced), over windows the
  image restored let go (A-767 C1 step 2b).
- **Severity:** a warm restart lost a let-go file's list, name and window from the content store, so the object no
  longer read by its identity. Before 11168fab3 the same restart named a wrong git tree for the green, silently.

## Symptom

```
daemon::tests::t_c1_windows_let_go_survive_warm_restarts_whole_and_come_back_by_their_bytes
the store whole as published, none refused
  left: (Some(([], [], [])), 0)
```

The successor's image was whole: its committed paged entries were its predecessor's 11, and `split_held` gave 6 windows,
1 list, 1 name, 2 region holds and 1 window let go. After the recovery the store held no list, name or window let go.
The successor's counters: `vcs_paths.refused` 1, `vcs_store.windows_unheld` 1, nothing logged.

## Root cause

A recovery replays each git repository's records, then makes each green's map and trees again from its head's files
(`paths::remake_green_sliced`), then lets go what its landed packs owe. The remake puts each file
(`ContentStore::put_object`) and builds the trees, which hash each blob by reading the file through
`ContentStore::windows` (`slates_git::derived` `Work`, tree.rs). The 1 MiB file's window that its predecessor let go was
restored let go, and a plain put holds an object without bringing its windows back.

- Since 11168fab3 the walk yields a typed miss there (`WindowMiss::Away`), and the build was refused
  `TreesRefused::Content(Missed)`. `paths::count` counted the refusal and logged nothing. With no map, nothing held the
  file, and `release_unheld_windows` freed its root, list, name and window let go.
- Before 11168fab3 the walk ended at the let-go window without a word, so the hash ran over the short file: the remade
  map named a blob id and a root tree other than the predecessor's. t_c1 compared only the store, so it passed.

## Impact

Any warm restart of a daemon holding a git green whose head file had a window let go: before 11168fab3, a green whose
remade trees named the wrong blob, so its next made commit named another tree than the one its versions held; since
11168fab3, the file's content freed from the store, readable only by bringing it back from a landed pack.

## Fix

1. `put_file_whole`: a recovery's remake notes each window of a head file the store holds let go
   (`ContentStore::aways_of`, with its let-go number), then puts the file with `ContentStore::put_object_whole`, which
   brings each back from the file's bytes. Those are the green's own (`engine.content`), in hand, so no read of the
   node's store is needed.
2. `let_go_brought_again`: once the map and trees are made, each noted window is let go again at its own number
   (`ContentStore::let_go_again`), a unit of the recovery's slices each, so the warm restart keeps out of RAM what its
   predecessor let go and the image's aways are unchanged. Counted `vcs_paths.remake_brought_back`; one not let go again
   is counted `vcs_paths.remake_kept` and logged.
3. `paths::count` logs every refused step with its green and why, not only a named file's.

Red: t_c1 now also asserts, each round, `vcs_paths.refused` 0 on the successor and the green's root tree id the
predecessor's (`green_tree_id`). At main's tip it read `(Some(([], [], [])), 0, 1, None)`: store empty, one refusal, no
map. With the hash put back to its silent short read of before 11168fab3: the store as published and none refused,
but the root tree `sha1:ad92c4a1…` against the predecessor's `sha1:892a5d84…`. Now: green.
`content::tests::a_window_brought_back_to_read_its_object_whole_is_let_go_again_at_its_number` holds the store's half.

## Siblings swept

- The live whole builds (`paths::step` with `whole`, `put_whole`, `put_change`) put files with `put_object` too, and
  their trees hash through the same walk: a live whole build of a green whose file has a window let go would be refused
  the same way. Not changed here; reported to main.
