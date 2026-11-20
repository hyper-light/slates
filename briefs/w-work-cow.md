# w-work-cow: a Work over a Green costs its divergence, not the Green (main, 2026-11-20)

## Why
Ada, 2026-11-20 ~13:20 PST, after a Slates-vs-Sapling comparison of the two checkouts (Slates 896233d1f, Sapling d49b955d3ae): "dispatch agents to tackle these areas and gaps for slates." The comparison's central finding: "Slates' fast copy-on-write volumes and its precise merge engine are individually real, but the ordinary mounted-editor workflow does not yet connect the two end to end." Sapling's checkout is at ~/Projects/sapling (read it, never write it); EdenFS's docs are under eden/fs/docs.

## Your work
- `create_work` (crates/server/src/verbs.rs ~4927) walks the Green's files and copies every path and byte vector into
  a private map, so ten Works over one populated Green copy its contents ten times before anyone edits.
- Make a Work refer to the Green's immutable base, copy-on-write as an ordinary volume is (EdenFS-style laziness: pay
  for divergence only), with the base pinned for the Work's life and released at its end.
- **The measurement that decides it**, as a measured example, release: time and additional resident bytes from
  "request Work" through "read one existing file and make one edit", repeated as the base grows (1, 10, 100 k files;
  1 MB to 1 GB) and as agents join (1, 10, 100 Works over one Green). Before and after, all N shown. Red:
  creating N Works over a G-byte Green adds O(N x metadata) bytes, never N x G.
- Read §4.16 (works, greens), D-25 (bases) and GAP-A9-14 first. Coordinate with w-work-mount (mounting a Work) by inbox
  note; it builds on your Work.

## Rules
Follow briefs/common.md in full: clone the state branch to ~/fanout-state-<you> with `git clone --branch agents/fanout-state git@github.com:hyper-light/slates.git` (HTTPS if SSH fails). The sections that bind this work most: CLAUDE.md §8 BINDS (focused unit and integration tests only; heavy and cross-platform runs go in run-requests.md), MEMORY SIZING (size from the work, no allocation the design can avoid), STORAGE and the one-unit changeset ruling, PERFORMANCE GAPS UP FRONT, and DECISIONS BACKED BY DATA (a design A/B as a measured example). Red first for every fix. Land each piece on slates origin/main (merge, read the diff stat, push), confirm with merge-base, update the design section, its status line and GAPS in the same change, then stop and report: landed (commit), numbers (machine, release), next. Never force; never checkout, switch, stash, reset or commit-tree. No installs or system changes. Keep progress/<you>.md current and pushed.
