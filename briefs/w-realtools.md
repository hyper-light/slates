# w-realtools: real tools define filesystem correctness across snapshots, eviction and takeover (main, 2026-11-20)

## Why
Ada, 2026-11-20 ~13:20 PST, after a Slates-vs-Sapling comparison of the two checkouts (Slates 896233d1f, Sapling d49b955d3ae): "dispatch agents to tackle these areas and gaps for slates." The comparison's central finding: "Slates' fast copy-on-write volumes and its precise merge engine are individually real, but the ordinary mounted-editor workflow does not yet connect the two end to end." Sapling's checkout is at ~/Projects/sapling (read it, never write it); EdenFS's docs are under eden/fs/docs.

## Your work
- **The comparison's third lesson, from EdenFS's own records** (~/Projects/sapling/eden/fs/docs/InodeStorage.md):
  - unstable inode numbers broke C++ `#pragma once`;
  - changing an untouched file's timestamp makes `make` rebuild it.
  A test that reads the right bytes after a restart can pass while a real incremental build does the wrong thing.
- **Acceptance tests by use, with real tools, across** a snapshot change, cache eviction or memory pressure, and a
  daemon restart and takeover:
  - (1) a C++ build using `#pragma once` (clang and gcc where present) compiles each header once across those
    events: inode numbers stable;
  - (2) `make` after those events rebuilds nothing untouched: mtimes and ctimes stable;
  - (3) `cargo build` incremental is a no-op;
  - (4) git status over the mount shows no spurious changes.
- **Red first:** each fails today if the property breaks; fix what fails at its root, per platform
  (macOS NFS, Linux FUSE, Windows WinFsp).
- Put the cross-platform runs in run-requests.md (§8); the macOS focused runs are yours.

## Rules
Follow briefs/common.md in full: clone the state branch to ~/fanout-state-<you> with `git clone --branch agents/fanout-state git@github.com:hyper-light/slates.git` (HTTPS if SSH fails). The sections that bind this work most: CLAUDE.md §8 BINDS (focused unit and integration tests only; heavy and cross-platform runs go in run-requests.md), MEMORY SIZING (size from the work, no allocation the design can avoid), STORAGE and the one-unit changeset ruling, PERFORMANCE GAPS UP FRONT, and DECISIONS BACKED BY DATA (a design A/B as a measured example). Red first for every fix. Land each piece on slates origin/main (merge, read the diff stat, push), confirm with merge-base, update the design section, its status line and GAPS in the same change, then stop and report: landed (commit), numbers (machine, release), next. Never force; never checkout, switch, stash, reset or commit-tree. No installs or system changes. Keep progress/<you>.md current and pushed.
