### A-617 part 1 — A-602's lock check answers the first failing lock in Subversion's path order, as FSFS's `verify_locks` does (2026-11-19, g268 for UI goal 93; §4.17.4 item 3.4; A-592's finding, A-602)
Applied in the same change to: §4.17.4 item 3.4 (the order the first failing lock answers in), GAPS (the §4.17.4
entry: A-592's finding fixed), `crates/svn/src/lock.rs` (`LockTable::verdict` through `met`, `in_path_order` and
`path_order`), `crates/svn/src/lock/tests.rs` (the model returns at the first failing check; the census counts the
commits the order decided), `crates/svn/src/commit/tests.rs` (A-592's scratch case, committed; the transcription's
order beneath a path read whole).
- **What was there.** `LockTable::verdict` gathered the locks a commit's changes meet into a map by the locks' paths and
  verified them in its byte order. FSFS's `verify_locks` sorts the changed paths with `svn_sort_compare_items_as_paths`
  (`path.c` `svn_path_compare_paths`: a path's children right after it, before its later siblings) and returns at the
  first path whose check fails (`transaction.c` 3537-3539, 3578). So a commit refused by locks at both `/t/a/b` and
  `/t/a-b` was answered at `/t/a-b`, where `svnserve` answers `/t/a/b` (A-592's finding). A-602's oracle let it pass:
  it accepted any lock that could refuse the commit anywhere, though only the order beneath one path read whole, a
  hash table's in FSFS, is free.
- **The decision.** `met` gives the locks in `verify_locks`' order: the changed paths in that path order, a modified
  path's own lock and every lock at or beneath a path added, deleted or replaced, a path beneath one so read skipped,
  each lock once; beneath a path read whole the same path order, the path's own lock first, as FSFS's walk sends it.
  `path_order` compares names one by one, which equals `svn_path_compare_paths` on canonical paths (A-592's oracle held
  the two equal over prefixes and bytes either side of `/`). The model of A-602's oracle returns at the first failing
  path, so the table must answer one of that path's locks.
- **Evidence** (the Mac Studio d1323898, kiln, 2026-11-19, debug, a fresh clone of the branch). Failing first (04:39
  UTC, load 8.7): with the tests and lock.rs before the fix, `cargo test --locked -p slates-svn --lib` failed 2 of 155:
  A-592's case answered `/t/a-b`, and A-602's oracle met a commit modifying `/a-b` and `/a/f` refused at `/a-b`, none
  of the first failing path's `{/a/f}`. Green (04:39 UTC): 155 of 155 in 1.14 s; A-602's census: 408 refused commits,
  38 of them decided by the order; A-592's census unchanged in kind. `cargo fmt` clean. Clippy, `xtask check` and the
  doc gate are main's (the run-request queue).
