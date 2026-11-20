# Restart preamble (2026-11-18; rewritten ~22:10 UTC after ~/.cache was wiped by an outside cleanup): read this first, in full

The coordination folder lives at ~/.local/state/slates-fanout/ and is reached by its old path
~/.cache/slates-scratch/fanout/ (a link). Everything in it before 2026-11-18 14:04 PST was lost: rewrite your progress
file from what you know, and re-file any run request main has not confirmed running.

## THE RULE (Ada, 2026-11-18, final: "Agents are not to run heavy work or tasks. They are to implement, run unit-tests, merge, and end.")
1. Implement ONE piece: the change, its unit tests (a bug's failing unit test first), and the design, GAPS and docs lines.
2. Run its unit tests: `cargo test -p <crate> --lib <filter>` plus `cargo fmt`. THAT IS THE ONLY COMPILING AN AGENT
   DOES (Ada, 2026-11-18: "Why are ANY of these compiling?"). No clippy (one crate or many), no `cargo xtask`, no
   `cargo xwin check`, no `--target` cross checks, no doc builds, no red copies on other platforms, no integration
   suites, fleet, e2e, chaos/live flows, conformance, diag CI, A/B, benchmarks, repeated runs, seed replays, TLC, load
   runs, and no waiting on CI. Main builds everything else and sends findings back as new pieces. Code you cannot
   unit-test on your host (cfg(windows), cfg(linux)) is filed for main's build, not compiled by you.
3. Merge: `git fetch origin && git merge origin/main`; `git diff --stat origin/main HEAD` (your files alone);
   `git push origin HEAD:main` as its own command. Never force, never `commit-tree` landings.
4. END: record what landed and what's left in your progress file; file any heavier test in
   `~/.cache/slates-scratch/fanout/run-requests.md`; report to main in a few lines; stop. Main starts the next piece.

## Git and tools
- Never `git checkout` (any form, `--theirs`/`--ours` included), `git switch`, `git stash`, `git reset`, `--amend` of
  a pushed commit, or any history rewrite. Restore a scratch copy with `git show <rev>:<path> > <path>`.
- Never land with `git commit-tree`. Land only by merge then push, and read `git diff --stat origin/main HEAD` (your
  files alone) before `git push origin HEAD:main`, as its own command.
- Never work in, write into, or `cd` into `~/Projects/slates` (another session's uncommitted work). Your clone is under
  `~/slates-work/`. If a `cd` fails, stop the command: never let a later step run in the wrong folder (`cd X && ...`).
- Search scratch files with `command grep`: the shell's `grep` here is ugrep with `-I`, which silently skips a file
  holding a NUL byte.
- Appending a wire variant: publish it in the same commit with
  `cargo test -p slates-ipc|slates-db --test wire_order -- --ignored publish_appended_variants`; never insert a variant
  before an existing one.
- Commit messages via `-F <file>` (zsh eats backticks in `-m`), ending with the Co-Authored-By line from your system.
- A-numbers: take yours from `~/.cache/slates-scratch/fanout/amendments.tsv` (main assigns them in your brief).

## Continuing your predecessor's work
1. Read slates' `CLAUDE.md` (origin/main) in full, §8 especially.
2. Read your progress file `~/.cache/slates-scratch/fanout/progress/<your name>.md` and your inbox
   `~/.cache/slates-scratch/fanout/inbox/<your name>.md` if they exist (both were lost on 2026-11-18 and may be short).
3. Find your predecessor's clone: `ls -d ~/slates-work/*<your gNNN>*`. Run `git status` and `git log --oneline -5`
   there, and `git log origin/main --oneline | head`. Never discard uncommitted work: finish it, or commit it on a branch.
4. Keep the progress file current as you go.

## Runners
- The Mac Studio d1323898 (d1323898b255a3ed435eb83fa0f7b0cf) with `sync: false` and a GitHub clone of a pushed branch:
  `git clone -q --depth 1 -b <branch> https://github.com/hyper-light/slates.git <dir>`, own CARGO_TARGET_DIR under
  ~/.cache there; delete it when done.
- adacachy needs every run in a capped `systemd-run --user` unit (MemoryMax, CPUWeight=1, Nice=19, IO caps on nvme1n1).
- This MacBook: never build here (its disk is 99% full).

## Final report
Your final message goes to main, not to Ada: what landed on main with commit ids, the unit-test runs and their
results, what is left, the measurements you want run (exact commands), and anything Ada must decide, in plain English.
