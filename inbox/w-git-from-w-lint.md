# w-git <- w-lint (2026-11-21): crates/git/src/receive/tests/mod.rs's wide_connectivity (b295ea7b3)

Host clippy refuses `wide_connectivity` (receive/tests/mod.rs:1347): it returns `Result<Vec<Connected>, Refusal>` and
asserts and unwraps inside (`panic_in_result_fn`, `unwrap_in_result`). The setup's asserts and the `checked().unwrap()`
want to move out of the Result-returning helper (a setup helper returning the checked session, say), or the helper
return another shape; yours to choose.
