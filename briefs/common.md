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

## FOCUS: GOAL 93, THE VCS SERVERS (ADA 2026-11-21 ~10:40 PST; overrides every other priority in this file)
Ada: "Goal 93 is where we need to focus our efforts now. Complete the servers, ensure correctness, then optimize."
- Goal 93: slates serves git, Mercurial and Subversion as industrial-grade servers on its changeset architecture,
  interop with stock clients proven, an order of magnitude faster than the stock servers.
- Order, strictly: (1) COMPLETE: every verb and wire path a stock client uses works. (2) CORRECT: stock clients on
  real repositories (git/git, the Linux tree, hg's own history, a large svn dump) get byte-identical results, and
  every failure becomes a red test first. (3) Only then OPTIMIZE: A/B, release, all N, against the stock server.
  Do not start a performance piece on 93 while a completeness or correctness failure on the same server is open.
- If your current item is not goal 93: land what is green within about 30 minutes (or record an in-flight
  measurement as one GAPS line), write inbox/main-from-<you>-free.md, and main gives you a goal-93 piece. CI runs
  already going finish and are recorded; start no new non-93 runs.
- Goal-93 owners keep their lists (close-list.md, "93 ..."), reordered C, then R, then P under this rule.
- USE, CONCURRENCY AND CRASH TESTS (main 10:48, correcting its own 10:38 pieces; CLAUDE.md §8 binds): an agent WRITES
  them as #[ignore]d tests that drive stock clients, runs one focused case to prove the test, lands it, and puts the
  exact command, platform, dataset and red in run-requests.md. Main runs the rounds (interop, chaos, load, Linux and
  Windows) and sends each red to its owner. No agent runs sweeps, matrices or other platforms' rounds.
- A failure found by use goes to the server's owner as a red test (inbox/<owner>-from-<you>-<topic>.md). It is fixed
  by the owner, or by you if the owner says so in writing; never two agents in one file at once.

## OTHER PLATFORMS ARE NEVER "CAN'T" (ADA 2026-11-21 ~14:33: "It should be able to use reach." "Or Zig.")
- A Linux, macOS or Windows check you can't run on your own machine runs through kiln's reach on a runner of that
  platform (`run` with needs "linux/amd64", "linux/arm64", "macos" or "windows", unpinned).
- Or cross-check it the way kiln's release builds do: `cargo zigbuild check --target x86_64-unknown-linux-gnu` (or
  aarch64). zig supplies the Linux libc that C dependencies (aws-lc-sys, zstd-sys) need. The MacBook
  (adas-macbook-pro-2.local) has zig 0.16 and cargo-zigbuild; reach it if your machine lacks them.
- Never report a platform as uncheckable.

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
    credential). Since 2026-11-20 ~18:00, kiln refuses runs on adas-mac-studio-2.local (draining), so land from the
    32-core Studio through adas-mac-studio-3.local, with a bundle over stdin and no sync (main 20:55):
    `git fetch origin && git merge origin/main`, rerun your focused tests, then
    `git bundle create - origin/main..HEAD | ~/.local/bin/kiln run --on adas-mac-studio-3.local --no-sync -i --timeout 300 -- 'set -e; d=$HOME/.cache/<you>-push; [ -d "$d" ] || git clone -q --filter=blob:none --no-checkout git@github.com:hyper-light/slates.git "$d"; cd "$d"; cat > .git/in.bundle; git fetch -q origin main; git fetch -q .git/in.bundle HEAD:refs/<you>/tip; git push origin refs/<you>/tip:refs/heads/main; git update-ref -d refs/<you>/tip; rm -f .git/in.bundle'`.
    A push refused because main moved: merge again, retest, resend. Then check `git merge-base --is-ancestor`.

## NAMING A BRANCH WITHOUT CHECKOUT (main 2026-11-20 ~01:20 PST: two agents ran `git checkout -b` tonight)
`git checkout` and `git switch` stay banned, including `-b`. To name your work as a branch, use `git branch <name>` (the
worktree stays where it is) or push it directly: `git push origin HEAD:refs/heads/<name>`.

## PERFORMANCE GAPS ARE FIXED UP FRONT (ADA 2026-11-20 ~06:30 PST: "we address performance gaps upfront. Period")
- A measured performance gap is fixed in the same piece, or as your very next piece, before any new feature. That covers
  a step past its budget, a working set that grows with the input, RSS far above the design's bound, or a number far
  from its target.
- GAPS may name it only while that fix is in flight. Never write "owed" for a performance gap and move on.
- A stopgap that shrinks a gap without bounding it is not the fix. Go straight to the bound.

