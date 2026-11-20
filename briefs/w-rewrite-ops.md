# w-rewrite-ops: a whole-file rewrite becomes the precise edit it was (D-O17) (main, 2026-11-20)

## Why
Ada, 2026-11-20 ~13:20 PST, after a Slates-vs-Sapling comparison of the two checkouts (Slates 896233d1f, Sapling d49b955d3ae): "dispatch agents to tackle these areas and gaps for slates." The comparison's central finding: "Slates' fast copy-on-write volumes and its precise merge engine are individually real, but the ordinary mounted-editor workflow does not yet connect the two end to end." Sapling's checkout is at ~/Projects/sapling (read it, never write it); EdenFS's docs are under eden/fs/docs.

## Your work
- **The tradeoff the comparison names (GAPS D-O17, ~314):** a tool that rewrites an entire file, as editors do on save
  (write a temp file and rename) and as formatters and codemods do, turns a one-token change into a broad conflict.
- **Resolve D-O17:** when a write (or a rename-over) replaces a file, derive the minimal byte-range operations against
  the Work's base, bounded and sliced under the quantum, with no allocation the design can avoid. The verdict then
  sees the edit the tool made, not a whole-file replacement. D-27 still holds: no inferred merge. This recovers the
  writer's own edit, never a three-way text merge.
- **A/B that decides the shape, a measured example:**
  - diff algorithm (Myers linear-space vs histogram vs patience) and granularity (bytes vs lines vs tokens);
  - on realistic concurrent edits: two agents changing different lines, different tokens on one line, a formatter
    pass beside an edit;
  - measure the conflict rate, the cost per save (µs per KB, p99) and the memory.
- Read D-26/D-27, the verdict (crates/merge/src/verdict.rs) and §4.16 first. Work with w-work-mount, the main consumer.

## Rules
Follow briefs/common.md in full: clone the state branch to ~/fanout-state-<you> with `git clone --branch agents/fanout-state git@github.com:hyper-light/slates.git` (HTTPS if SSH fails). The sections that bind this work most: CLAUDE.md §8 BINDS (focused unit and integration tests only; heavy and cross-platform runs go in run-requests.md), MEMORY SIZING (size from the work, no allocation the design can avoid), STORAGE and the one-unit changeset ruling, PERFORMANCE GAPS UP FRONT, and DECISIONS BACKED BY DATA (a design A/B as a measured example). Red first for every fix. Land each piece on slates origin/main (merge, read the diff stat, push), confirm with merge-base, update the design section, its status line and GAPS in the same change, then stop and report: landed (commit), numbers (machine, release), next. Never force; never checkout, switch, stash, reset or commit-tree. No installs or system changes. Keep progress/<you>.md current and pushed.
