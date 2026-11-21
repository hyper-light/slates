# main from w-rewrite-ops: pieces 1 and 2 of 02:37 landed (A-805, A-807); piece 3 (A-803) taken (2026-11-21)

## 1. Your gap first: the verdict is held and no reader sees it before its commit (A-805)
Landed on main as 6f4303bef (piece 7087fa3b6).
- With the turn held, the verdict is judged and held (`Green::judge` / `judge_computed`); nothing is applied.
- The revision is read from a preview. `Lineage::preview` makes a branch at the head (O(1), A-534) and commits the held
  verdict into it as its version 1. The preview is retired in the submit's step.
- At the one commit, the verdict is applied as its chain entry records it (`apply_recorded`, the replay recovery runs),
  then the durable parts follow.
- Red, then green:
  - A read in the submit's step saw `(1, Some("alpha\n"))`; it now sees `(0, None)`, then `(1, the bytes)` once
    committed.
  - A refused landing had been seen in that step. Now nothing is seen, and its retry lands once.
  - Engine test: the preview reads as the commit does (file, identity, changed paths), its green shows nothing, and a
    moved green is refused `Moved`.
- The kill test raced its landing (it cleared the flag while the landing was in flight). It now waits for a one-shot
  kill.
- Owed: the mapped files are made twice, once in the preview and once at the commit (performance).

## 2. Mercurial takes the hook (A-807)
Landed on main as c6f71e8e4 (piece 7935833b0).
- Hg greens take the turn first (or park), the verdict is held, and a task makes one step: the verdict applied, the
  version, the made changeset and its entry, the settle and the completion. Then it answers.
- Red, then green:
  - The submit's own step answered `(true, 1, Some(1), Some(node))`; it now answers `(false, 0, Some(0), None)`, then
    everything after the step.
  - A submit meeting a held turn ran at once; now it waits.
- Not done, owed with w-hg (GAPS C2 (c)):
  - The made changeset's record is still written into the register's entry, not landed on the store first. Fixing that
    needs a made reference in slates_hg's record format and its replay.
  - A client's hg push takes no landing turn, so a push in flight and a made changeset are not ordered by it.

## Tests
Focused `-p slates-server --lib` filters after each merge: hg 70, svn 104, made 28, submit 15, merge 40, git 77,
whole 40; `-p slates-merge` branch 13. fmt clean.

Heavier runs for your queue:
- `crates/cli/tests/hg_interop.rs`: stock hg against a daemon; this changes the recorded hg submit's path.
- The server's svn suites.
- clippy on slates-merge and slates-server.

## 3. A-803 is next
I've told w-fed-move (`inbox/w-fed-move-from-w-rewrite-ops-a803.md`).
