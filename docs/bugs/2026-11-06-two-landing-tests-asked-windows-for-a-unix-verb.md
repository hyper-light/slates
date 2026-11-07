# Two landing tests asked Windows for a Unix verb (2026-11-06, §4.15, Phase 4 step 6)

## Description

On main `d61c3be2`, `cargo test -p slates-server --lib` fails two tests on Windows:
- `verbs::tests::a_landing_of_a_named_snapshot_presents_that_snapshot_whatever_the_head_became`;
- `verbs::tests::a_presentation_past_the_bound_is_refused_and_changes_nothing`.

Clients found them on both Windows CI runners. CI's Windows lanes do not run the server's unit tests, so main never
saw them.

## Measurement

jupiter (Windows 11 x86_64, 32 logical processors, 123.6 GB RAM, 131.4 GB commit limit), 2026-11-06, own worktree and
target directory, another project's test gate running throughout:

| Tree | Test threads | Runs | Result |
|---|---|---|---|
| main | 1 | 1 | 185 passed, these 2 failed, 2 ignored; 16 s |
| fixed | 1 | 6, 3 of them on the tree as committed | 186 passed, 0 failed, 2 ignored; 17 to 21 s where timed |
| fixed | 4 | 3 | 176 to 182 passed, 4 to 10 failed, 2 ignored |
| fixed | 32 (the default here) | 3 | aborted, no result: `memory allocation of 1746925056 bytes failed` |
| main | 32 | 1 | aborted the same way, exit `0xc0000409` |

Both tests fail with the same text: `Refused { refusal: Unsupported { feature: "landing" } }`.

## Root cause

`landing::land_verb` has a body on Unix only. Windows has no landing writer yet (the design's Phase 4 step 6; GAPS,
"Windows has no OS writer yet"), so the verb refuses `Unsupported` there, on purpose. Both tests expect
`GrantRequired`, and neither was gated when it was written (`c324c97a` and its follow-up), so they asked Windows for
what only Unix serves. The product's refusal is right; the tests were wrong. And no test ran the refusal on Windows:
`crates/server/tests/daemon.rs`, which does drive it, is `#![cfg(unix)]`.

## Impact

Test-only: no daemon behaviour changes. Landing stays unavailable on Windows, as the design already says.

## Edits

- `crates/server/src/verbs.rs`: both tests are `#[cfg(unix)]`. A new `#[cfg(not(unix))]` test,
  `a_landing_with_no_writer_on_this_platform_is_refused_and_changes_nothing`, presents a landing and expects
  `Unsupported` naming landing, no presentation in the status report and no new `landing_planned`, so gating the two
  loses no Windows coverage and the refusal is held for the first time.
- `docs/wip/GAPS.md`: the writer's entry names the refusal's test, and the oversubscription entry names this binary.

## Tests

- Red-check: with the refusal changed to name `not-landing`, the new test fails
  (`Refused { refusal: Unsupported { feature: "not-landing" } }`); restored, it passes.
- The two gated tests were red on main, above.
- Windows `cargo clippy -p slates-server --lib --tests --locked -- -D warnings` and `cargo fmt --check` are clean.
- Not run here: Linux and macOS. On them this change compiles out the new test and leaves the two tests as they were.

## The same binary with more than one test at a time (not fixed here)

Not caused by this bug, and left as it is: it is the oversubscription GAPS records for the client binary, now measured
on this binary too. Each in-process daemon sizes itself from the whole machine, and Windows charges commit when memory
is made.
- At 32 threads, free commit fell to 0.1 GB (sampled every 400 ms) and the process aborted on the same 1,746,925,056-byte
  reservation GAPS traces to the lease-expiry timer wheel.
- At 4 threads, a temporary log in `ContentPool::hand_out` (never committed) showed each failure paired with a refused
  map, 9 of 9, 4 of 4 and 6 of 6 over three runs: `VirtualAlloc(MEM_COMMIT)`, code 1455, `ERROR_COMMITMENT_LIMIT`. The
  sampled low of free commit, 10.7 to 17.8 GB, missed the moment of each refusal.
- jupiter's disk had about 9 GB free during these runs, which may have limited how far its automatic page file could
  grow; not measured separately.

So this binary needs `--test-threads=1` on Windows, as the client binary's lanes do, until GAPS's owed work is done.

## Sweep

- **A sibling, reported and not changed:** `crates/client/tests/presentation.rs`
  (`a_killed_clients_presented_landing_is_abandoned_with_it`) has no gate and fails on jupiter for the same reason:
  `Refused(Unsupported { feature: "landing" })` at line 60. CI's Windows lanes name their client test binaries, and
  this one is not among them, so it has not been seen there.
- The other test files that drive a landing and have no file-level gate: `crates/land/tests/durability.rs`, `lease.rs`,
  `oracle.rs` and `source.rs` name no real-host writer and were read, not run on Windows;
  `crates/server/tests/common/landing.rs` is a shared module, whose users were not each checked. All other such test
  files are `#![cfg(unix)]`.
- Reported, not changed: on Windows a refused map is reported as `BudgetExceeded { available: 44252790784 }` for a 1 MiB
  volume. `Store::admittable` counts the pool's free extents, and `ContentPool::hand_out` discards the OS's error, so
  the refusal names room the OS has just declined to give.
