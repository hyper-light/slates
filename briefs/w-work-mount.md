# w-work-mount: ten ordinary editors, their journals submitted, the Green granted to disk (main, 2026-11-20)

## Why
Ada, 2026-11-20 ~13:20 PST, after a Slates-vs-Sapling comparison of the two checkouts (Slates 896233d1f, Sapling d49b955d3ae): "dispatch agents to tackle these areas and gaps for slates." The comparison's central finding: "Slates' fast copy-on-write volumes and its precise merge engine are individually real, but the ordinary mounted-editor workflow does not yet connect the two end to end." Sapling's checkout is at ~/Projects/sapling (read it, never write it); EdenFS's docs are under eden/fs/docs.

## Your work
- **The objective, verbatim from the comparison:** "Run ten ordinary editors, submit their journals, and grant the
  resulting Green to disk", end to end, by use.
- **Today:**
  - GAP-A9-14 leaves mounted Work/Green integration open.
  - crates/server/src/landing.rs (~221) says a merge volume has no store-backed snapshot to land.
  - Only an SDK agent declaring precise edits can use the merge service.
- **Build, red first, each piece landed:**
  - (1) A Work mounts like any volume (FUSE, NFS, WinFsp, per platform), and real tools' writes through the mount are
    recorded as the Work's journal (its changeset).
  - (2) Submitting a mounted Work's journal to the Green gets the verdict.
  - (3) An accepted Green version has a store-backed snapshot the landing engine plans and lands under a human grant
    (R10; the grant only through the CLI or a confirmation surface).
  - (4) The by-use test: ten real editor processes on ten mounted Works over one Green, submissions, verdicts
    (accepted, identical, conflict), then a grant lands the result. The landed bytes must equal the oracle.
- Real editors save by rewriting whole files (temp file and rename), so coordinate with w-rewrite-ops, which turns a
  rewrite into precise operations. Build on w-work-cow's copy-on-write Work.

## Rules
Follow briefs/common.md in full: clone the state branch to ~/fanout-state-<you> with `git clone --branch agents/fanout-state git@github.com:hyper-light/slates.git` (HTTPS if SSH fails). The sections that bind this work most: CLAUDE.md §8 BINDS (focused unit and integration tests only; heavy and cross-platform runs go in run-requests.md), MEMORY SIZING (size from the work, no allocation the design can avoid), STORAGE and the one-unit changeset ruling, PERFORMANCE GAPS UP FRONT, and DECISIONS BACKED BY DATA (a design A/B as a measured example). Red first for every fix. Land each piece on slates origin/main (merge, read the diff stat, push), confirm with merge-base, update the design section, its status line and GAPS in the same change, then stop and report: landed (commit), numbers (machine, release), next. Never force; never checkout, switch, stash, reset or commit-tree. No installs or system changes. Keep progress/<you>.md current and pushed.
