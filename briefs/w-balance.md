# w-balance: one server, many repositories, balanced by learned usage (main, 2026-11-20 ~02:00 PST)

## Ada's words (2026-11-20)
- "a single server should be able to facilitate numerous repositories and Slates' server side should be intelligent
  enough to balance consumption and usage between them based upon agent, usage patterns. ... Consider that we must
  facilitate everything from small toy repos to repositories as large as Google's or Meta's *in multiple*."
- "an individual slates server must be able to handle tens of thousands of concurrent large pushes"
- "we aren't just a git/mercurial/svn server too, we use our merge mechanisms and default concurrent scale to
  facilitate significantly higher scaled throughput paired with significantly lower resource usage."

## Goal
Design and implement how one slates server, and the fleet behind it, divides memory, CPU, admission and placement among
many repositories of very different sizes and activity. The division is learned from what agents and clients actually
do, never fixed shares or constants.

## Read first
- CLAUDE.md: R1-R10 and the §2 bans.
- docs/wip/SLATES_DESIGN.md:
  - §4.17 and §4.17.1 (D-28): a push is a work submitted to a green; one copy of every byte, by identity.
  - §4.16: the merge engine.
  - §4.11: compression, dedup and the cost model.
  - §4.10: replication and placement.
  - §4.8: partitions. Ids route to owners; there is no global catalog.
  - D-11: derived tunables.
- Amendments:
  - A-704, A-711 and A-713: admission by shard capacity.
  - A-714: a repository's content charged to its owner shard.
  - A-636: family placement.
  - A-575: lineage.
- Tunables are learned at runtime per deployment, never one machine's constants.

## Where it stands (measured 2026-11-20 by w-perf, release main 7b46fbdcd, Mac Studio)
- One push of a 3.4 MB git pack holds about 229 MiB per repository, and none of it is ever released:
  - 69.1 MiB: objects stored inflated in `slates_vcs::ContentStore`.
  - 63.5 MiB: the register entry's copy of the same objects.
  - 63.7 MiB: arena growth for the first copy.
- 100 concurrent pushes into 100 repositories held 28 GB.
- w-perf is writing the plan that brings those copies down to one per byte, by identity. Its owners are the merge
  engine, db, git, hg and svn. Coordinate with it through main. You own the balancing, not the copies.

## Your scope
1. DESIGN FIRST. Write an amendment, in §4.17.x or §4.8/§4.10 as fits, with tiered evidence. It covers:
   - What a repository costs. It is hot (recent heads, active works, live pushes), warm (recent history) or cold (old
     history and indexes that can be rebuilt). The levers:
     - §4.11 compression of cold content;
     - dropping derived indexes and rebuilding them on demand;
     - spreading a repository's partitions across shards and nodes;
     - admission.
   - Learned demand per repository, and per agent or client: push and fetch rates, working-set sizes, object-size
     distributions, concurrency and time patterns. Shares derived by stated formulas over measured inputs (D-11), and
     logged with their inputs.
   - Fairness and isolation:
     - a huge or hot repository never starves the others;
     - small repositories stay on the fast path (R9);
     - every bound is derived and refuses with a typed refusal;
     - backpressure, never OOM.
   - Scale, from toy repositories to Google- or Meta-size monorepos, several at once. One node's RAM can't hold
     several of those. Say how a giant repository spreads across shards and nodes (partitions, family placement, f+1
     replication) and what one node keeps hot. RAM only (R1): there is no disk tier, so cold means compressed, on
     peers, or both.
   - No global catalog (§2 ban 12). No locks on data paths. Laptop ≡ fleet (R8): the same mechanism at N=1.
   Put a one-page outline in your kiln summary BEFORE code, and wait for main's go.
2. IMPLEMENT PIECEWISE, under common.md's fast landing loop (implement, focused tests, merge). Likely pieces:
   - per-repository usage accounting (counters);
   - the learned share model, with a model or oracle test;
   - admission by share;
   - the trigger that compresses cold content;
   - placement that spreads a repository.
3. PROVE BY USE. Extend w-perf's push_load harness (crates/cli/bench/push_load.py, branch agents/w-perf-pushload)
   to many repositories of mixed sizes and skewed activity. Show balanced latency and memory, and no starvation. Ask
   main before any heavy load on a shared Studio.

## Rules
- Follow ~/fanout-state-w-balance/briefs/common.md in full:
  - the fast landing loop;
  - claim an A-number by pushing the ledger before you write it anywhere;
  - keep your own state folder;
  - the Studio push routes;
  - every piece ends on origin/main, confirmed with merge-base.
- Follow CLAUDE.md.
- Never force. Never checkout, switch, stash, reset or commit-tree.
- Never print, log or copy any API key or credential.
- Report through your kiln summary, since main can't take SendMessage across machines. Keep a progress file at
  ~/fanout-state-w-balance/progress/w-balance.md and push it to agents/fanout-state after each step.
