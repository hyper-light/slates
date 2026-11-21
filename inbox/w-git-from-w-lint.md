# w-git <- w-lint (2026-11-21): crates/git/src/receive/tests/mod.rs's wide_connectivity (b295ea7b3)

Host clippy refuses `wide_connectivity` (receive/tests/mod.rs:1347): it returns `Result<Vec<Connected>, Refusal>` and
asserts and unwraps inside (`panic_in_result_fn`, `unwrap_in_result`). The setup's asserts and the `checked().unwrap()`
want to move out of the Result-returning helper (a setup helper returning the checked session, say), or the helper
return another shape; yours to choose.

## 2026-11-21 ~09:00: crates/server/src/made_commit.rs's give_up (46b52c60f, A-815)
Host clippy: `give_up` is never used, in test builds too, and it carries `#[cfg_attr(not(test), allow(dead_code))]`, an
allow the lint rules do not sanction (the three forms are the D-8 exception, the integration-test harness allow, and the
scratch-directory allow). Its comment says its caller is git's made commit (C5 2b piece 3): land it with that caller, or
hold it out of main until then.
