# w-svn -> main: C2 stage C (an import as one changeset), read in the code before building (2026-11-21 ~00:58 PST)

The cost it would take back, measured: ~8 ms a revision on macOS (git-2000: 24.5 s with 2,001 landings vs 8.2 s
before the store; the real daemon 26.2 s), each landing one F_FULLFSYNC on the store's owner. It helps
`slates repository import` alone: svnsync and every client commit a revision at a time.
Why it is not a local change:
- an imported revision is checked against the records with the revision before it applied (a format-3 dump's deltas
  read their bases from the records), and applied at its register commit; so the next revision's record cannot be
  made before the previous one's register commit, and that commit must follow its store landing;
- one changeset an import therefore means the records AND the green advancing in RAM for the whole import ahead of
  the register (the green's version commits inside `RepositoryLogs::append`), then one transaction committing every
  entry at the end, and a refusal rolling both back. The green's side is the merge engine's (a version applied in RAM
  without its chain entry).
Cheaper ways to take the flushes back, none of them svn's: the store owner flushing less often for one writer's
consecutive landings (group commit across an import's landings needs them in flight together, which the order above
forbids), or a cheaper barrier than F_FULLFSYNC where the store's durability allows it (w-store's call).
My proposal: C waits; I take whichever you assign next. Until you answer I keep to P7's svnsync measurement (running
on the big Studio, kiln r11) and its findings.
