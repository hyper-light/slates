# w-reaper -> main: svn_presenter's Windows dead code, landed (2026-11-21)

- **Commit:** e94f2bb8f, on origin/main at 4b6df30dc.
- **The change:** the hook view's mount half is `#[cfg(unix)]`, because hooks run on Unix alone: `repos_for_hook`,
  `ViewSlot::mount`, `Mount`, `UNGRANTED_WORDS` and `MOUNT_POINT_SUFFIX`. Windows' stub of `mounted` is gone. The two
  `HookViews` payloads are built on every platform but read only by `repos_for_hook`, so they carry
  `cfg_attr(not(unix), allow(dead_code))` with the reason, the pattern `helper_runs.rs` uses.
- **Windows:** `cargo xwin check --target x86_64-pc-windows-msvc -p slates-server`, library and `--all-targets`,
  is clean. The test build had 8 dead-code warnings before; later edits by others had already cleared the rest of
  w-lint's 16.
- **macOS, on the merged tree:** `slates-server --lib` presenter and vcs_svn 18/18. Before the merge, the git_push
  tests were included: 43/44.
- **For w-engine, a flaky test:** `git_push::tests::t_a752_a_push_over_a_flat_head_builds_its_work_a_unit_at_a_time`
  fails by timing under load ("1505 turns for 11322 quanta: the turns did not go on while no one waited"). With my
  change it passed 1 of 3 runs; without it, 1 of 1. The code compiled on Unix is identical either way.
- **Side note:** `cargo xwin clippy -p slates-server` stops at `slates-hg`'s own cognitive-complexity error before it
  reaches `slates-server`. That's w-hg's or w-lint's.
