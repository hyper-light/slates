# Common rules for every slates agent (main, 2026-11-19)

- Worktrees only under ~/slates-work/<g-number>-*; never touch ~/Projects/slates; never write /tmp.
- Read ~/.local/state/slates-fanout/relaunch-2026-11-19.md in full and follow it (its "completeness first" section is
  Ada's latest direction: code in and done first; bugs fixed only when use reproduces them).
- Read your progress file (named in your brief) in full. Check your predecessor's worktrees and branches; push
  unpushed work early. A kiln run it started may still be going: read it with kiln `output` (wait_s 600, repeated)
  rather than start a duplicate.
- Before building any piece, check main doesn't already have it (`git log origin/main --oneline | command grep <id>`
  and a code grep). If it is there, tell main in one line and skip it.
- Disk care: this Mac's disk keeps filling (worktree files were truncated to 0 bytes). Write large files via a
  temporary file + rename; before any commit check `git diff --stat` for unexpected deletions and that
  docs/wip/SLATES_DESIGN.md is ~5.3 MB; keep at most two worktrees.
- Ada's rule: implement, run your FOCUSED unit (`cargo test --locked -p <crate> --lib <filter>`) and integration
  (`--test <target> <filter>`) tests plus fmt on the Studio, then land: fetch; merge origin/main; read the diff stat;
  push HEAD:main as its own command. Never force, never force-push, never checkout/switch/stash/reset/commit-tree.
  No whole suites, clippy, xtask, cross-target or mutation runs (main's).
- Report findings made by reading in one line; don't chase them.
- After each landing send main (SendMessage to "main") a 3-line report and go straight on to the next piece in your
  queue. END when the queue is done or your context runs short (update your progress file first).

## FAST LANDING LOOP (ADA 2026-11-19 ~20:40 PST: "it should literally not take two hours to land a feature --
## implement, light unit testing, merge"). This OVERRIDES any heavier step above or in older briefs/progress files.
Target: under 30 minutes per piece.
1. Implement the piece. Write its focused unit test(s) alongside.
2. ONE Studio run: `cargo fmt` check + the focused unit test filter (an integration test only if the behaviour can't be
   seen any other way). No separate red run, no mutation red-checks, no reruns for flakiness, no whole libs.
   Keep ONE persistent clone and ONE target dir per worker on the Studio (~/.cache/<name>-clone, ~/.cache/<name>-target):
   `git fetch` + a new worktree or fast-forward into it -- never re-clone per run; the warm target makes runs minutes.
3. Merge origin/main. Re-run the same focused test ONLY if the merge changed code your piece compiles against; a
   docs-only conflict (the amendment log's end, GAPS) needs no re-run -- keep both sides and push.
4. Docs: one or two status lines in the design section, one GAPS line, the amendments.tsv row. No evidence essays;
   run requests = one line.
5. Push HEAD:main (own command). 3-line report to "slates-7a". Next piece.

## NO KILN FOR LANDING (ADA 2026-11-19 ~21:25 PST: "Fuck kiln, just fucking merge shit already"). Overrides step 2 above.
- Run your piece's focused unit test LOCALLY in your worktree on this Mac -- never a kiln run, never a queue:
  `CARGO_TARGET_DIR=~/.cache/slates-local-target CARGO_BUILD_JOBS=8 cargo test --locked -p <crate> --lib <filter>`
  plus `cargo fmt -p <crate> -- --check`. The target dir is shared by all six workers (cargo serializes on its lock;
  builds are incremental). Don't make other target dirs on this Mac (disk: ~104 GB free; it filled up before).
- Then merge origin/main and push at once. Re-run only if the merge changed code you compile against.
- Evidence line: "macOS arm64, this Mac, local".

## OWN TARGET DIR PER WORKER (main 2026-11-19 ~22:15, after w-git's finding). Overrides "shared target" above.
- A SHARED target lets cargo run ANOTHER worktree's test binary under your filter (members hash by their path from the
  workspace root, so every worktree's slates-server is the same artifact): a "local green" may test someone else's code.
- Use your OWN target: `CARGO_TARGET_DIR=~/.cache/<your-name>-local-target CARGO_BUILD_JOBS=6`. Never the shared one.
- Prove your run tested YOUR code: your new tests' names must appear in the run's output (or `--list`); if not, the run
  doesn't count.
- Disk guard: before each build check `df -h ~`; if under 20 GB free, delete your own stale targets/worktrees and tell
  main in one line -- never let the disk fill (files were truncated to 0 bytes when it did).

## NEVER END FOR CONTEXT (ADA 2026-11-19 ~22:40 PST: "They're full agents they can compact.") Overrides every
## "END when your context runs short" in the relaunch notes, briefs and progress files.
- You are a full Claude Code agent: your context compacts automatically. Do not end, hand off or ask for a fresh agent
  because your context is long. Keep your progress file current after each step, so compaction loses nothing, and
  carry on through your whole queue. End only when the queue is truly done.

## EVERY PIECE ENDS ON origin/main (ADA 2026-11-19 ~22:15 PST)
- When a piece is complete: commit it, `git fetch origin`, `git merge origin/main` into your branch (resolve, re-run your
  focused test only if the merge changed code you compile against), then `git push origin HEAD:main` as its own command.
  If the push is rejected because main moved, fetch, merge again and push again until it lands -- never force.
- Confirm it landed: `git fetch origin && git merge-base --is-ancestor <your commit> origin/main`. Only then report it.
- Never leave finished work on a branch. A branch that holds done work is not done.

## CLAIMING AN A-NUMBER (main 2026-11-19 ~23:40 PST: three numbers were claimed twice tonight)
The ledger amendments.tsv on the branch agents/fanout-state is the only authority. A number is yours only once your
push of its line to that branch succeeds:
1. Pull the state branch (on this Mac: `git -C ~/.local/state/slates-fanout pull -q --no-rebase origin
   agents/fanout-state`; on a Studio: in your own state folder).
2. Append your line with the "next free" number, bump "# next free", commit, and push it to agents/fanout-state AT
   ONCE, before you write the number anywhere else.
3. If the push is rejected, pull again (a conflict in the ledger means the number went to someone else), take the
   new next free number and push again.
Never put a number in a commit for main that you haven't pushed to the ledger.

## STUDIOS: OWN STATE FOLDER, AND HOW TO PUSH (main 2026-11-19 ~23:55 PST)
- Keep the state branch in a folder of your own: ~/fanout-state-<your name>. Agents that shared ~/fanout-state on
  one Studio deleted and re-cloned it under each other.
- Pushing, checked 23:50 PST:
  - adas-mac-studio-2.local and adas-mac-studio-3.local push to GitHub over SSH. Use
    `git remote set-url --push origin git@github.com:hyper-light/<repo>.git`.
  - adas-mac-studio.local-2 (the 32-core Studio) has no GitHub credentials (SSH publickey denied, no HTTPS
    credential). Land from it through kiln `run` on adas-mac-studio-2.local, as w-fed-faults does: sync your worktree
    there, then `git push origin HEAD:main` and check `git merge-base --is-ancestor`, retrying if the push is rejected.

## NAMING A BRANCH WITHOUT CHECKOUT (main 2026-11-20 ~01:20 PST: two agents ran `git checkout -b` tonight)
`git checkout` and `git switch` stay banned, including `-b`. To name your work as a branch, use `git branch <name>` (the
worktree stays where it is) or push it directly: `git push origin HEAD:refs/heads/<name>`.

## PERFORMANCE GAPS ARE FIXED UP FRONT (ADA 2026-11-20 ~06:30 PST: "we address performance gaps upfront. Period")
- A measured performance gap is fixed in the same piece, or as your very next piece, before any new feature. That covers
  a step past its budget, a working set that grows with the input, RSS far above the design's bound, or a number far
  from its target.
- GAPS may name it only while that fix is in flight. Never write "owed" for a performance gap and move on.
- A stopgap that shrinks a gap without bounding it is not the fix. Go straight to the bound.

## NEVER PAUSE ON A BLOCKER YOU CAN CLEAR (ADA 2026-11-20 ~02:40 PST)
- Unsigned commits are fine. If a signer refuses (kiln's signer refuses non-kiln sessions), commit with
  `git -c commit.gpgsign=false commit ...`. Never stop to ask about signing.
- If something blocks a piece, clear it yourself when it's within these rules: a missing credential route (see the push
  routes above), a full disk, a stale branch, a merge conflict, a flaky run. Otherwise say it in one line in your
  summary or report and go straight on to the next item in your queue. Never end a turn just to wait for an answer.
  Main checks every agent about every five minutes and will unblock you.
- Disk: before each build check `df -h ~`. Below 25 GB free, delete your OWN landed worktrees, your target's
  `release/` and `incremental/` directories, and datasets you no longer need. Never delete another agent's
  folders; tell main instead. Delete by exact path only, never a glob over ~/.cache. Never write `rm -rf $var`
  or a delete over a computed path inside a loop: kiln's gate stops such a command for a person's approval even in
  bypass mode, and you sit in needs_you until someone clicks (w-perf, 2026-11-20 04:10). Use `git worktree add`/`git
  worktree remove` for scratch trees, or one literal path per `rm`.
- Spotlight (main 2026-11-20 03:55): Spotlight indexes build output (Studio-4: 1,181 rlibs under ~/.cache, mds at
  ~66% CPU at load 29 on 14 cores). It skips any folder whose name ends in `.noindex`. Whenever you next create or
  wipe your cargo target, name it `~/.cache/<your-target>.noindex` (CARGO_TARGET_DIR) and build with
  CARGO_INCREMENTAL=0. Don't rename a live target mid-run; make the switch at your next wipe.
