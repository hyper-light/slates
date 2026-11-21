# w-realtools -> main: goal 93, Windows by use on jupiter (git and hg against --vcs-store)

## Round 0 (2026-11-21 ~11:05 PST), origin/main 67fa7f75a. Nothing has run yet.
- BLOCKED, needs Ada: no stock Mercurial on jupiter. It isn't on PATH, in Program Files (Mercurial or TortoiseHg), or
  importable from python 3.15. Installing it is a system change, so the hg half waits for Ada, as svn does. git is
  there: Git for Windows 2.56.0.
- WAITING: w-work-mount's Windows StoreRoot (5db34aab0) isn't on origin/main yet. I asked it when, in
  inbox/w-work-mount-from-w-realtools.md.
- DISK RISK on jupiter: C: has 18 GB free (99% used). w-work-mount's target there is 35 GB. My release build needs its
  own target, several GB. I'll check free space before building and stop if it would drop below ~10 GB. Say so if you
  want it built elsewhere first.
- Meanwhile: writing the git harness (git/git and a Windows-paths repository: case, reserved names, CRLF attributes,
  long paths; clone, push, pull, tags, fsck, byte-compare). It's modeled on crates/cli/bench/hg_push_profile.sh:
  release `slates anchor --vcs-http 127.0.0.1:0 --vcs-store DIR`, a token from `slates credential token`, and stock
  git with a bearer header.

## Round 1 (2026-11-21 ~11:45 PST): the test landed; the jupiter round is yours (§8)
- LANDED c66f7160 (merge 8f44f7e2): crates/cli/tests/git_interop.rs `stock_git_carries_windows_paths_byte_identically`,
  #[ignore]d next to the existing git/git session (stock_git_carries_a_whole_repository_over_each_edge).
- Focused proof on macOS: Adas-Mac-Studio-2, Apple Git 2.50.1, debug, base e0079c7: 1 passed in 14.87 s, fmt clean.
- run-requests.md (8f44f7e2 entry): the jupiter round, both tests, release build, after 5db34aab0 lands and C: has room.
- Stopped per your 10:48 correction: r11 (never started) and r12 (git/git download on jupiter; its partial folder is
  deleted). I'm stopping now. hg waits on Ada.
