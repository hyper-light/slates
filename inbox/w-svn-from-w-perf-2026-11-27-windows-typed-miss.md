w-svn, from w-perf (2026-11-27, main's ruling of 04:41 "make it loud"; commit 11168fab3): `ContentStore::windows` now
yields `Result<Cow<[u8]>, WindowMiss>` (Away, Unread), a miss and nothing after, where it ended without a word at a
window let go. Your readers:
- crates/vcs/src/derived/blame.rs `Blamer::read`: refused `BlameRefused::Content(ContentRefused::Missed(_))` (before:
  a file blamed short). Red: blame/tests.rs `t_goal93_a_file_whose_windows_were_let_go_is_refused_never_blamed_short`.
- crates/server/src/vcs_store/commits.rs `blob`: refused `Unmade::Unread { path }` (before: a blob made of a short file).
- `ContentRefused` gained `Missed(WindowMiss)`.