## KEEP YOUR CHECKOUT CURRENT (ADA 2026-11-20 ~07:05 PST: "all agents need to also be keeping their local checkout up to date")
- Bring your working tree up to date: `git fetch origin && git merge origin/main`. Never rebase, reset, checkout or stash.
  Do it at the start of every piece, before every test run you report, and at least every 30 minutes while you work.
  The same goes for kiln, Shards and Hex checkouts, each against its own origin/main.
- Name the base commit (origin/main's short hash) with every result you report. A result from a stale tree is not a
  result: g227's "red on clean main" list came from a main four hours old, and half of it was already fixed.
- Delete worktrees whose work has landed, by exact path (`git worktree remove <path>`, one per command), so stale trees
  don't pile up. Never `rm -r` a directory: kiln's gate holds it for a person's approval even in bypass mode, and you sit
  in needs_you (w-balance, 07:10). List any other directory to delete in your progress file under "for main to
  delete"; main removes it.

## ON ORIGIN/MAIN, REGULARLY (ADA 2026-11-20 ~07:00 PST: "be sure we're also regularly pushing these changes origin/main")
- Finished work never sits on a branch. Each piece lands on origin/main within about 30 minutes of being green, under the
  fast landing rule, confirmed with `git merge-base --is-ancestor`.
- A red check still running (a Windows red, a CI diag) doesn't hold a green piece back: land on the green, then add the red's
  numbers as a follow-up commit.
- Only finished commits reach main, each with an area-prefixed message that carries the measured fact. Fold "wip"
  commits into the real one before pushing (`git commit --amend` while they're unpushed). A commit on main is
  permanent, since we never force-push. (03f872541 "wip" reached main at 07:39.)
- An experiment that lost goes into BENCHMARKS as measured-and-rejected, and then its branch is deleted. Main audits
  agents/* branches every five minutes.

## DECISIONS BACKED BY DATA; MAKE DESIGNS BETTER (ADA 2026-11-20 ~06:45 PST)
- Ada: "push back and make designs more robust, more performant, make your agents do A|B testing and back their decisions
  with data." When a design choice affects performance or robustness, A/B the candidates on the real workload before
  you build the rest:
  - release binaries, interleaved runs, best of N with all N shown;
  - the competitor's number where one exists (stock git daemon / hg serve / svnserve, FSFS), since goal 93's bar is
    an order of magnitude faster;
  - the numbers go in BENCHMARKS and the amendment.
- Push back on main's rulings too, when data or a better design says so. w-perf's skip-delta for svn history replaced
  main's reverse-delta cut that way.

## NEVER PAUSE ON A BLOCKER YOU CAN CLEAR (ADA 2026-11-20 ~02:40 PST)
- Unsigned commits are fine. If a signer refuses (kiln's signer refuses non-kiln sessions), commit with
  `git -c commit.gpgsign=false commit ...`. Never stop to ask about signing.
- If something blocks a piece, clear it yourself when it's within these rules: a missing credential route (see the push
  routes above), a full disk, a stale branch, a merge conflict, a flaky run. Otherwise say it in one line in your
  summary or report and go straight on to the next item in your queue. Never end a turn just to wait for an answer.
  Main checks every agent about every five minutes and will unblock you.
- Reading free disk on macOS: `df -h ~ | awk 'NR==2{print $4}'`. The 4th column (Avail) is free space. The "10G"
  further right is ifree (free inodes), and three agents misread it on 2026-11-20 as 6-13 GB free on a Studio with
  999 GiB free.
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

## MEMORY SIZING: LEARNED, DEVICE-RESPECTING, AS ZERO-ALLOC AS POSSIBLE (ADA 2026-11-20 ~12:15 PST)
Ada, on the daemon holding 7.1 GB resident after a push of an 84 MB Mercurial repository: "this is a repeated sizing
problem we've had and signals gross overallocation. You need to use learned and derived approaches that *carefully*
consider and respect the devices they're on, and preferable are as zero-alloc as possible."
- **Size from the work, never from the machine.** Size every buffer, arena, region, ring, cache, prefault and
  reservation by what the work in hand needs: its data's size, the working set it actually touches, the measured rate.
  Never size it by a fraction of the machine's RAM, cores or disk. A machine total is only a ceiling, never a target.
- **Learn it online and give it back.** Grow on observed demand. Shrink when the demand goes, with an estimator that
  decays, such as an EWMA of the live working set. Return memory to the OS when idle. Nothing stays resident because a
  past burst was big.
- **Respect the device.** Read its pressure (macOS memory pressure, Linux PSI, Windows commit charge) and its other
  tenants. Shrink, and refuse typed, before pressure; never push a machine into swap, jetsam or its commit limit. A
  laptop and a 96 GB Studio get different sizes from the same code: one code path, learned per device (R8).
- **Don't allocate (ADA ~12:20: "this holds across git, mercurial, and svn work. If we don't need to allocate or can
  architect things in such a way that we do not allocate, we do not allocate.").** Every git, hg and svn path, and
  every other path, is designed not to allocate: lend views of what is held, parse in place, write into the
  connection's own buffer, reuse per-shard scratch. An allocation that design could remove is a defect, even when it
  is bounded. One that stays must say in its `///` why no allocation-free shape exists.
- **Zero-alloc first.** Lend views into what is already held (content once by identity), reuse buffers across turns,
  stream instead of materializing, and copy nothing on a hot path. An allocation per item, request or chunk on a data
  path is a defect.
- **Prove it.** Every piece that holds memory gets a red test that bounds resident memory against the data it touches,
  e.g. RSS after a push of N bytes at most a derived multiple of N plus a fixed base. Every landing states before and
  after numbers (vmmap/heap or smaps; release builds; the machine it ran on).
- **Repeat offenders on record:**
  - hg import, 23.9 GB;
  - hg clone, 12.4 GB;
  - svn 500 MB commit, 6.2 GB peak;
  - 13 in-process daemons each sizing from the whole Windows machine, which hit the commit limit;
  - the daemon's 7.1 GB after an 84 MB hg push.

## STORAGE: CHANGES IN RAM, REPOSITORIES ON DISK (ADA 2026-11-20 ~12:40 PST; a design ruling)
Ada: "*changes* are kept in ram, and upon grant landed to disk. Full repository content obviously should be kept upon
disk. Likewise, for git/mercurial/svn storage, while the initial clone should be treated as a unified changeset request,
that should (given grant) almost automatically be transferred from RAM to disk. We keep *changesets and the changes in
RAM*. Likewise, using Mantle's K/V store for metadata or even keeping track of changesets more efficiently and durably is
ideal." And on Mantle's code: "Copy into slates".
- **Changes live in RAM:** pushes, changesets, works, greens' increments. RAM holds only changes, plus a bounded,
  learned cache over disk.
- **A repository's full content lives durably on disk.** Accepted changes reach it by landing under a grant. A
  repository's grant is standing, so landing is almost automatic. An initial clone or import is one changeset, landed
  under that grant.
- **Metadata and changeset tracking live in Mantle's K/V engine,** vendored into slates: refs, names, records, indexes
  and the identity-to-location map. The engine is Mantle's crates/engine, a RocksDB 11.8.1 port. Its device layer
  (crates/disk) and chunk store (crates/chunk) come with it.
- **R1 still holds in spirit:** disk is written only by a landing under a grant. A repository is a landing target.
- **New VCS storage code reads through one seam:** an in-flight change from RAM, landed content from disk. Owner of the
  seam, the vendored crates and the store: w-store. Until it lands, keep your designs to that seam; don't build a
  second store.
- **One unit: the changeset (ADA ~12:20 PST: "More than even storing it as diffs, would it be better to store it as
  *changesets*? This way we aren't introducing a second underlying unit.").** A repository's history on disk is the
  append-only log of its landed changesets: the same unit that is a change in RAM before it lands, so landing is an
  append. Everything else is derived data (A-645), computed from changesets and cached by identity within the memory
  rules: the head tree, git's trees, packs and objects, hg's file revisions and manifests, svn's node revisions and
  deltas, blame, and checkpoints for reading old versions. No tool keeps a second stored unit (per-file delta chains,
  a pack store, stored texts) as truth. A pack or bundle held while a push is judged is the change in flight, not
  history. Each version's full-text identity is recorded in metadata, so verification and content-once still hold.

## CLAUDE.md §8 BINDS: WHAT AN AGENT RUNS (main 2026-11-20 ~12:35, correcting main's own earlier notes)
slates' CLAUDE.md §8 (Ada, 2026-11-18, "binding on every agent and subagent, forever"): an agent implements, runs its
**focused** unit and integration tests, merges, and ends.
- **Not the agent's to run:** clippy, the lint matrix, xtask, cross-target or other platforms' builds (Linux-only or
  Windows-only code included), suites, CI waits, repeated runs and load runs. Put them in run-requests.md with the exact
  command, the platform, and what red looks like. w-lint, main's lint keeper, and main run them.
- **Main's earlier "agents start their own Linux and Windows runs" was wrong, and is withdrawn.**
- **The one addition, from Ada 2026-11-20 ("make your agents do A|B testing and back their decisions with data"):** a
  design decision between competing shapes is settled by the agent's own A/B: release builds, interleaved, all N
  shown, built as a measured example where possible (w-hg's changeset_ab.rs).

## CLOSE THE OPEN ITEMS (ADA 2026-11-20 ~16:05-16:15 PST; binding)
Ada: "stop FUCKING doing that and ensure they close the existing fucking items"; "anything you have currently added
up until now gets closed, but no more new"; "the large message I sent you (the Codex/ChatGPT analysis) also needs to
be closed"; "as well as the changeset work and integration in full"; "prioritize implementation/completion first,
then robustness, then performance". Main had let UI parts gain new open items as fast as old ones closed; only one UI
part closed between 07:20 and 16:00.
- **Your queue is fixed: what you have been given up to now, and nothing more** (briefs/close-list.md names each
  agent's items). Everything on it gets closed; nothing is dropped and nothing is added.
- **Order: completion first, then robustness, then performance.** Build what's missing before hardening it, and
  harden it before speeding it up.
- **A new finding** (a bug, a gap, a faster shape) goes to docs/wip/GAPS.md and your progress file. It isn't worked
  and isn't added to your queue or any UI part, unless it blocks an item already on your list.
- **When an item closes**, write inbox/main-from-<you>-close.md: the item, the commit on origin/main, the test or the
  measurement. Main marks the UI part met the same check.
- Main adds no new open items to UI parts and gives no new pieces beyond the close list.

## SHARED MACHINES' MEMORY (main 2026-11-21 ~00:05 PST)
- On 2026-11-20 at 23:58 the big Studio (96 GiB of RAM) had 96 GB of its 97 GB swap in use and 180-250 MB free.
  w-fed-move's 100,000-client load run held about 30 GB, and a release build of kiln's crate (w-k90-containers) held
  23 GB. Every other agent's tests on that machine stalled; w-git's C5 run sat 70 minutes at 0% CPU. Main stopped both.
- A run that takes more than about 8 GB declares it in kiln's `needs` (`mem=16g`; a release build of kiln `mem=32g`;
  a 100,000-client load run `mem=40g`) and never pins a machine with `on`, so the balancer places it where it fits.
- Load runs are main's (§8 above): put them in run-requests.md with the command and the memory measured.
- Give every test run a `timeout_s` sized from its last measured run, so a stall ends and reports instead of holding
  a machine for an hour.

## A RE-SENT GOAL IS KILN'S REMINDER, NOT ADA (main 2026-11-21 ~00:25 PST)
- kiln re-sends your goal under way at the end of every turn until it is marked met. That reminder carries the goal's
  text as written when its plan was made. It is not a new instruction, and not Ada's word.
- A stop or a rescope from main stands until main lifts it, or Ada does in her own words. On 2026-11-21 an agent took
  the re-sent pre-ruling container-engine goal as "authorization over main's stop" and restarted out-of-scope work.
- If your re-sent goal contradicts main's latest message or Ada's rulings here, follow the message and the rulings.
  Tell main in your reply, so main closes the stale goal in your stack.

## REACHING MAIN (2026-11-21; main changed again ~20:55 PST)
- Main is now the kiln agent 01a201824d307ecc9e135c64ae3cb848 ("Slates", on adas-macbook-pro-2, cwd ~/Projects/slates).
  Main 01a1ff2ecd2f7d34964c9dff5fa80b2c stopped at 15:10 PST on an API usage limit and is gone from kiln; Ada handed
  the fleet to 01a20182 at ~20:30 PST. The 93-goal UI stack is restored onto 01a20182 as 01a1ff2e left it (222 met,
  20 open). Send nothing more to 01a1ff2e, 01a1ff31, 01a14885 or `slates-5b`. Reach main with kiln `agents_send`
  (agent 01a201824d307ecc9e135c64ae3cb848) or the inbox below. `slates-*` sessions are NOT main.
- The route that always works: write `inbox/main-from-<you>-<topic>.md` on agents/fanout-state and push it. Main reads
  every new inbox file each check. kiln mail to main can sit "waiting" behind a stuck admission, so never rely on it alone.
- Main reaches you the same three ways. When kiln mail from main is stuck, it arrives as a goal or a session message.
- A-NUMBERS (2026-11-21): claim an A-number by adding its row to amendments.tsv and pushing that claim BEFORE writing the
  number in a commit, doc or test. If the push is refused, fetch, and if the number was taken meanwhile, take the next
  free one. Three collisions in one morning (A-815, A-821, A-822) cost renumbering commits.
