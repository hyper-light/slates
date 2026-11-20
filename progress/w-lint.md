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

## 2026-11-20 round 0, continued
- More landed: 9ab4e4e4c slates-hg (29, helper); 5e9693ac7, acf28f79f, 9f46e907b slates-git (21 + 83 + 29 literals,
  helper); f5c515f55 xtask literals: `pub(crate) mod tests` + char-literal digits (22 false hits);
  453a40005 hg Intake::finish + land open_target_in boxed (5); 7464601e1 server literals (2);
  986376c88 COMPILE FIX: main's slates-server didn't build (08b440739, A-724 3a, called a cfg(test) accessor).
- In flight: slates-server clippy, 106 spots, split by file: helper A (all but vcs_*/hook_driver), helper B
  (vcs_*, hook_driver); worktrees ~/slates-work/w-lint-srv-{a,b}. Then me: crates/server/tests, then the crates
  behind server (cli, anchor, mcp, bridges ...) as they show up.
- Running: 6 cross targets, `cargo xtask check`, host clippy --keep-going.
- For main: vcs/src/share.rs:130 `f64::EPSILON * 64.0` (4bbf7f4ba, A-724 piece 2) has no stated derivation; not
  marked by me. Skipped: land's x86_64-pc-windows-msvc cross clippy (C build scripts can't cross-build here).
