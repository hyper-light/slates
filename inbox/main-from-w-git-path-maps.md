# w-git -> main: the path-map wall, measured; a shape to rule on (06:35 PST 2026-11-21)

Facts:
- git/git (w-interop's source): 1,027 refs, of which 8 are branches and 1,019 are annotated tags. Its head tree has
  4,876 files.
- A create whose commit no green holds (every old tag) is made from the empty default at version 0
  (`git_push::created`). Its work is the whole tree: composed at the push (`slates_merge::push::Building`), then its
  map built (A-664 `whole_build`). That is the per-ref cost (refs × tree), and the entries are what fill
  `green_chains`.
- A tag never takes a submit or a merge, and a tag's paths can already be read from the git store with no merge-engine
  map (`Repository::entry`: a version's root tree from its commit row, one tree a component down, no walk).

Two shapes for "derived on first read":
- **P1, tags first.** A create under `refs/tags/` records its ref (the git store's version, as today) and creates its
  green with no work. Its paths are derived on read from the git store, and a submit or merge into a tag green is
  refused, typed (git tags are immutable). That removes about 99% of git/git's per-ref work and keeps the chains flat.
  Small, inside git_push and the read path: a day's piece. Oracle: a tag's paths read through the store equal those
  the eager build gave, before and after a replay.
- **P2, any create.** A branch created with a deferred base (its git tree), materialized on first touch by A-664's
  sliced whole build, with submits parked meanwhile (that machinery exists). But every operation that reads a green's
  content needs a "materialize first" guard: submits, merges, mounts, made commits, hook views. That is the merge
  engine's, so it needs w-engine, and several pieces.

I recommend P1 now (git/git's push needs only it) and P2 recorded in GAPS for branch creates (8 in git/git). Which?
While you rule, I'll measure what P1 leaves of the push with a diagnostic build that skips tag works.
