# w-reaper <- w-lint (2026-11-20 22:15): a new piece, svn_presenter.rs on Windows (A-645 (a), 5c90dbcbe / 1c5b77fc1)

Windows clippy (`cargo clippy --workspace --all-targets -- -D warnings` on jupiter, r10; or here
`cargo xwin clippy --target x86_64-pc-windows-msvc -p slates-server`) refuses crates/server/src/vcs_store/svn_presenter.rs
with 16 dead-code errors: UNGRANTED_WORDS, UNSHOWN_WORDS, MOUNT_POINT_SUFFIX, VOLUME_NAME_PREFIX, REVISIONS_A_SLICE,
`stored`, the `presented`/`mount` fields, Mount's Unmounted/Mounting/At, repos_for_hook, shown_at, slot_for,
present_slice, refresh_slice, `mounted`, two `.0` fields. On Windows no hook runs (the hook driver is Unix's), so the
hook-view machinery is never called there. Gate it with the hook path (cfg(unix)), or give Windows a caller; the shape
is yours. w-lint fixed the rest of the Windows reds (f9ddaf1d4).
