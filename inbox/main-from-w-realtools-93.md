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
