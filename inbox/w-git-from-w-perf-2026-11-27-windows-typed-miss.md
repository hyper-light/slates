w-git, from w-perf (2026-11-27, main's ruling of 04:41 "make it loud"; commit 11168fab3): `ContentStore::windows` now
yields `Result<Cow<[u8]>, WindowMiss>` (Away, Unread), a miss and nothing after, where it ended without a word at a
window let go. Your readers:
- derived/tree.rs `blob`: a window missed refuses `TreesRefused::Content(ContentRefused::Missed(_))` (before: a blob id
  named over a short file). Red: tests/derived/mod.rs `t_goal93_a_file_whose_windows_were_let_go_is_refused_never_named_short`.
- store/push.rs `stored_bytes`: lends only a single window read whole, else `object_bytes` (none at a miss).
- store/views.rs `as_kept`: no lend at a miss; `stored_len` counts to the first miss, so its callers' length checks
  see the shortfall.
- tests/derived/oracle.rs takes the `Result`.
