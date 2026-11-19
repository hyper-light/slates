# A git hook given no push options was handed no `GIT_PUSH_OPTION_COUNT` (2026-11-19, found by A-665)

## Description

Found while recording what stock receive-pack hands its hooks (A-665, `crates/git/fixtures/view`, made by
`make-view-fixtures.sh` with stock git 2.50.1). In the two recorded pushes to a server that advertised no push options
(`receive.advertisePushOptions` off), `pre-receive` and `post-receive` both saw `GIT_PUSH_OPTION_COUNT=0`
(`hook.loose.*.txt`, `hook.pack.*.txt`). slates' `vcs_hooks::push_option_env(None)` removed the variable from the hook's
environment instead, following `githooks.adoc` ("If it is negotiated to not use the push options phase, the
environment variables will not be set").

## Root cause

The document and the code differ, and the code is what hooks see. `cmd_receive_pack` (`builtin/receive-pack.c`
2592-2620 at v2.50.1) keeps the push's options in a list it always hands `execute_commands` and `run_receive_hook`,
filled only when the options were negotiated; `run_and_feed_hook` (825-835) unsets the count only when that list is
missing, which it never is on this path, so the count is set, `0`, whenever no options came.

## Impact

A hook that tests `$GIT_PUSH_OPTION_COUNT` (git's own `pre-receive.sample` begins `if test -n
"$GIT_PUSH_OPTION_COUNT"`, line 9 at v2.50.1) took the other branch under slates than under stock git, and a hook run
with `set -u` that reads it failed under slates on every push without options. No hook was run by a server push path yet (`vcs_hooks` has no caller
outside tests), so no deployed push was affected.

## Edits

- `crates/server/src/vcs_hooks.rs`: `push_option_env` sets `GIT_PUSH_OPTION_COUNT` always, `0` when none were
  negotiated; `EnvValue::Unset`, whose only producer this was, removed; the module docs say why.
- `crates/server/src/hook_driver.rs`: its `env_remove` arm for `Unset` removed; the launch test hands
  `GIT_PUSH_OPTION_COUNT=0` and holds the hook to seeing `0`.
- Tests, red without the change: `gits_push_options_are_what_stock_receive_pack_hands_each_hook` (pre-receive, update
  and post-receive for the three recorded pushes, held to the recorded `GIT_PUSH_OPTION_*`), and
  `gits_hooks_get_what_receive_pack_gives_them`'s count without options now `0`.
- The sweep for the same difference in the other hooks' variables: see this piece's GAPS line.

## Runs

Windows x86_64, jupiter (kiln, debug, 2026-11-19, a fresh clone and target directory each; the Mac Studio offline):
red with `vcs_hooks.rs` and `hook_driver.rs` as at 967e9e4bd (the tests alone), `cargo test --locked -p slates-server
--lib -- vcs_hooks`: 31 passed, the two git tests above failed; green at 90fdf1708: 33 of 33, rustfmt's layout from
that clone. `hook_driver`'s launch test is Unix-only (`#[cfg(unix)]`) and is filed for main's macOS and Linux runs.
