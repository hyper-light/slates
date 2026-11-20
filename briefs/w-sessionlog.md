# w-sessionlog: a Smartlog-sized view of an agent session (main, 2026-11-20)

## Why
Ada, 2026-11-20 ~13:20 PST, after a Slates-vs-Sapling comparison of the two checkouts (Slates 896233d1f, Sapling d49b955d3ae): "dispatch agents to tackle these areas and gaps for slates." The comparison's central finding: "Slates' fast copy-on-write volumes and its precise merge engine are individually real, but the ordinary mounted-editor workflow does not yet connect the two end to end." Sapling's checkout is at ~/Projects/sapling (read it, never write it); EdenFS's docs are under eden/fs/docs.

## Your work
- **The comparison's second lesson:** someone supervising ten agents should see the one decision that needs them
  without cross-referencing volume ids, version output, conflict JSON and the landing audit.
- **Build `slates log`** (CLI first, then the same read-only view over MCP and the SDKs). It shows:
  - which Green version each Work started from;
  - what advanced since;
  - which submissions were accepted, identical or collided, and where;
  - which manifest awaits a grant.
  Like Sapling's Smartlog, it hides what is irrelevant and shows only the relationships that decide the next action.
  Read ~/Projects/sapling/website/docs/overview/smartlog.md.
- No grant verb anywhere outside the CLI and the confirmation surface (R10): the view only shows a pending grant.
- **Red first, by use:**
  - Ten Works over one Green, with one collision and one pending landing: the view names exactly those two as needing
    the human, and nothing else.
  - The output is stable and machine-readable (`--json`) as well as human.
- Size its reads from what it shows (Ada's rule): no full-history walk to draw the latest state.

## Rules
Follow briefs/common.md in full: clone the state branch to ~/fanout-state-<you> with `git clone --branch agents/fanout-state git@github.com:hyper-light/slates.git` (HTTPS if SSH fails). The sections that bind this work most: CLAUDE.md §8 BINDS (focused unit and integration tests only; heavy and cross-platform runs go in run-requests.md), MEMORY SIZING (size from the work, no allocation the design can avoid), STORAGE and the one-unit changeset ruling, PERFORMANCE GAPS UP FRONT, and DECISIONS BACKED BY DATA (a design A/B as a measured example). Red first for every fix. Land each piece on slates origin/main (merge, read the diff stat, push), confirm with merge-base, update the design section, its status line and GAPS in the same change, then stop and report: landed (commit), numbers (machine, release), next. Never force; never checkout, switch, stash, reset or commit-tree. No installs or system changes. Keep progress/<you>.md current and pushed.
