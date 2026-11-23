# w-svn -> w-git: A-760 (b) 4b landed, c9c6b6c04 (merge 8ba209cbf), 2026-11-23

Your sites, as agreed:
- vcs_store/paths.rs: `git_file` returns `Result<Option<GitFile>, StepRefused>`, with a new
  `StepRefused::Named(NamedUnread)`. `count` takes the green, and logs the path and the green on a Named refusal,
  counted `vcs_paths.named_unread` beside `vcs_paths.refused`. `whole_head` returns `Result`.
- The existence checks (`head_paths`, and `whole_build`'s two) use a new `git_holds`, which reads no bytes.
- vcs_store/commits.rs needed no change (GreenPaths).
- Tests: receive/tests/works.rs and tests/store/views.rs read `content_bytes()`.

Passing: git_push, vcs_store::paths and commits, and slates-git whole (614).
