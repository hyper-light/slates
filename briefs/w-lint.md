# w-lint: main compiles and passes the lint wall on every target (main, 2026-11-19 ~22:45 PST)

## Why
About 24 changes an hour land on main under the fast loop. That loop skips clippy and cross-target runs: common.md
makes them main's job. Main's CI cancels each run when the next push lands, so the last run that finished is the
latest verdict: 44676412887 at 3691deb5c (2026-11-20 04:44Z). It failed in 15 jobs:
- Format, since fixed.
- The lint wall in slates-db and slates-vcs.
- The Windows build, fixed by main 36478561d: the anchor's `vcs_svn_listener` initializer lacked its cfg(unix).
- Conformance, TSan, Miri and SDK packaging.

You do main's lint and compile work. Keep main passing CI's compile and lint steps on every target, with fixes
landing in small pieces.

## Yours, and not yours
Yours: compile errors and lint-wall failures on every target CI lints:
- clippy with -D warnings, on the macOS host with `--workspace --all-targets`.
- The cross lints .github/workflows/ci.yml runs: x86_64 and aarch64 Linux (gnu and musl), and x86_64 and aarch64
  Windows msvc, with the crate sets and features ci.yml passes.
- `cargo xtask check` (structural, literals, unsafe budget).
Read the exact commands from ci.yml's steps, such as "Clippy the cross-buildable crates" and "Clippy the
Windows-buildable crates". Run what CI runs, nothing more.

Not yours: failing tests, conformance, TSan, Miri and SDK packaging. Report each to main in one line, with the test's
name and, if it's cheap to find, the first commit where it fails. Don't chase them.

## Known at 3691deb5c (lines will have moved; start from a fresh run on current main)
- slates-db:
  - an octal-looking escape at repository/policy.rs:285
  - cognitive complexity at repository.rs:940 (13/10), 1601 (26/10) and 2182 (12/10); policy.rs:379 and 443 (11/10);
    review.rs:601 (15/10) and 686 (12/10); repository/entry/tests.rs:2041 (11/10)
  - a very complex type at repository.rs:1811 and repository/entry/tests.rs:1123
  - a collapsible `if` at register_nemesis.rs:630
- slates-vcs, seen on Windows clippy:
  - cognitive complexity at derived/blame.rs:515 (12/10)
  - a collapsible `if` at wildmatch.rs:137
- slates-ipc `--test rendezvous` failed on Windows. That's a test: report it only.

## How to fix
- Make the smallest change that keeps behaviour:
  - Split a complex function into named helpers along its own steps (CLAUDE.md §3).
  - Collapse the `if`.
  - Give a complex type a `type` alias.
  - Write the escape unambiguously.
- Never weaken the lint wall: no `#[allow]` or `#[expect]`, no clippy.toml threshold change, no `[workspace.lints]`
  change. If a lint looks wrong, tell main instead.
- Test targets are code too (`--all-targets`).
- One landing per crate or lint family. Commit message:
  `lint: <crate>: <what> -- <the command>, <n> errors before, clean after`.
  Write it to a file and commit with -F, since zsh eats backticks.
- Before each landing:
  - Red: run the crate's clippy on the target that failed and see the error.
  - Green: run it again after the fix and see it clean.
  - `cargo fmt --all -- --check`.
  - If you split a function, run the crate's own unit tests (`cargo test --locked -p <crate> --lib`).
  Then land as common.md says: fetch, merge origin/main, read the diff stat, `git push origin HEAD:main` as its own
  command, and confirm with `git merge-base --is-ancestor`.
- If one agent's landings keep bringing a lint back, tell main in one line, naming the commit. Main tells the owner.

## Rounds
1. Clean main on every target.
2. Then once an hour: fetch origin/main, fast-forward your worktree, run the same commands, fix and land.
3. Write one kiln summary line a round: `round N at <sha>: clean`, or `round N at <sha>: fixed <n> in <crates>
   (<commits>)`. Add any red test you saw, in one line.

## Setup and rules
- Use this Mac Studio. Keep one persistent clone at ~/slates-work/w-lint: clone it once from
  https://github.com/hyper-light/slates.git, then run
  `git remote set-url --push origin git@github.com:hyper-light/slates.git` (HTTPS has no credentials here; SSH works).
- Use your own target dir, ~/.cache/w-lint-target, with `--target` per platform and CARGO_BUILD_JOBS=8.
- Keep 20 GB of disk free: check `df -h ~` before each round, and delete only your own target dir.
- If a target's rustup std is missing, or a crate's C build script (zstd-sys, aws-lc-sys) can't cross-build here, skip
  that target loudly in your summary. Don't install toolchains.
- Windows cross clippy works from macOS for the library set with
  `--no-default-features --features slates-machine/pure-hash`, as ci.yml's cross-lint lane does.
- Read ~/fanout-state/briefs/common.md in full and the repository's CLAUDE.md. You're a full agent and can compact,
  so never end for context.
- Never touch ~/Projects/slates, never write /tmp, never force or force-push, and never checkout, switch, stash,
  reset or commit-tree.
- Never print, log or copy any API key or credential.
- After each landing, append to ~/fanout-state/progress/w-lint.md, commit it, and push it to agents/fanout-state.
- You can't SendMessage main across machines. Main reads your kiln summary.
