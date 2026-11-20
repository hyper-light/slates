# w-lint progress

## 2026-11-19 round 0
- Machine: Adas-Mac-Studio-2.local (kiln runner b24c5c95; 14 cores, 36 GB, 460 GB disk), 14-17 GiB free: under the
  20 GB guard. None of the space in use is w-lint's.
- So: a blobless clone at ~/slates-work/w-lint (210 MB) here; clippy, cross lints and xtask go through a kiln run with
  needs disk=60g (r1, at origin/main 7f81be683), CARGO_TARGET_DIR=~/.cache/w-lint-target on whichever machine it lands on.
- Commit signing (kiln-git-sign) refuses this session: "only a process of an agent session kiln runs here signs".
  Ada OK'd this one progress commit unsigned; landings on main still need signing fixed or Ada's OK.

## 2026-11-20 round 0, landings so far (commits unsigned: Ada's OK, kiln-git-sign refuses this session)
- Disk recovered to 66-119 GiB free, so builds run here: clone ~/slates-work/w-lint, target ~/.cache/w-lint-target;
  helpers have worktrees ~/slates-work/w-lint-{svn,git,merge,hg} with their own targets (delete when done).
- Landed on origin/main (each confirmed with merge-base --is-ancestor):
  - 7c91c0a32 slates-vcs (8); 91d85df6e slates-ipc + slates-rt cfg(test) split (9, cross targets);
    2b0ea20bc slates-db (13 + 3 literals); d1ccb1ad2 xtask literals follows #[path] test modules (125 false hits in hg);
    7de1f32b1 + 75eb6527a slates-http-edge (6); 7e5eab88e slates-vfs (3).
  - helpers: 058481ce8 slates-merge (46); 5828c1457 slates-ssh (9); 64300b31e, 7975f5cd7, dc3044069 slates-svn
    (17 + 22 + 3, 89 literals).
- In flight: slates-git helper, slates-hg helper. Left for me: slates-server (24 literals + clippy once deps clear),
  vcs/src/share.rs literal, then the full wall again (host, 6 cross targets, xtask check incl. unsafe, fmt).
- For main: the literal check reads a byte char like b'3' as the number 3 (doesn't strip char literals).
