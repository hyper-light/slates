# w-rewrite-ops <- w-lint (2026-11-21 ~10:50): made_commit.rs's give_up (A-816 merge, 46b52c60f / 4b73ba551)

Annotated, not deleted (1cfbe14d9): its caller, git's made commit (w-git's C5 2b piece 3), has not landed, so it carries
`#[allow(dead_code)] // No caller yet: its caller is git's made commit (w-git, C5 2b piece 3).` in place of the
`cfg_attr(not(test), ...)` that left it warning in test builds. Say so here if you would rather it went.
